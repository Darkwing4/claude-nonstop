use std::time::Duration;

use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::scenario::{STARTUP, Scenario, ScenarioBuilder, TURN, unix_now};
use claude_nonstop_e2e::terminal::Multiplexer;
use serde_json::Value;

const PROMPT: &str = "run the queued task";
const PROMPT_SESSION: &str = "claude-nonstop";
const MULTIPLEXER_SETTING: &str = "CLAUDE_NONSTOP_GENERAL_MULTIPLEXER";
const AN_HOUR: u64 = 3600;
const RETRY_BACKOFF: Duration = Duration::from_secs(60);

#[test]
fn f1_prompt_at_a_time_starts_a_session_tmux() {
    prompt_at_a_time_starts_a_session(Multiplexer::Tmux);
}

#[test]
fn f1_prompt_at_a_time_starts_a_session_zellij() {
    prompt_at_a_time_starts_a_session(Multiplexer::Zellij);
}

#[test]
fn f1_prompt_now_starts_a_session_tmux() {
    prompt_now_starts_a_session(Multiplexer::Tmux);
}

#[test]
fn f1_prompt_now_starts_a_session_zellij() {
    prompt_now_starts_a_session(Multiplexer::Zellij);
}

#[test]
fn f2_prompt_for_the_next_reset_starts_when_the_limit_resets_tmux() {
    let reset_at = whole_minute_after(Duration::from_secs(120));
    let mut scenario = prompt_queue(Multiplexer::Tmux)
        .subscription()
        .env("CLAUDE_NONSTOP_LIMIT_PROBE_MINUTES", "1")
        .replies([Reply::usage_limit(LimitClaim::FiveHour, reset_at)])
        .start();
    scenario.launch_claude();
    scenario.submit("say ok");
    let session_id = scenario.session_id();
    scenario.wait_for_ledger_session(&session_id, "the open limit", TURN, |record| {
        record["status"] == "stopped"
    });

    let id = queue_prompt(&scenario, &[PROMPT]);

    let queued = queued_prompt(&scenario, &id);
    assert_eq!(
        (&queued["when"], &queued["status"]),
        (&Value::from("next-reset"), &Value::from("pending")),
        "{queued}"
    );

    let until_reset = Duration::from_secs(reset_at.saturating_sub(unix_now()));
    let delivered = wait_for_delivery(&mut scenario, &id, until_reset + STARTUP + TURN);
    let launched_at = delivered["launched_at"].as_u64().unwrap_or_default();
    assert!(
        launched_at >= reset_at,
        "the prompt waits for the limit to reset at {reset_at}, launched at {launched_at}"
    );
    assert_eq!(prompt_requests(&scenario), 1);
}

#[test]
fn f3_slow_prompt_that_hits_a_limit_at_start_is_retried_tmux() {
    prompt_that_hits_a_limit_at_start_is_retried(Multiplexer::Tmux);
}

#[test]
fn f3_slow_prompt_that_hits_a_limit_at_start_is_retried_zellij() {
    prompt_that_hits_a_limit_at_start_is_retried(Multiplexer::Zellij);
}

#[test]
fn f4_headless_prompt_runs_claude_print() {
    let scenario = headless_prompt_queue().start();

    let id = queue_prompt(&scenario, &["--now", PROMPT]);

    let delivered = wait_for_status(&scenario, &id, "delivered", STARTUP + TURN);
    let claude_pid = headless_pid(&delivered);
    scenario.wait_for_exit("claude -p", claude_pid, TURN);
    let stop = scenario.wait_for_hook("Stop", TURN);
    let notification = scenario.wait_for_notification("claude-nonstop: prompt started", TURN);
    assert_eq!(prompt_requests(&scenario), 1);
    assert_eq!(stop.payload["last_assistant_message"], "ok");
    assert_eq!(
        headless_output(&scenario, &id).lines().last(),
        Some("ok"),
        "the answer of claude -p lands in the headless log"
    );
    assert!(
        notification
            .body
            .ends_with(&format!("session opened in headless pid {claude_pid}")),
        "{notification:?}"
    );
    assert!(
        scenario.terminal.panes().is_empty(),
        "headless prompts open no window"
    );
}

#[test]
fn f4_headless_prompt_that_hits_a_limit_records_it() {
    let scenario = headless_prompt_queue()
        .subscription()
        .replies([Reply::usage_limit(
            LimitClaim::FiveHour,
            unix_now() + AN_HOUR,
        )])
        .start();

    let id = queue_prompt(&scenario, &["--now", PROMPT]);

    let failure = scenario.wait_for_hook("StopFailure", STARTUP + TURN);
    let session_id = failure.payload["session_id"]
        .as_str()
        .expect("StopFailure carries a session id")
        .to_owned();
    let launched = scenario.wait_for(
        &format!("prompt {id} to run in a headless process"),
        TURN,
        |scenario| find_prompt(scenario, &id).filter(|prompt| prompt["pane"]["pid"].is_u64()),
    );
    scenario.wait_for_exit("claude -p", headless_pid(&launched), TURN);
    scenario.wait_for("the StopFailure hook to finish", TURN, |scenario| {
        (!scenario.sandbox.is_running("claude-nonstop hook")).then_some(())
    });
    assert_eq!(failure.payload["error"], "rate_limit");

    let limit = scenario.sandbox.ledger_session(&session_id);
    assert!(
        limit
            .as_ref()
            .is_some_and(|record| record["status"] == "stopped"),
        "KNOWN BUG 2: claude -p exits right after the failure and kills the StopFailure hook, \
         so the usage limit of a headless run is never recorded (ledger record: {limit:?}, log:\n{})",
        scenario.sandbox.nonstop_log()
    );
}

fn prompt_at_a_time_starts_a_session(multiplexer: Multiplexer) {
    let mut scenario = prompt_queue(multiplexer).start();

    let id = queue_prompt(&scenario, &["--at", "+5s", PROMPT]);

    let queued = queued_prompt(&scenario, &id);
    let due = queued["at"].as_u64().expect("--at stores a time");
    assert_eq!(queued["when"], "at", "{queued}");

    let delivered = wait_for_delivery(&mut scenario, &id, STARTUP + TURN);
    let launched_at = delivered["launched_at"].as_u64().unwrap_or_default();
    assert!(
        launched_at >= due,
        "the prompt starts no earlier than {due}, launched at {launched_at}"
    );
    assert_eq!(prompt_requests(&scenario), 1);
}

fn prompt_now_starts_a_session(multiplexer: Multiplexer) {
    let mut scenario = prompt_queue(multiplexer).start();

    let id = queue_prompt(&scenario, &["--now", PROMPT]);

    assert_eq!(queued_prompt(&scenario, &id)["when"], "now");
    wait_for_delivery(&mut scenario, &id, STARTUP + TURN);
    assert_eq!(prompt_requests(&scenario), 1);
}

fn prompt_that_hits_a_limit_at_start_is_retried(multiplexer: Multiplexer) {
    let mut scenario = prompt_queue(multiplexer)
        .subscription()
        .replies([Reply::usage_limit(
            LimitClaim::FiveHour,
            unix_now() + AN_HOUR,
        )])
        .start();

    let id = queue_prompt(&scenario, &["--now", PROMPT]);

    let failed_attempt =
        scenario.wait_for_log(&format!("prompt {id} attempt 1 failed"), STARTUP + TURN);
    let retry = wait_for_status(&scenario, &id, "pending", TURN);
    assert!(
        failed_attempt.ends_with("hit a usage limit right after starting; retry in 60s"),
        "{failed_attempt}"
    );
    assert_eq!(retry["attempts"], 1, "{retry}");

    let delivered = wait_for_delivery(&mut scenario, &id, RETRY_BACKOFF + STARTUP + TURN);
    let not_before = retry["not_before"].as_u64().expect("a retry has a gate");
    let launched_at = delivered["launched_at"].as_u64().unwrap_or_default();
    assert_eq!(delivered["attempts"], 2, "{delivered}");
    assert!(
        launched_at >= not_before,
        "the retry waits for {not_before}, launched at {launched_at}"
    );
    assert_eq!(prompt_requests(&scenario), 2, "one request per attempt");
}

fn prompt_queue(multiplexer: Multiplexer) -> ScenarioBuilder {
    Scenario::builder(multiplexer)
        .login_in_settings()
        .env(MULTIPLEXER_SETTING, multiplexer.name())
}

fn headless_prompt_queue() -> ScenarioBuilder {
    Scenario::in_tmux()
        .login_in_settings()
        .env(MULTIPLEXER_SETTING, "headless")
}

fn queue_prompt(scenario: &Scenario, args: &[&str]) -> String {
    let command = [&["prompt", "add"], args].concat();
    let output = scenario.sandbox.nonstop(&command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "claude-nonstop {} failed: {stdout}{}",
        command.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );

    stdout
        .split_whitespace()
        .next()
        .expect("prompt add prints the prompt id")
        .to_owned()
}

fn find_prompt(scenario: &Scenario, id: &str) -> Option<Value> {
    let ledger = scenario.sandbox.ledger()?;

    ledger["prompts"]
        .as_array()?
        .iter()
        .find(|prompt| prompt["id"] == id)
        .cloned()
}

fn queued_prompt(scenario: &Scenario, id: &str) -> Value {
    find_prompt(scenario, id).unwrap_or_else(|| panic!("prompt {id} is not in the ledger"))
}

fn wait_for_status(scenario: &Scenario, id: &str, status: &str, timeout: Duration) -> Value {
    scenario.wait_for(&format!("prompt {id} {status}"), timeout, |scenario| {
        find_prompt(scenario, id).filter(|prompt| prompt["status"] == status)
    })
}

fn wait_for_delivery(scenario: &mut Scenario, id: &str, timeout: Duration) -> Value {
    let delivered = wait_for_status(scenario, id, "delivered", timeout);
    let window = scenario
        .pane_at(&delivered["pane"])
        .unwrap_or_else(|| panic!("the window of prompt {id} is open: {delivered}"));
    scenario.adopt_claude(window.pane.clone());
    scenario.wait_for_prompt_request(PROMPT, TURN);
    let notification = scenario.wait_for_notification("claude-nonstop: prompt started", TURN);
    assert_eq!(window.pane.session, PROMPT_SESSION);
    assert!(
        notification
            .body
            .ends_with(&format!("session opened in {}", window.pane)),
        "{notification:?}"
    );

    let stop = scenario.wait_for_hook("Stop", TURN);
    assert_eq!(stop.payload["last_assistant_message"], "ok");

    delivered
}

fn prompt_requests(scenario: &Scenario) -> usize {
    scenario
        .mock
        .main_loop_requests()
        .iter()
        .filter(|request| request.last_user_text().as_deref() == Some(PROMPT))
        .count()
}

fn headless_pid(prompt: &Value) -> u32 {
    prompt["pane"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .unwrap_or_else(|| panic!("the prompt runs in a headless process: {prompt}"))
}

fn headless_output(scenario: &Scenario, id: &str) -> String {
    let log = scenario
        .sandbox
        .state_dir
        .join("headless")
        .join(format!("{id}.log"));

    std::fs::read_to_string(&log).unwrap_or_else(|error| panic!("read {}: {error}", log.display()))
}

fn whole_minute_after(delay: Duration) -> u64 {
    (unix_now() + delay.as_secs()).div_ceil(60) * 60
}
