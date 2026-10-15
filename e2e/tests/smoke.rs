use claude_nonstop_e2e::scenario::{Scenario, TURN};
use claude_nonstop_e2e::terminal::Multiplexer;

#[test]
fn s0_smoke_tmux() {
    claude_answers_and_nonstop_stays_idle(Multiplexer::Tmux);
}

#[test]
fn s0_smoke_zellij() {
    claude_answers_and_nonstop_stays_idle(Multiplexer::Zellij);
}

fn claude_answers_and_nonstop_stays_idle(multiplexer: Multiplexer) {
    let mut scenario = Scenario::builder(multiplexer).start();
    scenario.launch_claude();

    scenario.submit("say ok");

    let stop = scenario.wait_for_hook("Stop", TURN);
    let requests = scenario.mock.main_loop_requests();
    assert_eq!(stop.payload["last_assistant_message"], "ok");
    assert_eq!(requests.len(), 1, "one prompt makes one API request");
    assert_eq!(requests[0].last_user_text().as_deref(), Some("say ok"));
    assert!(
        scenario.sandbox.hooks_of("StopFailure").is_empty(),
        "a successful turn does not fail"
    );
    assert_eq!(
        scenario.sandbox.nonstop_log(),
        "",
        "claude-nonstop has nothing to do"
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
