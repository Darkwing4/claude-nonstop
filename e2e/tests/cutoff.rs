use std::time::Duration;

use claude_nonstop_e2e::mock_api::{Reply, Request};
use claude_nonstop_e2e::scenario::{Scenario, TURN};
use claude_nonstop_e2e::terminal::{Multiplexer, Pane};
use serde_json::json;

const PROMPT: &str = "say ok";
const DRAFT: &str = "a draft the user is still typing";
const CONTINUES_BEFORE_GIVING_UP: usize = 3;
const BYTE_WATCHDOG_MS: &str = "10000";
const REQUEST_TIMEOUT_MS: &str = "5000";
const TYPING: Duration = Duration::from_secs(10);
const FOREGROUND_SHELL: &[&str] = &["bash", "-c", "\"$@\"; exit $?", "foreground-shell"];
const OUTSIDE_ANY_PANE: &[&str] = &[
    "env",
    "-u",
    "TMUX",
    "-u",
    "TMUX_PANE",
    "-u",
    "ZELLIJ",
    "-u",
    "ZELLIJ_SESSION_NAME",
    "-u",
    "ZELLIJ_PANE_ID",
];

#[test]
fn b1_turn_cut_off_by_server_errors_is_continued_tmux() {
    turn_cut_off_by_server_errors_is_continued(Multiplexer::Tmux);
}

#[test]
fn b1_turn_cut_off_by_server_errors_is_continued_zellij() {
    turn_cut_off_by_server_errors_is_continued(Multiplexer::Zellij);
}

#[test]
fn b1_cut_stream_with_failed_fallback_is_continued_tmux() {
    cut_stream_with_failed_fallback_is_continued(Multiplexer::Tmux);
}

#[test]
fn b1_cut_stream_with_failed_fallback_is_continued_zellij() {
    cut_stream_with_failed_fallback_is_continued(Multiplexer::Zellij);
}

#[test]
fn b1_stalled_stream_with_failed_fallback_is_continued_tmux() {
    stalled_stream_with_failed_fallback_is_continued(Multiplexer::Tmux);
}

#[test]
fn b1_stalled_stream_with_failed_fallback_is_continued_zellij() {
    stalled_stream_with_failed_fallback_is_continued(Multiplexer::Zellij);
}

#[test]
fn b2_cut_offs_without_progress_give_up_after_three_continues_tmux() {
    cut_offs_without_progress_give_up_after_three_continues(Multiplexer::Tmux);
}

#[test]
fn b2_cut_offs_without_progress_give_up_after_three_continues_zellij() {
    cut_offs_without_progress_give_up_after_three_continues(Multiplexer::Zellij);
}

#[test]
fn b3_tool_progress_resets_the_cut_off_counter_tmux() {
    tool_progress_resets_the_cut_off_counter(Multiplexer::Tmux);
}

#[test]
fn b3_tool_progress_resets_the_cut_off_counter_zellij() {
    tool_progress_resets_the_cut_off_counter(Multiplexer::Zellij);
}

#[test]
fn b4_text_in_the_input_line_blocks_the_continue_tmux() {
    text_in_the_input_line_blocks_the_continue(Multiplexer::Tmux);
}

#[test]
fn b4_text_in_the_input_line_blocks_the_continue_zellij() {
    text_in_the_input_line_blocks_the_continue(Multiplexer::Zellij);
}

#[test]
fn b4_other_program_in_the_foreground_blocks_the_continue_tmux() {
    other_program_in_the_foreground_blocks_the_continue(Multiplexer::Tmux);
}

#[test]
fn b4_other_program_in_the_foreground_blocks_the_continue_zellij() {
    other_program_in_the_foreground_blocks_the_continue(Multiplexer::Zellij);
}

#[test]
fn b4_claude_outside_any_pane_is_not_continued_tmux() {
    claude_outside_any_pane_is_not_continued(Multiplexer::Tmux);
}

#[test]
fn b4_claude_outside_any_pane_is_not_continued_zellij() {
    claude_outside_any_pane_is_not_continued(Multiplexer::Zellij);
}

#[test]
fn b5_cut_stream_saved_by_the_fallback_needs_nothing_tmux() {
    let cut_twice_then_fallback_answers = [Reply::CutStream, Reply::CutStream, Reply::ok("ok")];
    let mut scenario = Scenario::in_tmux()
        .replies(cut_twice_then_fallback_answers)
        .start();
    scenario.launch_claude();

    scenario.submit(PROMPT);

    let stop = scenario.wait_for_hook("Stop", TURN);
    assert_eq!(stop.payload["last_assistant_message"], "ok");
    assert_eq!(
        attempts(&scenario),
        ["stream", "stream", "fallback"],
        "two cut streams, then the non-stream fallback answers"
    );
    assert!(
        scenario.sandbox.hooks_of("StopFailure").is_empty(),
        "a turn saved by the fallback does not fail"
    );
    assert_eq!(
        scenario.sandbox.nonstop_log(),
        "",
        "claude-nonstop has nothing to do"
    );
    assert!(
        scenario.sandbox.notifications().is_empty(),
        "nobody is notified"
    );
}

#[test]
fn c8_request_timeout_is_continued_like_a_cut_off_tmux() {
    request_timeout_is_continued_like_a_cut_off(Multiplexer::Tmux);
}

#[test]
fn c8_request_timeout_is_continued_like_a_cut_off_zellij() {
    request_timeout_is_continued_like_a_cut_off(Multiplexer::Zellij);
}

fn turn_cut_off_by_server_errors_is_continued(multiplexer: Multiplexer) {
    let first_try_and_one_retry = [Reply::server_error(), Reply::server_error()];
    let mut scenario = Scenario::builder(multiplexer)
        .replies(first_try_and_one_retry)
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "server_error");
    expect_one_continue(&scenario, &pane, &["stream", "stream"]);
}

fn cut_stream_with_failed_fallback_is_continued(multiplexer: Multiplexer) {
    let cut_twice_then_fallback_fails = [Reply::CutStream, Reply::CutStream, Reply::server_error()];
    let mut scenario = Scenario::builder(multiplexer)
        .replies(cut_twice_then_fallback_fails)
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "server_error");
    expect_one_continue(&scenario, &pane, &["stream", "stream", "fallback"]);
}

fn stalled_stream_with_failed_fallback_is_continued(multiplexer: Multiplexer) {
    let stall_then_fallback_fails = [Reply::HangMidStream, Reply::server_error()];
    let mut scenario = Scenario::builder(multiplexer)
        .env("CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS", BYTE_WATCHDOG_MS)
        .replies(stall_then_fallback_fails)
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "server_error");
    expect_one_continue(&scenario, &pane, &["stream", "fallback"]);
}

fn cut_offs_without_progress_give_up_after_three_continues(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer)
        .fallback_reply(Reply::server_error())
        .start();
    let pane = scenario.launch_claude();
    let cutoff_message = scenario.nonstop_setting("agents.claude.cutoff_message");

    scenario.submit(PROMPT);

    let failed_turns = CONTINUES_BEFORE_GIVING_UP + 1;
    let verdicts = hook_verdicts(&scenario, failed_turns, TURN * failed_turns as u32);
    for (verdict, resume) in verdicts.iter().zip(1..=CONTINUES_BEFORE_GIVING_UP) {
        assert!(
            verdict.contains(&format!("resume {resume}/3 written to {pane}")),
            "{verdict}"
        );
    }

    let last = &verdicts[CONTINUES_BEFORE_GIVING_UP];
    assert!(
        last.contains("3 resumes in a row without progress, giving up"),
        "{last}"
    );

    let notification = scenario.wait_for_notification("claude-nonstop: gave up", TURN);
    let counter = scenario
        .sandbox
        .cutoff_counter(&scenario.session_id())
        .expect("cut-off counter is written");
    let mut expected = vec![Some(PROMPT.to_owned()); 2];
    expected.extend(vec![Some(cutoff_message); 2 * CONTINUES_BEFORE_GIVING_UP]);
    assert_eq!(
        prompts(&scenario),
        expected,
        "every turn is tried twice; three continues, no fourth"
    );
    assert_eq!(counter["count"], 3);
    assert!(notification.urgent, "{notification:?}");
    assert_eq!(scenario.screen().input_line().as_deref(), Some(""));
}

fn tool_progress_resets_the_cut_off_counter(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer).start();
    let note = scenario.sandbox.workdir.join("note.txt");
    std::fs::write(&note, "progress\n").expect("write the note Claude reads");
    let read_the_note = Reply::tool_use("Read", json!({"file_path": note}));
    scenario.mock.enqueue([
        Reply::server_error(),
        Reply::server_error(),
        read_the_note,
        Reply::server_error(),
        Reply::server_error(),
    ]);
    let pane = scenario.launch_claude();
    let cutoff_message = scenario.nonstop_setting("agents.claude.cutoff_message");

    scenario.submit(PROMPT);

    let verdicts = hook_verdicts(&scenario, 2, TURN * 2);
    for verdict in &verdicts {
        assert!(
            verdict.contains(&format!("resume 1/3 written to {pane}")),
            "a tool result after the continue is progress, the count starts over: {verdict}"
        );
    }

    let stop = scenario.wait_for_hook("Stop", TURN);
    let counter = scenario
        .sandbox
        .cutoff_counter(&scenario.session_id())
        .expect("cut-off counter is written");
    let requests = scenario.mock.main_loop_requests();
    assert_eq!(stop.payload["last_assistant_message"], "ok");
    assert_eq!(
        prompts(&scenario)
            .iter()
            .flatten()
            .filter(|prompt| **prompt == cutoff_message)
            .count(),
        2,
        "one continue after each cut-off"
    );
    assert!(
        requests.iter().any(carries_tool_result),
        "Claude Code ran the tool and sent its result"
    );
    assert_eq!(counter["count"], 1);
}

fn text_in_the_input_line_blocks_the_continue(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer)
        .env("API_TIMEOUT_MS", REQUEST_TIMEOUT_MS)
        .replies([Reply::Hang, Reply::Hang])
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);
    scenario.wait_for_prompt_request(PROMPT, TURN);
    scenario.terminal.type_text(&pane, DRAFT);
    scenario.wait_for_screen("the draft in the input line", TYPING, |screen| {
        screen.input_line().as_deref() == Some(DRAFT)
    });
    assert!(
        scenario.sandbox.hooks_of("StopFailure").is_empty(),
        "the draft must be typed while the turn still hangs"
    );

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "server_error");
    let verdict = hook_verdicts(&scenario, 1, TURN).remove(0);
    assert!(
        verdict.contains(&format!("{pane} has text in its input line")),
        "{verdict}"
    );
    expect_nothing_typed(&scenario, DRAFT);
}

fn other_program_in_the_foreground_blocks_the_continue(multiplexer: Multiplexer) {
    let first_try_and_one_retry = [Reply::server_error(), Reply::server_error()];
    let mut scenario = Scenario::builder(multiplexer)
        .replies(first_try_and_one_retry)
        .start();
    let pane = scenario.launch_claude_under(FOREGROUND_SHELL, &[]);

    scenario.submit(PROMPT);

    scenario.wait_for_hook("StopFailure", TURN);
    let verdict = hook_verdicts(&scenario, 1, TURN).remove(0);
    assert!(
        verdict.contains(&format!("{pane} runs bash, not claude")),
        "{verdict}"
    );
    expect_nothing_typed(&scenario, "");
}

fn claude_outside_any_pane_is_not_continued(multiplexer: Multiplexer) {
    let first_try_and_one_retry = [Reply::server_error(), Reply::server_error()];
    let mut scenario = Scenario::builder(multiplexer)
        .replies(first_try_and_one_retry)
        .start();
    scenario.launch_claude_under(OUTSIDE_ANY_PANE, &[]);

    scenario.submit(PROMPT);

    scenario.wait_for_hook("StopFailure", TURN);
    let verdict = hook_verdicts(&scenario, 1, TURN).remove(0);
    assert!(
        verdict.contains("server_error outside any pane, nothing to type into"),
        "{verdict}"
    );
    expect_nothing_typed(&scenario, "");
}

fn request_timeout_is_continued_like_a_cut_off(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer)
        .env("API_TIMEOUT_MS", REQUEST_TIMEOUT_MS)
        .replies([Reply::Hang, Reply::Hang])
        .start();
    let pane = scenario.launch_claude();

    scenario.submit(PROMPT);

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "server_error");
    assert_eq!(
        failure.payload["last_assistant_message"],
        "Request timed out"
    );
    expect_one_continue(&scenario, &pane, &["stream", "stream"]);
}

fn expect_one_continue(scenario: &Scenario, pane: &Pane, failed_attempts: &[&str]) {
    let cutoff_message = scenario.nonstop_setting("agents.claude.cutoff_message");
    let verdict = hook_verdicts(scenario, 1, TURN).remove(0);
    assert!(
        verdict.contains(&format!("resume 1/3 written to {pane}")),
        "{verdict}"
    );

    scenario.wait_for_prompt_request(&cutoff_message, TURN);
    let notification = scenario.wait_for_notification("claude-nonstop: resumed", TURN);
    let stop = scenario.wait_for_hook("Stop", TURN);
    let counter = scenario
        .sandbox
        .cutoff_counter(&scenario.session_id())
        .expect("cut-off counter is written");
    let mut expected_prompts = vec![Some(PROMPT.to_owned()); failed_attempts.len()];
    expected_prompts.push(Some(cutoff_message));
    let mut expected_attempts = failed_attempts.to_vec();
    expected_attempts.push("stream");
    assert_eq!(
        prompts(scenario),
        expected_prompts,
        "the failed attempts, then exactly one continue"
    );
    assert_eq!(attempts(scenario), expected_attempts);
    assert_eq!(counter["count"], 1);
    assert!(notification.body.contains("resume 1/3"), "{notification:?}");
    assert_eq!(stop.payload["last_assistant_message"], "ok");
}

fn expect_nothing_typed(scenario: &Scenario, input_line: &str) {
    let prompts = prompts(scenario);
    let session_id = scenario.session_id();

    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.as_deref() == Some(PROMPT)),
        "only the user's prompt reaches the API: {prompts:?}"
    );
    assert_eq!(scenario.screen().input_line().as_deref(), Some(input_line));
    assert!(
        scenario.sandbox.cutoff_counter(&session_id).is_none(),
        "no continue is counted"
    );
    assert!(
        scenario.sandbox.notifications().is_empty(),
        "nobody is told about a continue that did not happen"
    );
}

fn hook_verdicts(scenario: &Scenario, count: usize, timeout: Duration) -> Vec<String> {
    scenario.wait_for(
        &format!("{count} verdict(s) of the StopFailure hook"),
        timeout,
        |scenario| {
            let verdicts: Vec<String> = scenario
                .sandbox
                .nonstop_log()
                .lines()
                .filter(|line| line.contains(" hook["))
                .map(str::to_owned)
                .collect();

            for verdict in &verdicts {
                assert!(
                    !verdict.contains("is busy with a turn"),
                    "KNOWN BUG 5: the hook types the moment the turn ends, can still see Claude Code's busy \
                     footer and gives up without a retry: {verdict}"
                );
            }

            (verdicts.len() >= count).then_some(verdicts)
        },
    )
}

fn prompts(scenario: &Scenario) -> Vec<Option<String>> {
    scenario
        .mock
        .main_loop_requests()
        .iter()
        .map(Request::last_user_text)
        .collect()
}

fn attempts(scenario: &Scenario) -> Vec<&'static str> {
    scenario
        .mock
        .main_loop_requests()
        .iter()
        .map(|request| {
            if request.is_stream() {
                "stream"
            } else {
                "fallback"
            }
        })
        .collect()
}

fn carries_tool_result(request: &Request) -> bool {
    request.body["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rfind(|message| message["role"] == "user"))
        .and_then(|message| message["content"].as_array())
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
}
