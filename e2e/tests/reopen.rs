use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::scenario::{STARTUP, Scenario, ScenarioBuilder, TURN, unix_now};
use serde_json::Value;

const RESUME_SESSION: &str = "claude-nonstop";
const AN_HOUR: u64 = 3600;

#[test]
fn d6_closed_pane_is_reopened_after_the_limit_tmux() {
    closed_pane_is_reopened_after_the_limit(Scenario::in_tmux());
}

#[test]
fn d6_closed_pane_is_reopened_after_the_limit_zellij() {
    closed_pane_is_reopened_after_the_limit(Scenario::in_zellij());
}

#[test]
fn d6_closed_pane_is_reopened_in_the_configured_multiplexer_zellij() {
    closed_pane_is_reopened_after_the_limit(
        Scenario::in_zellij().env("CLAUDE_NONSTOP_GENERAL_MULTIPLEXER", "zellij"),
    );
}

#[test]
fn d7_session_in_an_unknown_pane_is_left_to_the_user_tmux() {
    let mut scenario = limited_session(Scenario::in_tmux());
    scenario.launch_claude_outside_multiplexer();
    scenario.submit("say ok");

    let session_id = scenario.session_id();
    scenario.wait_for_log("rate_limit outside any pane", TURN);
    let limit = wait_for_status(&scenario, &session_id, "stopped");
    let claude_pid = scenario
        .sandbox
        .claude_pid(&session_id)
        .expect("Claude Code registers its pid in sessions/");
    assert!(limit["pane"].is_null(), "{limit}");

    resume_now(&scenario, &session_id);

    let failed = wait_for_status(&scenario, &session_id, "failed");
    let notification = scenario.wait_for_notification("claude-nonstop: needs you", TURN);
    assert_eq!(
        failed["last_error"],
        format!("session still runs as pid {claude_pid} in an unknown pane")
    );
    assert!(
        notification.body.contains("its pane is unknown"),
        "{notification:?}"
    );
    assert!(
        windows_in_resume_session(&scenario).is_empty(),
        "nothing is reopened next to a live session"
    );
    assert_eq!(
        scenario.mock.main_loop_requests().len(),
        1,
        "nothing is typed anywhere"
    );
}

fn closed_pane_is_reopened_after_the_limit(builder: ScenarioBuilder) {
    let mut scenario = limited_session(builder);
    let pane = scenario.launch_claude();
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");
    scenario.submit("say ok");

    let session_id = scenario.session_id();
    let limit = wait_for_status(&scenario, &session_id, "stopped");
    let claude_pid = scenario
        .sandbox
        .claude_pid(&session_id)
        .expect("Claude Code registers its pid in sessions/");
    let limited_pane = scenario.pane_at(&limit["pane"]).map(|listed| listed.pane);
    assert_eq!(limited_pane, Some(pane.clone()), "{limit}");

    scenario.terminal.close(&pane);
    scenario.wait_for_exit("claude in the closed pane", claude_pid, TURN);
    resume_now(&scenario, &session_id);

    let reopened = scenario.wait_for_log("reopened in", STARTUP + TURN);
    let record = wait_for_status(&scenario, &session_id, "resumed");
    assert_eq!(
        record["pane"]["mux"],
        pane.multiplexer.name(),
        "KNOWN BUG 6: general.multiplexer=auto reopens a session closed in {} in tmux \
         whenever tmux is on PATH, a detached session the user never looks at: {reopened}",
        pane.multiplexer.name()
    );
    let window = scenario
        .pane_at(&record["pane"])
        .expect("the ledger points at the reopened window");
    scenario.adopt_claude(window.pane.clone());
    let resume = scenario.wait_for_prompt_request(&resume_message, TURN);
    let starts = scenario.wait_for_hooks("SessionStart", 2, TURN);
    scenario.wait_for_notification("claude-nonstop: reopened", TURN);
    assert_eq!(window.pane.session, RESUME_SESSION);
    assert!(window.alive, "{window:?}");
    assert!(
        reopened.contains(&format!("reopened in {}, resume 1/", window.pane)),
        "{reopened}"
    );
    assert_eq!(
        (
            &starts[1].payload["source"],
            &starts[1].payload["session_id"]
        ),
        (&Value::from("resume"), &Value::from(session_id.as_str())),
        "the window runs claude --resume <session id>"
    );
    assert!(
        resume.body["messages"].to_string().contains("say ok"),
        "the reopened session carries the conversation from before the limit"
    );
    assert_eq!(record["resumed_by"], "nonstop");
}

fn limited_session(builder: ScenarioBuilder) -> Scenario {
    builder
        .subscription()
        .login_in_settings()
        .replies([Reply::usage_limit(
            LimitClaim::FiveHour,
            unix_now() + AN_HOUR,
        )])
        .start()
}

fn wait_for_status(scenario: &Scenario, session_id: &str, status: &str) -> Value {
    scenario.wait_for_ledger_session(
        session_id,
        &format!("the session {status} in the ledger"),
        STARTUP + TURN,
        |record| record["status"] == status,
    )
}

fn resume_now(scenario: &Scenario, session_id: &str) {
    let output = scenario.sandbox.nonstop(&["resume-now", session_id]);

    assert!(
        output.status.success(),
        "claude-nonstop resume-now failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn windows_in_resume_session(scenario: &Scenario) -> Vec<String> {
    scenario
        .terminal
        .panes()
        .into_iter()
        .filter(|listed| listed.pane.session == RESUME_SESSION)
        .map(|listed| listed.pane.to_string())
        .collect()
}
