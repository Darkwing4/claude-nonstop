use std::path::Path;

use claude_nonstop_e2e::mock_api::{LimitClaim, Reply};
use claude_nonstop_e2e::scenario::{Scenario, ScenarioBuilder, TURN, unix_now};

const PROMPT: &str = "say ok";
const INSTALLED_MAX_RETRIES: &str = "1";
const FIRST_TRY_AND_ONE_RETRY: usize = 2;
const LIMIT_RESET_IN_SECONDS: u64 = 3 * 60 * 60;

struct Failure {
    error: &'static str,
    message: &'static str,
    requests: Requests,
}

#[derive(Debug, PartialEq, Eq)]
struct Requests {
    streaming: usize,
    non_streaming: usize,
}

impl Requests {
    const fn streaming(count: usize) -> Requests {
        Requests {
            streaming: count,
            non_streaming: 0,
        }
    }

    const fn streaming_then_one_fallback(count: usize) -> Requests {
        Requests {
            streaming: count,
            non_streaming: 1,
        }
    }
}

#[test]
fn a2_server_error_tmux() {
    every_request_fails_with(
        Scenario::in_tmux(),
        Reply::server_error(),
        Failure {
            error: "server_error",
            message: "API Error: 500 Internal server error",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_overloaded_tmux() {
    every_request_fails_with(
        Scenario::in_tmux(),
        Reply::overloaded(),
        Failure {
            error: "server_error",
            message: "API Error: 529 Overloaded",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_rate_limited_api_key_tmux() {
    every_request_fails_with(
        Scenario::in_tmux(),
        Reply::status(
            429,
            "rate_limit_error",
            "Number of request tokens has exceeded your per-minute rate limit",
        ),
        Failure {
            error: "rate_limit",
            message: "API Error: Request rejected (429) · Number of request tokens",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_invalid_api_key_tmux() {
    every_request_fails_with(
        Scenario::in_tmux(),
        Reply::status(401, "authentication_error", "invalid x-api-key"),
        Failure {
            error: "authentication_failed",
            message: "Invalid API key · Fix external API key",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_forbidden_tmux() {
    every_request_fails_with(
        Scenario::in_tmux(),
        Reply::status(
            403,
            "permission_error",
            "Your API key does not have permission to use the specified resource.",
        ),
        Failure {
            error: "authentication_failed",
            message: "Please run /login · API Error: 403",
            requests: Requests::streaming(1),
        },
    );
}

#[test]
fn a2_connection_reset_tmux() {
    every_request_fails_with(
        Scenario::in_tmux(),
        Reply::ResetConnection,
        Failure {
            error: "server_error",
            message: "API Error: Connection dropped (ECONNRESET)",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_connection_refused_tmux() {
    let mut scenario = Scenario::in_tmux().start();
    scenario.launch_claude();
    scenario.mock.refuse_connections();

    turn_fails_with(
        &scenario,
        Failure {
            error: "server_error",
            message: "API Error: Connection refused",
            requests: Requests::streaming(0),
        },
    );
}

#[test]
fn a2_request_timeout_tmux() {
    every_request_fails_with(
        Scenario::in_tmux().env("API_TIMEOUT_MS", "5000"),
        Reply::Hang,
        Failure {
            error: "server_error",
            message: "Request timed out",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_stream_cut_and_fallback_reset_tmux() {
    every_request_fails_with(
        Scenario::in_tmux().replies([Reply::CutStream, Reply::CutStream]),
        Reply::ResetConnection,
        Failure {
            error: "server_error",
            message: "API Error: Connection dropped (ECONNRESET)",
            requests: Requests::streaming_then_one_fallback(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

#[test]
fn a2_stream_error_event_and_fallback_overloaded_tmux() {
    let overloaded_mid_stream = Reply::SseError {
        error_type: "overloaded_error".to_owned(),
    };

    every_request_fails_with(
        Scenario::in_tmux().replies([overloaded_mid_stream]),
        Reply::overloaded(),
        Failure {
            error: "server_error",
            message: "API Error: 529 Overloaded",
            requests: Requests::streaming_then_one_fallback(1),
        },
    );
}

#[test]
fn a2_session_limit_tmux() {
    every_request_fails_with(
        Scenario::in_tmux().subscription(),
        Reply::usage_limit(LimitClaim::FiveHour, unix_now() + LIMIT_RESET_IN_SECONDS),
        Failure {
            error: "rate_limit",
            message: "You've hit your session limit · resets ",
            requests: Requests::streaming(1),
        },
    );
}

#[test]
fn a2_weekly_limit_tmux() {
    every_request_fails_with(
        Scenario::in_tmux().subscription(),
        Reply::usage_limit(LimitClaim::SevenDay, unix_now() + LIMIT_RESET_IN_SECONDS),
        Failure {
            error: "rate_limit",
            message: "You've hit your weekly limit · resets ",
            requests: Requests::streaming(1),
        },
    );
}

#[test]
fn a2_capacity_limit_on_subscription_tmux() {
    every_request_fails_with(
        Scenario::in_tmux().subscription(),
        Reply::status(
            429,
            "rate_limit_error",
            "Number of request tokens has exceeded your per-minute rate limit",
        ),
        Failure {
            error: "rate_limit",
            message: "API Error: Server is temporarily limiting requests (not your usage limit)",
            requests: Requests::streaming(FIRST_TRY_AND_ONE_RETRY),
        },
    );
}

fn every_request_fails_with(builder: ScenarioBuilder, reply: Reply, expected: Failure) {
    let mut scenario = builder.fallback_reply(reply).start();
    scenario.launch_claude();

    turn_fails_with(&scenario, expected);
}

fn turn_fails_with(scenario: &Scenario, expected: Failure) {
    let session_id = scenario.session_id();
    let settings = scenario.sandbox.settings();
    assert_eq!(
        settings["env"]["CLAUDE_CODE_MAX_RETRIES"], INSTALLED_MAX_RETRIES,
        "request counts assume the retry limit claude-nonstop installs"
    );

    scenario.submit(PROMPT);

    let payload = scenario.wait_for_hook("StopFailure", TURN).payload;
    let message = payload["last_assistant_message"]
        .as_str()
        .unwrap_or_default();
    let transcript = payload["transcript_path"].as_str().unwrap_or_default();
    assert_eq!(payload["hook_event_name"], "StopFailure");
    assert_eq!(payload["session_id"], session_id.as_str());
    assert_eq!(
        payload["cwd"],
        scenario.sandbox.workdir.display().to_string()
    );
    assert!(
        Path::new(transcript).is_file(),
        "transcript_path names the session transcript: {payload}"
    );
    assert!(
        payload["prompt_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "prompt_id is set: {payload}"
    );
    assert!(
        payload.get("agent_id").is_none(),
        "the main session reports no agent_id: {payload}"
    );
    assert_eq!(payload["error"], expected.error, "{payload}");
    assert!(
        message.starts_with(expected.message),
        "last_assistant_message starts with {:?}: {message:?}",
        expected.message
    );
    assert_eq!(
        requests_for_prompt(scenario),
        expected.requests,
        "API requests made for the failed turn"
    );
}

fn requests_for_prompt(scenario: &Scenario) -> Requests {
    let (streaming, non_streaming): (Vec<_>, Vec<_>) = scenario
        .mock
        .main_loop_requests()
        .into_iter()
        .filter(|request| request.last_user_text().as_deref() == Some(PROMPT))
        .partition(|request| request.is_stream());

    Requests {
        streaming: streaming.len(),
        non_streaming: non_streaming.len(),
    }
}
