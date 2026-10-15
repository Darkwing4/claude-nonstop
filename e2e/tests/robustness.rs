use std::path::PathBuf;

use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::sandbox::FakeBehavior;
use claude_nonstop_e2e::scenario::{Scenario, ScenarioBuilder, TURN, unix_now};

const PROMPT: &str = "say ok";
const BROKEN_LEDGER: &str = "{\"version\": 1, \"sessions\": {\"half-written";
const LIMIT_RESET_IN_SECONDS: u64 = 60 * 60;

#[test]
fn h1_corrupt_ledger_is_set_aside_and_the_session_still_resumes_tmux() {
    let mut scenario = overloaded_turn(Scenario::in_tmux()).start();
    scenario.launch_claude();
    let overload_message = scenario.nonstop_setting("agents.claude.overload_message");
    std::fs::write(scenario.sandbox.ledger_path(), BROKEN_LEDGER).expect("break the ledger");

    scenario.submit(PROMPT);

    scenario.wait_for_prompt_request(&overload_message, TURN);
    let session_id = scenario.session_id();
    let resumed = scenario.wait_for_ledger_session(
        &session_id,
        "the overloaded session resumed",
        TURN,
        |record| record["status"] == "resumed",
    );
    let set_aside = corrupt_ledgers(&scenario);
    assert_eq!(resumed["stop"], "overload");
    assert_eq!(set_aside.len(), 1, "one broken ledger is set aside");
    assert_eq!(
        std::fs::read_to_string(&set_aside[0]).expect("read the set-aside ledger"),
        BROKEN_LEDGER,
        "the broken ledger is kept as it was"
    );
    scenario.wait_for_log(&format!("set aside as {}", set_aside[0].display()), TURN);
}

#[test]
fn h2_unknown_nonstop_variable_fails_the_hook_without_a_trace_in_claude_tmux() {
    let mut scenario = Scenario::in_tmux()
        .replies([Reply::server_error(), Reply::server_error()])
        .start();
    let mut settings = scenario.sandbox.settings();
    settings["env"]["CLAUDE_NONSTOP_FOO"] = "1".into();
    scenario.sandbox.write_settings(&settings);
    scenario.launch_claude();

    scenario.submit(PROMPT);

    let outcome = wait_for_nonstop_hook_outcome(&scenario);
    let error = scenario.wait_for_log("CLAUDE_NONSTOP_FOO", TURN);
    let prompts: Vec<Option<String>> = scenario
        .mock
        .main_loop_requests()
        .iter()
        .map(|request| request.last_user_text())
        .collect();
    assert!(
        error.contains("CLAUDE_NONSTOP_FOO: unknown configuration key"),
        "{error}"
    );
    assert!(
        outcome.ends_with("completed with status 0"),
        "the generated hook command ends in `|| exit 0`, so Claude Code sees a successful hook: {outcome}"
    );
    assert_eq!(
        prompts,
        [Some(PROMPT.to_owned()), Some(PROMPT.to_owned())],
        "a hook without a configuration types nothing"
    );
}

#[test]
fn h3_missing_notify_send_does_not_block_a_resume_tmux() {
    resume_goes_through_when_notify_send(FakeBehavior::Missing, "native: not started");
}

#[test]
fn h3_hanging_notify_send_does_not_block_a_resume_tmux() {
    resume_goes_through_when_notify_send(FakeBehavior::Hang, "native: timed out");
}

#[test]
fn h3_hook_cancelled_over_a_hanging_notify_send_still_leaves_a_resumer_tmux() {
    let reset_at = (unix_now() + LIMIT_RESET_IN_SECONDS).div_ceil(60) * 60;
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .fake_command("notify-send", FakeBehavior::Hang)
        .replies([Reply::usage_limit(LimitClaim::FiveHour, reset_at)])
        .start();
    scenario.launch_claude();

    scenario.submit(PROMPT);

    let outcome = wait_for_nonstop_hook_outcome(&scenario);
    let session_id = scenario.session_id();
    scenario.wait_for_ledger_session(&session_id, "the limit in the ledger", TURN, |record| {
        record["status"] == "stopped"
    });
    let resumer = scenario.sandbox.ledger().expect("the ledger exists")["resumer_pid"].clone();
    let log = scenario.sandbox.nonstop_log();
    assert!(
        log.contains("resumer spawned"),
        "KNOWN BUG 8: the hook sends the limit notification before it spawns the resumer \
         and gives notify-send 5 s, as long as its own hook timeout, so Claude Code cancels the hook \
         and the recorded limit is left without a resumer to wake it \
         (resumer_pid: {resumer}, claude-nonstop log: {log:?}, Claude Code: {outcome})"
    );
}

fn resume_goes_through_when_notify_send(behavior: FakeBehavior, outcome: &str) {
    let mut scenario = overloaded_turn(Scenario::in_tmux())
        .fake_command("notify-send", behavior)
        .start();
    let pane = scenario.launch_claude();
    let overload_message = scenario.nonstop_setting("agents.claude.overload_message");

    scenario.submit(PROMPT);

    scenario.wait_for_prompt_request(&overload_message, TURN);
    let resumed = scenario.wait_for_log("overloaded: try 1 written to", TURN);
    assert!(resumed.contains(&format!("written to {pane}")), "{resumed}");
    assert!(resumed.contains(outcome), "{resumed}");
}

fn overloaded_turn(builder: ScenarioBuilder) -> ScenarioBuilder {
    let first_try_and_one_retry = [Reply::ResetConnection, Reply::ResetConnection];

    builder.replies(first_try_and_one_retry)
}

fn wait_for_nonstop_hook_outcome(scenario: &Scenario) -> String {
    scenario.wait_for(
        "Claude Code to finish the claude-nonstop StopFailure hook",
        TURN,
        |scenario| {
            let debug_log = std::fs::read_to_string(scenario.sandbox.debug_log()).ok()?;

            debug_log
                .lines()
                .filter(|line| {
                    line.contains("StopFailure:") && line.contains("claude-nonstop hook")
                })
                .find(|line| {
                    line.ends_with(" cancelled") || line.contains(" completed with status ")
                })
                .map(str::to_owned)
        },
    )
}

fn corrupt_ledgers(scenario: &Scenario) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(&scenario.sandbox.state_dir).expect("list the state directory");

    entries
        .map(|entry| entry.expect("read a state directory entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ledger.json.corrupt."))
        })
        .collect()
}
