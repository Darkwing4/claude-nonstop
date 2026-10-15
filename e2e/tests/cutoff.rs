use claude_nonstop_e2e::mock_api::Reply;
use claude_nonstop_e2e::scenario::{Scenario, TURN};
use claude_nonstop_e2e::terminal::Multiplexer;

#[test]
fn b1_turn_cut_off_by_server_errors_is_continued_tmux() {
    turn_cut_off_by_server_errors_is_continued(Multiplexer::Tmux);
}

#[test]
fn b1_turn_cut_off_by_server_errors_is_continued_zellij() {
    turn_cut_off_by_server_errors_is_continued(Multiplexer::Zellij);
}

fn turn_cut_off_by_server_errors_is_continued(multiplexer: Multiplexer) {
    let first_try_and_one_retry = [Reply::server_error(), Reply::server_error()];
    let mut scenario = Scenario::builder(multiplexer)
        .replies(first_try_and_one_retry)
        .start();
    let pane = scenario.launch_claude();
    let cutoff_message = scenario.nonstop_setting("agents.claude.cutoff_message");

    scenario.submit("say ok");

    let failure = scenario.wait_for_hook("StopFailure", TURN);
    assert_eq!(failure.payload["error"], "server_error");

    let verdict =
        scenario.wait_for_log_line("the hook's verdict on the cut-off turn", TURN, |line| {
            line.contains("turn cut off (server_error)")
        });
    assert!(
        !verdict.contains("is busy with a turn"),
        "PRODUCT BUG (found by e2e): the hook types the moment the turn ends, can still see Claude Code's \
         busy footer and gives up without a retry: {verdict}"
    );
    assert!(
        verdict.contains(&format!("resume 1/3 written to {pane}")),
        "{verdict}"
    );

    scenario.wait_for_prompt_request(&cutoff_message, TURN);
    let session_id = scenario.session_id();
    let counter = scenario
        .sandbox
        .cutoff_counter(&session_id)
        .expect("cut-off counter is written");
    let notification = scenario.wait_for_notification("claude-nonstop: resumed", TURN);
    let prompts: Vec<Option<String>> = scenario
        .mock
        .main_loop_requests()
        .iter()
        .map(|request| request.last_user_text())
        .collect();
    assert_eq!(
        prompts,
        [
            Some("say ok".to_owned()),
            Some("say ok".to_owned()),
            Some(cutoff_message)
        ],
        "two failed attempts, then exactly one continue"
    );
    assert_eq!(counter["count"], 1);
    assert!(notification.body.contains("resume 1/3"), "{notification:?}");

    let stop = scenario.wait_for_hook("Stop", TURN);
    assert_eq!(stop.payload["last_assistant_message"], "ok");
}
