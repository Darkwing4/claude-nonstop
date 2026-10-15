use std::time::Duration;

use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::scenario::{Scenario, ScenarioBuilder, TURN, unix_now};
use claude_nonstop_e2e::terminal::{Key, Screen};
use serde_json::{Value, json};

const PROBE: Duration = Duration::from_secs(60);
const EPISODE_WINDOW: Duration = Duration::from_secs(120);
const MENU_QUESTION: &str = "What do you want to do?";
const MENU_ANSWERED: &str = "rate limit menu answered";

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

#[test]
fn d1_slow_five_hour_limit_resumes_once_after_reset_zellij() {
    let reset_at = whole_minute_after(PROBE * 2);
    let mut scenario = probing_every_minute(Scenario::in_zellij())
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::FiveHour, reset_at)])
        .start();
    let pane = scenario.launch_claude();
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");

    let limited = hit_the_limit(&scenario);
    assert!(
        limited
            .banner
            .starts_with("You've hit your session limit · resets"),
        "{}",
        limited.banner
    );

    let limit = reset_time_learned(&scenario, &limited.session_id);
    assert_eq!(recorded_pane_id(&limit), pane.id);
    assert_eq!(
        (
            limit["limit"].as_str(),
            limit["reset_source"].as_str(),
            limit["reset_at"].as_u64()
        ),
        (Some("five_hour"), Some("absolute"), Some(reset_at)),
        "{limit}"
    );

    let resumed = scenario.wait_for_ledger_session(
        &limited.session_id,
        "the session resumed after the reset",
        until(reset_at) + TURN,
        |record| record["status"] == "resumed",
    );
    scenario.wait_for_hook("Stop", TURN);
    assert_eq!(
        user_prompts(&scenario)[1..],
        [resume_message],
        "KNOWN BUG 3: after a five-hour reset the session must continue exactly once, with claude-nonstop's resume message; \
         Claude Code's own auto-continue races the resumer (ledger record: {resumed})"
    );
    assert_eq!(resumed["resumed_by"], "nonstop");
}

#[test]
fn d2_weekly_limit_menu_is_answered_as_soon_as_it_opens_tmux() {
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::SevenDay, weekly_reset())])
        .start();
    scenario.launch_claude();

    hit_the_limit(&scenario);

    let answers = menu_answers(&scenario);
    assert!(
        answers.iter().any(|answer| answer.contains(" hook[")),
        "PRODUCT BUG (found by e2e): the StopFailure hook looks for the rate limit menu before Claude Code draws it, \
         so the menu stays open until the reset; answers so far: {answers:?}, screen:\n{}",
        scenario.screen().tail(8)
    );
    assert!(
        answers[0].ends_with("stop and wait (0 steps)"),
        "{answers:?}"
    );
    scenario.wait_for_screen("the menu closed", TURN, Screen::is_idle_prompt);
}

#[test]
fn d2_five_hour_limit_menu_is_answered_before_the_resume_tmux() {
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::FiveHour, far_reset())])
        .start();
    turn_off_native_auto_continue(&scenario);
    scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    resume_now(&scenario, &limited.session_id);

    let verdict = first_resume_verdict(&scenario);
    let answers = menu_answers(&scenario);
    assert!(
        answers
            .last()
            .is_some_and(|answer| answer.ends_with("stop and wait (0 steps)")),
        "{answers:?}"
    );
    assert!(
        verdict.contains("resume 1/5 written to"),
        "PRODUCT BUG (found by e2e): right after pressing Enter in the rate limit menu the resumer reads the input line \
         while the menu is still drawn, takes the menu options for text the user is typing and backs off: {verdict}"
    );
    resumed_once_by_nonstop(&scenario, &limited.session_id);
}

#[test]
fn d2_menu_cursor_on_the_second_option_is_moved_to_stop_and_wait_tmux() {
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::SevenDay, weekly_reset())])
        .start();
    let pane = scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    show_rate_limit_menu(&scenario);
    scenario.terminal.press(&pane, Key::Down);
    scenario.wait_for_screen("the menu cursor on option 2", TURN, |screen| {
        screen.contains("❯ 2.")
    });

    resume_now(&scenario, &limited.session_id);

    scenario.wait_for_log(
        "rate limit menu answered with stop and wait (-1 steps)",
        TURN,
    );
    let after = scenario.wait_for_screen("the menu closed", TURN, |screen| {
        !screen.contains(MENU_QUESTION)
    });
    assert!(
        !after.contains("ontinuing automatically"),
        "the menu is left with option 1, not option 2:\n{}",
        after.tail(8)
    );
}

#[test]
fn d3_slow_limit_hit_again_after_the_resume_is_recorded_anew_tmux() {
    let first_reset = whole_minute_after(EPISODE_WINDOW + PROBE / 2);
    let second_reset = far_reset();
    let mut scenario = probing_every_minute(Scenario::in_tmux())
        .subscription()
        .replies([
            Reply::usage_limit(LimitClaim::FiveHour, first_reset),
            Reply::usage_limit(LimitClaim::FiveHour, second_reset),
        ])
        .start();
    turn_off_native_auto_continue(&scenario);
    scenario.launch_claude();
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");

    let limited = hit_the_limit(&scenario);
    stop_and_wait_by_hand(&scenario);
    scenario.wait_for_prompt_request(&resume_message, until(first_reset) + TURN);
    scenario.wait_for_hooks("StopFailure", 2, TURN);
    wait_for_log_lines(&scenario, "recorded, wake at", 2);

    let verdict = scenario.wait_for_log_line("the resumer's check of the resume", TURN, |line| {
        line.contains("resume delivered") || line.contains("still limited after the resume")
    });
    let settled = scenario
        .sandbox
        .ledger_session(&limited.session_id)
        .expect("the session stays in the ledger");
    assert_eq!(
        settled["status"], "stopped",
        "PRODUCT BUG (found by e2e): a limit hit again by the resume is lost: the hook reads the old banner from the \
         not yet flushed transcript and folds the new limit into the open episode, the resumer compares whole seconds \
         and misses a banner from the same second as the resume, so the session is marked resumed while it is limited \
         ({verdict}; ledger record: {settled})"
    );

    let record = scenario.wait_for_ledger_session(
        &limited.session_id,
        "the second limit with its reset time",
        PROBE + TURN,
        |record| record["status"] == "stopped" && record["reset_source"] != "fallback",
    );
    assert_eq!(
        (
            record["limit"].as_str(),
            record["reset_source"].as_str(),
            record["reset_at"].as_u64()
        ),
        (Some("five_hour"), Some("absolute"), Some(second_reset)),
        "{record}"
    );
    assert_eq!(
        user_prompts(&scenario),
        ["say ok", resume_message.as_str()],
        "one resume, then the new limit is waited out"
    );
}

#[test]
fn d4_limit_without_reset_time_waits_for_resume_now_tmux() {
    let mut scenario = limit_without_reset_time(Scenario::in_tmux()).start();
    scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    let notification = scenario.wait_for_notification("claude-nonstop: limit", TURN);
    assert_eq!(
        (
            limited.record["limit"].as_str(),
            limited.record["reset_source"].as_str()
        ),
        (Some("unknown"), Some("fallback")),
        "{}",
        limited.record
    );
    assert_eq!(
        limited.record["reset_at"].as_u64(),
        limited.record["detected_at"]
            .as_u64()
            .map(|detected_at| detected_at + 15 * 60)
    );
    assert!(
        notification
            .body
            .ends_with("no reset time, probing every 15 min"),
        "{notification:?}"
    );

    resume_now(&scenario, &limited.session_id);
    resumed_once_by_nonstop(&scenario, &limited.session_id);
}

#[test]
fn d4_slow_limit_without_reset_time_is_probed_without_typing_tmux() {
    let mut scenario = limit_without_reset_time(probing_every_minute(Scenario::in_tmux())).start();
    scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    let probed = scenario.wait_for_ledger_session(
        &limited.session_id,
        "the first probe",
        PROBE + TURN,
        |record| record["probe_count"] == 1,
    );
    scenario.wait_for_log("no reset time known, probing again in 1 min", TURN);
    assert_eq!(
        (probed["status"].as_str(), probed["reset_source"].as_str()),
        (Some("stopped"), Some("fallback")),
        "{probed}"
    );
    assert_eq!(
        user_prompts(&scenario),
        ["say ok", "say ok"],
        "a probe types nothing"
    );
}

#[test]
fn d5_slow_without_auto_resume_the_reset_is_left_to_the_user_tmux() {
    let reset_at = whole_minute_after(PROBE);
    let mut scenario = probing_every_minute(Scenario::in_tmux())
        .subscription()
        .env("CLAUDE_NONSTOP_GENERAL_AUTO_RESUME", "0")
        .replies([Reply::usage_limit(LimitClaim::FiveHour, reset_at)])
        .start();
    turn_off_native_auto_continue(&scenario);
    scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    stop_and_wait_by_hand(&scenario);

    let held = scenario.wait_for_ledger_session(
        &limited.session_id,
        "the reset left to the user",
        until(reset_at).max(PROBE) + TURN,
        |record| record["status"] == "failed",
    );
    let notification = scenario.wait_for_notification("claude-nonstop: needs you", TURN);
    assert_eq!(
        held["last_error"], "auto_resume is off, waiting for resume-now",
        "{held}"
    );
    assert!(notification.urgent, "{notification:?}");
    assert_eq!(user_prompts(&scenario), ["say ok"], "nothing is typed");

    resume_now(&scenario, &limited.session_id);
    resumed_once_by_nonstop(&scenario, &limited.session_id);
}

#[test]
fn d8_session_that_went_on_by_itself_is_not_resumed_tmux() {
    let answer = "back at it";
    let the_user_comes_back_a_second_later = Reply::slow(Duration::from_secs(1), Reply::ok(answer));
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .replies([
            Reply::usage_limit(LimitClaim::FiveHour, far_reset()),
            the_user_comes_back_a_second_later,
        ])
        .start();
    scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    scenario.submit("go on");
    wait_for_answer_in_transcript(&scenario, &limited.session_id, answer);

    resume_now(&scenario, &limited.session_id);
    let record = scenario.wait_for_ledger_session(
        &limited.session_id,
        "the session marked resumed",
        TURN,
        |record| record["status"] == "resumed",
    );
    scenario.wait_for_log("the session went on by itself, nothing to resume", TURN);
    assert_eq!(record["resumed_by"], "native", "{record}");
    assert_eq!(
        user_prompts(&scenario),
        ["say ok", "go on"],
        "claude-nonstop types nothing"
    );
}

#[test]
fn d9_restarted_resumer_checks_the_resume_right_away_tmux() {
    let mut scenario = Scenario::in_tmux()
        .subscription()
        .env("CLAUDE_NONSTOP_LIMIT_VERIFY_SECONDS", "3600")
        .replies([Reply::usage_limit(LimitClaim::FiveHour, far_reset())])
        .start();
    scenario.launch_claude();
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");

    let limited = hit_the_limit(&scenario);
    resume_now(&scenario, &limited.session_id);
    scenario.wait_for_prompt_request(&resume_message, TURN);
    scenario.wait_for_ledger_session(
        &limited.session_id,
        "the resume waiting for its check",
        TURN,
        |record| record["status"] == "resuming",
    );

    let resumer = scenario.sandbox.ledger().expect("the ledger is written")["resumer_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("the running resumer is in the ledger");
    scenario.sandbox.kill_process(resumer);
    scenario.start_daemon();

    resumed_once_by_nonstop(&scenario, &limited.session_id);
}

#[test]
fn d10_slow_weekly_limit_is_recorded_with_its_reset_date_tmux() {
    let reset_at = weekly_reset();
    let mut scenario = probing_every_minute(Scenario::in_tmux())
        .subscription()
        .replies([Reply::usage_limit(LimitClaim::SevenDay, reset_at)])
        .start();
    scenario.launch_claude();

    let limited = hit_the_limit(&scenario);
    assert!(
        limited
            .banner
            .starts_with("You've hit your weekly limit · resets"),
        "{}",
        limited.banner
    );

    let limit = reset_time_learned(&scenario, &limited.session_id);
    assert_eq!(
        (
            limit["status"].as_str(),
            limit["limit"].as_str(),
            limit["reset_source"].as_str(),
            limit["reset_at"].as_u64()
        ),
        (
            Some("stopped"),
            Some("weekly"),
            Some("absolute"),
            Some(reset_at)
        ),
        "{limit}"
    );
    assert_eq!(user_prompts(&scenario), ["say ok"], "nothing is typed");
}

struct Limited {
    session_id: String,
    banner: String,
    record: Value,
}

fn hit_the_limit(scenario: &Scenario) -> Limited {
    scenario.submit("say ok");

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "rate_limit", "{failure:?}");

    let session_id = failure.payload["session_id"]
        .as_str()
        .expect("StopFailure carries a session id")
        .to_owned();
    scenario.wait_for_log("recorded, wake at", TURN);
    let record = scenario
        .sandbox
        .ledger_session(&session_id)
        .expect("the limit is in the ledger");
    assert_eq!(
        (record["status"].as_str(), record["stop"].as_str()),
        (Some("stopped"), Some("usage_limit")),
        "{record}"
    );

    scenario.wait_for_screen("claude waiting for the user", TURN, |screen| {
        screen.is_idle_prompt() || screen.contains(MENU_QUESTION)
    });

    Limited {
        banner: failure.payload["last_assistant_message"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        session_id,
        record,
    }
}

fn reset_time_learned(scenario: &Scenario, session_id: &str) -> Value {
    scenario.wait_for_ledger_session(
        session_id,
        "the reset time in the ledger",
        PROBE + TURN,
        |record| record["reset_source"] != "fallback",
    )
}

fn recorded_pane_id(record: &Value) -> String {
    match &record["pane"]["pane_id"] {
        Value::String(id) => id.clone(),
        other => other.to_string(),
    }
}

fn menu_answers(scenario: &Scenario) -> Vec<String> {
    scenario
        .sandbox
        .nonstop_log()
        .lines()
        .filter(|line| line.contains(MENU_ANSWERED))
        .map(str::to_owned)
        .collect()
}

fn show_rate_limit_menu(scenario: &Scenario) {
    if !menu_answers(scenario).is_empty() {
        scenario.submit("/rate-limit-options");
    }

    scenario.wait_for_screen("the rate limit menu", TURN, |screen| {
        screen.contains(MENU_QUESTION)
    });
}

fn stop_and_wait_by_hand(scenario: &Scenario) {
    if !menu_answers(scenario).is_empty() {
        return;
    }

    scenario.wait_for_screen("the rate limit menu", TURN, |screen| {
        screen.contains(MENU_QUESTION)
    });
    scenario.terminal.press(scenario.claude(), Key::Enter);
    scenario.wait_for_screen("the menu closed", TURN, Screen::is_idle_prompt);
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

fn first_resume_verdict(scenario: &Scenario) -> String {
    scenario.wait_for_log_line("the resumer's first try to type", TURN, |line| {
        line.contains("resume 1/") || line.contains("resume to ")
    })
}

fn resumed_once_by_nonstop(scenario: &Scenario, session_id: &str) {
    let resume_message = scenario.nonstop_setting("agents.claude.resume_message");
    scenario.wait_for_prompt_request(&resume_message, TURN);

    let record =
        scenario.wait_for_ledger_session(session_id, "the session resumed", TURN, |record| {
            record["status"] == "resumed"
        });
    let resumes = user_prompts(scenario)
        .into_iter()
        .filter(|prompt| *prompt == resume_message)
        .count();
    assert_eq!(record["resumed_by"], "nonstop", "{record}");
    assert_eq!(resumes, 1, "the resume message is typed once");
}

fn wait_for_answer_in_transcript(scenario: &Scenario, session_id: &str, answer: &str) {
    scenario.wait_for(&format!("{answer:?} in the transcript"), TURN, |scenario| {
        let path = scenario.sandbox.transcript(session_id)?;
        let transcript = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

        transcript
            .lines()
            .any(|line| line.contains(r#""type":"assistant""#) && line.contains(answer))
            .then_some(())
    });
}

fn wait_for_log_lines(scenario: &Scenario, fragment: &str, count: usize) {
    scenario.wait_for(
        &format!("{count} claude-nonstop log lines with {fragment:?}"),
        TURN,
        |scenario| {
            let seen = scenario
                .sandbox
                .nonstop_log()
                .lines()
                .filter(|line| line.contains(fragment))
                .count();

            (seen >= count).then_some(())
        },
    );
}

fn user_prompts(scenario: &Scenario) -> Vec<String> {
    scenario
        .mock
        .main_loop_requests()
        .iter()
        .filter_map(|request| request.last_user_text())
        .collect()
}

fn limit_without_reset_time(builder: ScenarioBuilder) -> ScenarioBuilder {
    let reset_at = far_reset();
    let first_try_and_one_retry = [
        Reply::usage_limit(LimitClaim::FiveHour, reset_at),
        Reply::usage_limit(LimitClaim::FiveHour, reset_at),
    ];

    builder.replies(first_try_and_one_retry)
}

fn probing_every_minute(builder: ScenarioBuilder) -> ScenarioBuilder {
    builder.env("CLAUDE_NONSTOP_LIMIT_PROBE_MINUTES", "1")
}

fn turn_off_native_auto_continue(scenario: &Scenario) {
    let mut settings = scenario.sandbox.settings();
    settings["autoContinueAtUsageLimit"] = json!(false);
    scenario.sandbox.write_settings(&settings);
}

fn far_reset() -> u64 {
    whole_minute_after(Duration::from_secs(2 * 3600))
}

fn weekly_reset() -> u64 {
    whole_minute_after(Duration::from_secs(3 * 24 * 3600))
}

fn whole_minute_after(delay: Duration) -> u64 {
    (unix_now() + delay.as_secs()).div_ceil(60) * 60
}

fn until(at: u64) -> Duration {
    Duration::from_secs(at.saturating_sub(unix_now()))
}
