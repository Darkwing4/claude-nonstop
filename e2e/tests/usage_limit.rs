use std::time::Duration;

use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::scenario::{Scenario, TURN, unix_now};

#[test]
fn d1_five_hour_limit_resumes_once_after_reset_tmux() {
    let reset_at = whole_minute_after(Duration::from_secs(60));
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::FiveHour, reset_at)])
        .start();
    let pane = scenario.launch_claude();
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");

    scenario.submit("say ok");

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    let banner = failure.payload["last_assistant_message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(failure.payload["error"], "rate_limit");
    assert!(
        banner.starts_with("You've hit your session limit · resets"),
        "{banner}"
    );

    let session_id = scenario.session_id();
    let limit =
        scenario.wait_for_ledger_session(&session_id, "the limit in the ledger", TURN, |record| {
            record["status"] == "stopped"
        });
    scenario.wait_for_notification("claude-nonstop: limit", TURN);
    assert_eq!(limit["stop"], "usage_limit");
    assert_eq!(limit["pane"]["pane_id"], pane.id);
    assert_eq!(
        (
            limit["limit"].as_str(),
            limit["reset_source"].as_str(),
            limit["reset_at"].as_u64()
        ),
        (Some("five_hour"), Some("absolute"), Some(reset_at)),
        "PRODUCT BUG (found by e2e): the StopFailure hook reads the reset time only from the transcript, \
         which Claude Code has not flushed yet when the hook runs, so {banner:?} is recorded without its reset time"
    );

    let until_reset = Duration::from_secs(reset_at.saturating_sub(unix_now()));
    let resumed = scenario.wait_for_ledger_session(
        &session_id,
        "the session resumed after the reset",
        until_reset + TURN,
        |record| record["status"] == "resumed",
    );
    scenario.wait_for_hook("Stop", TURN);

    let continuations: Vec<Option<String>> = scenario
        .mock
        .main_loop_requests()
        .iter()
        .skip(1)
        .map(|request| request.last_user_text())
        .collect();
    assert_eq!(
        continuations,
        [Some(resume_message)],
        "KNOWN BUG 3: after a five-hour reset the session must continue exactly once, with claude-nonstop's resume message; \
         Claude Code's own auto-continue races the resumer (ledger record: {resumed})"
    );
    assert_eq!(resumed["resumed_by"], "nonstop");
}

fn whole_minute_after(delay: Duration) -> u64 {
    (unix_now() + delay.as_secs()).div_ceil(60) * 60
}
