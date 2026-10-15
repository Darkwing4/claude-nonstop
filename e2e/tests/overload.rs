use std::time::Duration;

use claude_nonstop_e2e::mock_api::Reply;
use claude_nonstop_e2e::scenario::{Scenario, TURN, unix_now};
use claude_nonstop_e2e::terminal::{Multiplexer, Pane};

const PROMPT: &str = "say ok";
const FIRST_BACKOFF_SECONDS: u64 = 1;
const OVERLOAD_CAP: Duration = Duration::from_secs(60);
const HOOK_VERDICTS: &[&str] = &["hook: ", "overloaded (", "turn cut off (", "usage limit ("];

#[test]
fn c1_overloaded_api_is_waited_out_tmux() {
    let mut scenario = Scenario::in_tmux()
        .replies(failed_turn(Reply::overloaded()))
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    let message = expect_failure(&scenario, 1, "server_error", "529 Overloaded");
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        waits_out_overload(&verdict, "server_error", FIRST_BACKOFF_SECONDS),
        "KNOWN BUG 1: Claude Code reports a 529 as server_error ({message:?}); the hook must wait it out \
         as an overload instead of continuing it as a cut-off turn: {verdict}"
    );

    let overload_message = expect_resumed_after_overload(&scenario, &pane);
    assert_eq!(
        prompts(&scenario),
        [PROMPT, PROMPT, overload_message.as_str()]
    );
}

#[test]
fn c2_overload_that_never_ends_is_given_up_tmux() {
    let mut scenario = Scenario::in_tmux()
        .env("CLAUDE_NONSTOP_OVERLOAD_MAX_TOTAL_MINUTES", "1")
        .env("CLAUDE_NONSTOP_OVERLOAD_STEADY_SECONDS", "1")
        .fallback_reply(Reply::overloaded())
        .start();
    scenario.launch_claude();
    let session_id = scenario.session_id();
    let overload_message = scenario.nonstop_setting("agents.claude.overload_message");

    scenario.submit(PROMPT);

    let message = expect_failure(&scenario, 1, "server_error", "529 Overloaded");
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        waits_out_overload(&verdict, "server_error", FIRST_BACKOFF_SECONDS),
        "KNOWN BUG 1: Claude Code reports a 529 as server_error ({message:?}); the hook must wait it out \
         as an overload instead of continuing it as a cut-off turn: {verdict}"
    );

    let failed = scenario.wait_for_ledger_session(
        &session_id,
        "the overload given up",
        OVERLOAD_CAP + TURN,
        |record| record["status"] == "failed",
    );
    let notification = scenario.wait_for_notification("claude-nonstop: gave up", TURN);
    scenario.wait_for_log("waited the full 1 min, giving up", TURN);
    assert_eq!(failed["stop"], "overload", "{failed}");
    assert!(notification.urgent, "{notification:?}");
    assert!(
        notification.body.contains("overloaded for 1 min"),
        "{notification:?}"
    );
    assert!(
        prompts(&scenario).contains(&overload_message),
        "the ladder retried before giving up"
    );
}

#[test]
fn c3_capacity_limit_is_waited_out_not_recorded_as_usage_limit_tmux() {
    capacity_limit_is_waited_out_not_recorded_as_usage_limit(Multiplexer::Tmux);
}

#[test]
fn c3_capacity_limit_is_waited_out_not_recorded_as_usage_limit_zellij() {
    capacity_limit_is_waited_out_not_recorded_as_usage_limit(Multiplexer::Zellij);
}

fn capacity_limit_is_waited_out_not_recorded_as_usage_limit(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer)
        .subscription()
        .replies(failed_turn(request_rate_limited()))
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    expect_failure(&scenario, 1, "rate_limit", "(not your usage limit)");
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        waits_out_overload(&verdict, "rate_limit", FIRST_BACKOFF_SECONDS),
        "{verdict}"
    );

    let overload_message = expect_resumed_after_overload(&scenario, &pane);
    assert_eq!(
        prompts(&scenario),
        [PROMPT, PROMPT, overload_message.as_str()]
    );
    assert!(
        !scenario.sandbox.nonstop_log().contains("usage limit ("),
        "no usage limit is recorded"
    );
    assert!(
        scenario
            .sandbox
            .notifications()
            .iter()
            .all(|notification| notification.title != "claude-nonstop: limit"),
        "nobody is told about a usage limit"
    );
}

#[test]
fn c4_refused_connection_is_waited_out_until_the_api_is_back_tmux() {
    refused_connection_is_waited_out_until_the_api_is_back(Multiplexer::Tmux);
}

#[test]
fn c4_refused_connection_is_waited_out_until_the_api_is_back_zellij() {
    refused_connection_is_waited_out_until_the_api_is_back(Multiplexer::Zellij);
}

fn refused_connection_is_waited_out_until_the_api_is_back(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer).start();
    let pane = scenario.launch_claude();
    scenario.mock.refuse_connections();

    scenario.submit(PROMPT);

    expect_failure(&scenario, 1, "server_error", "(ECONNREFUSED)");
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        waits_out_overload(&verdict, "server_error", FIRST_BACKOFF_SECONDS),
        "{verdict}"
    );

    scenario.mock.accept_connections();

    let overload_message = expect_resumed_after_overload(&scenario, &pane);
    assert_eq!(
        prompts(&scenario),
        [overload_message.as_str()],
        "refused attempts never reach the API"
    );
}

#[test]
fn c4_dropped_connection_is_waited_out_tmux() {
    let mut scenario = Scenario::in_tmux()
        .replies(failed_turn(Reply::ResetConnection))
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    expect_failure(&scenario, 1, "server_error", "(ECONNRESET)");
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        waits_out_overload(&verdict, "server_error", FIRST_BACKOFF_SECONDS),
        "{verdict}"
    );

    let overload_message = expect_resumed_after_overload(&scenario, &pane);
    assert_eq!(
        prompts(&scenario),
        [PROMPT, PROMPT, overload_message.as_str()]
    );
}

#[test]
fn c5_overload_during_a_usage_limit_is_left_to_the_limit_tmux() {
    let again = "say ok again";
    let mut scenario = Scenario::in_tmux()
        .replies(failed_turn(request_rate_limited()))
        .replies(failed_turn(Reply::ResetConnection))
        .start();
    scenario.launch_claude();
    let session_id = scenario.session_id();

    scenario.submit(PROMPT);

    expect_failure(&scenario, 1, "rate_limit", "Request rejected (429)");
    let limit_verdict = hook_verdict(&scenario, 1);
    let limit = scenario
        .sandbox
        .ledger_session(&session_id)
        .expect("the usage limit is in the ledger");
    assert!(limit_verdict.contains("usage limit ("), "{limit_verdict}");
    assert_eq!(
        (limit["stop"].as_str(), limit["status"].as_str()),
        (Some("usage_limit"), Some("stopped")),
        "{limit}"
    );

    scenario.submit(again);

    expect_failure(&scenario, 2, "server_error", "(ECONNRESET)");
    let verdict = hook_verdict(&scenario, 2);
    assert!(
        verdict.contains("overloaded (server_error) while a usage limit waits until")
            && verdict.ends_with("left to the limit"),
        "{verdict}"
    );
    assert_eq!(
        scenario.sandbox.ledger_session(&session_id),
        Some(limit),
        "the open usage limit stays as it was"
    );
    assert_eq!(prompts(&scenario), [PROMPT, PROMPT, again, again]);
}

#[test]
fn c6_pane_closed_during_an_overload_wait_cancels_it_tmux() {
    pane_closed_during_an_overload_wait_cancels_it(Multiplexer::Tmux);
}

#[test]
fn c6_pane_closed_during_an_overload_wait_cancels_it_zellij() {
    pane_closed_during_an_overload_wait_cancels_it(Multiplexer::Zellij);
}

fn pane_closed_during_an_overload_wait_cancels_it(multiplexer: Multiplexer) {
    let backoff_seconds = 15;
    let mut scenario = Scenario::builder(multiplexer)
        .env(
            "CLAUDE_NONSTOP_OVERLOAD_BACKOFF_SECONDS",
            &backoff_seconds.to_string(),
        )
        .replies(failed_turn(Reply::ResetConnection))
        .start();
    let pane = scenario.launch_claude();
    let session_id = scenario.session_id();

    scenario.submit(PROMPT);

    expect_failure(&scenario, 1, "server_error", "(ECONNRESET)");
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        waits_out_overload(&verdict, "server_error", backoff_seconds),
        "{verdict}"
    );
    scenario.wait_for_log("resumer running", TURN);
    let wakes_at = scenario
        .sandbox
        .ledger_session(&session_id)
        .and_then(|record| record["reset_at"].as_u64())
        .expect("the overload wait is in the ledger");

    scenario.terminal.close(&pane);

    assert!(
        unix_now() < wakes_at,
        "the pane must be gone before the resumer wakes up at {wakes_at}: raise the backoff"
    );

    let cancelled = scenario.wait_for_ledger_session(
        &session_id,
        "the overload cancelled",
        Duration::from_secs(backoff_seconds) + TURN,
        |record| record["status"] != "stopped",
    );
    scenario.wait_for_log("overloaded: the pane is gone, nothing to resume", TURN);
    assert_eq!(
        (
            cancelled["status"].as_str(),
            cancelled["last_error"].as_str()
        ),
        (Some("cancelled"), Some("pane is gone")),
        "{cancelled}"
    );
    assert_eq!(prompts(&scenario), [PROMPT, PROMPT], "nothing is typed");
    assert!(
        scenario.sandbox.notifications().is_empty(),
        "nothing is resumed, nobody is notified"
    );
}

#[test]
fn c7_invalid_api_key_is_left_to_the_user_tmux() {
    failure_is_left_to_the_user(
        Reply::status(401, "authentication_error", "invalid x-api-key"),
        "authentication_failed",
        "Invalid API key",
    );
}

#[test]
fn c7_forbidden_request_is_left_to_the_user_tmux() {
    failure_is_left_to_the_user(
        Reply::status(
            403,
            "permission_error",
            "Your API key does not have permission to use the specified resource.",
        ),
        "authentication_failed",
        "Please run /login",
    );
}

#[test]
fn c7_exhausted_credit_is_left_to_the_user_tmux() {
    failure_is_left_to_the_user(
        Reply::status(
            400,
            "invalid_request_error",
            "Your credit balance is too low to access the Anthropic API. \
             Please go to Plans & Billing to upgrade or purchase credits.",
        ),
        "billing_error",
        "Credit balance is too low",
    );
}

fn failure_is_left_to_the_user(reply: Reply, error: &str, fragment: &str) {
    let mut scenario = Scenario::in_tmux().fallback_reply(reply).start();
    scenario.launch_claude();

    scenario.submit(PROMPT);

    expect_failure(&scenario, 1, error, fragment);
    let verdict = hook_verdict(&scenario, 1);
    assert!(
        verdict.ends_with(&format!(
            "hook: {error} is not something a retry fixes, leaving it to the user"
        )),
        "{verdict}"
    );
    assert!(
        prompts(&scenario).iter().all(|prompt| prompt == PROMPT),
        "nothing is typed after the failure: {:?}",
        prompts(&scenario)
    );
    assert!(
        scenario.sandbox.ledger().is_none(),
        "nothing waits in the ledger"
    );
    assert!(
        scenario.sandbox.notifications().is_empty(),
        "nobody is notified"
    );
}

fn failed_turn(reply: Reply) -> [Reply; 2] {
    [reply.clone(), reply]
}

fn request_rate_limited() -> Reply {
    Reply::status(
        429,
        "rate_limit_error",
        "Number of request tokens has exceeded your per-minute rate limit",
    )
}

fn expect_failure(scenario: &Scenario, number: usize, error: &str, fragment: &str) -> String {
    let failure = scenario
        .wait_for_hooks("StopFailure", number, TURN)
        .remove(number - 1);
    let message = failure.payload["last_assistant_message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    assert_eq!(failure.payload["error"], error, "{message}");
    assert!(message.contains(fragment), "{message}");

    message
}

fn hook_verdict(scenario: &Scenario, number: usize) -> String {
    scenario.wait_for(
        &format!("hook verdict #{number} in the claude-nonstop log"),
        TURN,
        |scenario| {
            scenario
                .sandbox
                .nonstop_log()
                .lines()
                .filter(|line| HOOK_VERDICTS.iter().any(|verdict| line.contains(verdict)))
                .nth(number - 1)
                .map(str::to_owned)
        },
    )
}

fn waits_out_overload(verdict: &str, error: &str, seconds: u64) -> bool {
    verdict.contains(&format!(
        "overloaded ({error}): resume in {seconds}s, level 1"
    ))
}

fn expect_resumed_after_overload(scenario: &Scenario, pane: &Pane) -> String {
    let session_id = scenario.session_id();
    let overload_message = scenario.nonstop_setting("agents.claude.overload_message");

    scenario.wait_for_prompt_request(&overload_message, TURN);
    let resumed =
        scenario.wait_for_ledger_session(&session_id, "the overload resumed", TURN, |record| {
            record["status"] == "resumed"
        });
    scenario.wait_for_log(&format!("overloaded: try 1 written to {pane}"), TURN);
    let notification = scenario.wait_for_notification("claude-nonstop: resumed", TURN);
    let stop = scenario.wait_for_hook("Stop", TURN);
    assert_eq!(
        (
            resumed["stop"].as_str(),
            resumed["resumed_by"].as_str(),
            resumed["attempts"].as_u64()
        ),
        (Some("overload"), Some("nonstop"), Some(1)),
        "{resumed}"
    );
    assert!(
        notification.body.contains("overloaded, try 1"),
        "{notification:?}"
    );
    assert_eq!(stop.payload["last_assistant_message"], "ok");

    overload_message
}

fn prompts(scenario: &Scenario) -> Vec<String> {
    scenario
        .mock
        .main_loop_requests()
        .iter()
        .map(|request| request.last_user_text().unwrap_or_default())
        .collect()
}
