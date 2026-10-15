use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::scenario::{STARTUP, Scenario, TURN, unix_now};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};

const PROMPT: &str = "say ok";
const DAEMON_READY: &str = "resumer running (persistent)";
const LIMIT_RECORDED: &str = "recorded, wake at";
const LIMIT_RESET_IN: Duration = Duration::from_secs(60 * 60);
const PROJECT_CWD: &str = "/e2e/project";

#[test]
fn e1_daemon_alone_records_a_limit_from_the_transcript_tmux() {
    let reset_at = whole_minute_after(LIMIT_RESET_IN);
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .without_install()
        .replies([Reply::usage_limit(LimitClaim::FiveHour, reset_at)])
        .start();
    scenario.start_daemon();
    scenario.wait_for_log(DAEMON_READY, STARTUP);
    scenario.launch_claude();

    scenario.submit(PROMPT);

    let session_id = scenario.session_id();
    let limit = scenario.wait_for_ledger_session(
        &session_id,
        "the daemon to record the limit",
        TURN,
        |record| record["status"] == "stopped",
    );
    scenario.wait_for_notification("claude-nonstop: limit", TURN);
    assert_eq!(limit["stop"], "usage_limit");
    assert_eq!(limit["detected_via"], "transcript");
    assert_eq!(
        limit["pane"],
        Value::Null,
        "the daemon cannot know the pane"
    );
    assert_eq!(
        (
            limit["limit"].as_str(),
            limit["reset_source"].as_str(),
            limit["reset_at"].as_u64()
        ),
        (Some("five_hour"), Some("absolute"), Some(reset_at)),
        "the banner in the transcript carries the reset time"
    );
}

#[test]
fn e2_daemon_and_hook_share_one_limit_record_and_one_resumer_tmux() {
    let reset_at = whole_minute_after(Duration::from_secs(60));
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::SevenDay, reset_at)])
        .start();
    scenario.start_daemon();
    scenario.wait_for_log(DAEMON_READY, STARTUP);
    let pane = scenario.launch_claude();
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");

    scenario.submit(PROMPT);

    let session_id = scenario.session_id();
    scenario.wait_for(
        "both the hook and the daemon to record the limit",
        TURN,
        |scenario| {
            let log = scenario.sandbox.nonstop_log();
            let recorded_by = |component: &str| {
                log.lines()
                    .any(|line| line.contains(component) && line.contains(LIMIT_RECORDED))
            };

            (recorded_by(" hook[") && recorded_by(" daemon[")).then_some(())
        },
    );
    let ledger = scenario
        .sandbox
        .ledger()
        .expect("the limit is in the ledger");
    let sessions = ledger["sessions"]
        .as_object()
        .expect("the ledger lists sessions");
    let limit = &sessions[&session_id];
    assert_eq!(sessions.len(), 1, "one record for one limit: {ledger}");
    assert_eq!(limit["pane"]["pane_id"], pane.id, "the hook knows the pane");
    assert_eq!(
        (
            limit["limit"].as_str(),
            limit["reset_source"].as_str(),
            limit["reset_at"].as_u64()
        ),
        (Some("weekly"), Some("absolute"), Some(reset_at)),
        "the reset time comes from the transcript the daemon reads"
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
    let log = scenario.sandbox.nonstop_log();
    assert_eq!(
        continuations,
        [Some(resume_message)],
        "one resumer types one resume message"
    );
    assert_eq!(resumed["resumed_by"], "nonstop");
    assert!(
        !log.contains("resumer spawned"),
        "the running daemon is the resumer, the hook spawns no second one:\n{log}"
    );
}

#[test]
fn e3_daemon_rereads_a_truncated_transcript() {
    let mut scenario = Scenario::in_tmux().without_install().start();
    let transcript = project_dir(&scenario, "-e2e-known").join("history.jsonl");
    append_records(
        &transcript,
        &[
            user_record(&scenario, "e3-history"),
            limit_record(&scenario, "e3-history"),
        ],
    );
    scenario.start_daemon();
    scenario.wait_for_log(DAEMON_READY, STARTUP);

    append_records(&transcript, &[limit_record(&scenario, "e3-appended")]);

    wait_for_transcript_limit(&scenario, "e3-appended");
    assert!(
        scenario.sandbox.ledger_session("e3-history").is_none(),
        "a limit written before the daemon started is history, not a new limit"
    );

    truncate(&transcript);
    append_records(&transcript, &[limit_record(&scenario, "e3-truncated")]);

    wait_for_transcript_limit(&scenario, "e3-truncated");
}

#[test]
fn e3_daemon_reads_a_project_directory_created_while_it_was_busy() {
    let mut scenario = Scenario::in_tmux().without_install().start();
    let watched = project_dir(&scenario, "-e2e-watched");
    scenario.start_daemon();
    scenario.wait_for_log(DAEMON_READY, STARTUP);
    let daemon = scenario.daemon_pid();

    signal(daemon, Signal::STOP);
    append_records(
        &project_dir(&scenario, "-e2e-fresh").join("fresh.jsonl"),
        &[
            user_record(&scenario, "e3-fresh"),
            limit_record(&scenario, "e3-fresh"),
        ],
    );
    signal(daemon, Signal::CONT);
    append_records(
        &watched.join("marker.jsonl"),
        &[limit_record(&scenario, "e3-marker")],
    );

    wait_for_transcript_limit(&scenario, "e3-marker");
    assert!(
        scenario.sandbox.ledger_session("e3-fresh").is_some(),
        "PRODUCT BUG (found by e2e): a project directory that appears while the daemon is busy is \
         registered at its current size, so the limit already written into its first transcript is never read"
    );
}

fn wait_for_transcript_limit(scenario: &Scenario, session_id: &str) -> Value {
    let limit = scenario.wait_for_ledger_session(
        session_id,
        &format!("the daemon to record the limit of {session_id}"),
        TURN,
        |record| record["status"] == "stopped",
    );
    assert_eq!(limit["detected_via"], "transcript");
    assert_eq!(limit["limit"], "five_hour");
    assert_eq!(limit["reset_source"], "absolute");

    limit
}

fn project_dir(scenario: &Scenario, name: &str) -> PathBuf {
    let dir = scenario.sandbox.claude_config.join("projects").join(name);
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));

    dir
}

fn user_record(scenario: &Scenario, session_id: &str) -> Value {
    json!({
        "type": "user",
        "timestamp": timestamp_now(scenario),
        "sessionId": session_id,
        "cwd": PROJECT_CWD,
        "message": {"role": "user", "content": PROMPT},
    })
}

fn limit_record(scenario: &Scenario, session_id: &str) -> Value {
    let reset_at = format!("@{}", whole_minute_after(LIMIT_RESET_IN));
    let reset_clock = date(scenario, &["-d", &reset_at, "+%-I:%M%P"]);
    let time_zone = sandbox_env(scenario, "TZ");
    let banner = format!("You've hit your session limit · resets {reset_clock} ({time_zone})");

    json!({
        "type": "assistant",
        "timestamp": timestamp_now(scenario),
        "sessionId": session_id,
        "cwd": PROJECT_CWD,
        "isApiErrorMessage": true,
        "error": "rate_limit",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": banner}],
        },
    })
}

fn timestamp_now(scenario: &Scenario) -> String {
    date(scenario, &["-u", "+%Y-%m-%dT%H:%M:%S.%3NZ"])
}

fn append_records(transcript: &Path, records: &[Value]) {
    let text: String = records.iter().map(|record| format!("{record}\n")).collect();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(transcript)
        .unwrap_or_else(|error| panic!("open {}: {error}", transcript.display()));

    file.write_all(text.as_bytes())
        .unwrap_or_else(|error| panic!("append to {}: {error}", transcript.display()));
}

fn truncate(transcript: &Path) {
    OpenOptions::new()
        .write(true)
        .open(transcript)
        .and_then(|file| file.set_len(0))
        .unwrap_or_else(|error| panic!("truncate {}: {error}", transcript.display()));
}

fn signal(pid: u32, signal: Signal) {
    let pid = Pid::from_raw(pid as i32).expect("the daemon has a pid");

    kill_process(pid, signal)
        .unwrap_or_else(|error| panic!("signal {signal:?} to {pid:?}: {error}"));
}

fn sandbox_env<'a>(scenario: &'a Scenario, name: &str) -> &'a str {
    scenario
        .sandbox
        .env()
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| panic!("the sandbox sets {name}"))
}

fn date(scenario: &Scenario, args: &[&str]) -> String {
    let output = scenario
        .sandbox
        .command("date")
        .args(args)
        .output()
        .expect("run date");
    assert!(output.status.success(), "date {args:?} failed");

    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

fn whole_minute_after(delay: Duration) -> u64 {
    (unix_now() + delay.as_secs()).div_ceil(60) * 60
}
