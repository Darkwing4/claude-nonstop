use std::io::Read;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::ledger::{AgentId, DetectedVia};
use crate::log;
use crate::mux::PaneAddress;
use crate::paths::Paths;
use crate::recover;

pub static NOT_RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)run /login|not logged in|login expired|organization has disabled|credit balance is too low|invalid authentication|safeguards flagged|design-login").expect("not retryable regex")
});
static UNREACHABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)connection error|connection refused|econnrefused|econnreset|etimedout|enotfound|fetch failed|socket hang up").expect("unreachable regex")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    StreamCut,
    Overloaded,
    RateLimit,
    NotRetryable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Payload {
    pub hook_event_name: Option<String>,
    pub session_id: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub error: Option<String>,
    pub last_assistant_message: Option<String>,
    pub agent_id: Option<String>,
}

pub fn classify(
    error: &str,
    message: Option<&str>,
    overload: &crate::config::Overload,
) -> HookKind {
    if matches!(
        error,
        "server_error" | "overloaded" | "rate_limit" | "unknown"
    ) {
        if message.is_some_and(|text| NOT_RETRYABLE.is_match(text)) {
            return HookKind::NotRetryable;
        }

        if message.is_some_and(|text| overload.matches_message(text)) {
            return HookKind::Overloaded;
        }
    }

    if matches!(error, "server_error" | "unknown")
        && message.is_some_and(|text| UNREACHABLE.is_match(text))
    {
        return HookKind::Overloaded;
    }

    match error {
        "server_error" => HookKind::StreamCut,
        "overloaded" => HookKind::Overloaded,
        "rate_limit" => HookKind::RateLimit,
        "unknown" => HookKind::StreamCut,
        _ => HookKind::NotRetryable,
    }
}

pub fn run(paths: &Paths, config: &Config, input: &mut dyn Read) -> Result<i32> {
    let mut raw = String::new();
    if let Err(error) = input.read_to_string(&mut raw) {
        log::line("", format!("hook: stdin unreadable: {error}"));
        return Ok(0);
    }

    let payload: Payload = match serde_json::from_str(&raw) {
        Ok(payload) => payload,
        Err(error) => {
            log::line("", format!("hook: payload is not JSON: {error}"));
            return Ok(0);
        }
    };

    if payload.hook_event_name.as_deref() != Some("StopFailure")
        || payload.agent_id.is_some()
        || !config.agents.claude.enabled
    {
        return Ok(0);
    }

    let session_id = payload.session_id.clone().unwrap_or_default();
    let error = payload
        .error
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let kind = classify(
        &error,
        payload.last_assistant_message.as_deref(),
        &config.overload,
    );

    let Some(pane) = PaneAddress::from_env(&std::env::vars().collect()) else {
        log::line(
            &session_id,
            format!("hook: {error} outside any pane, nothing to type into"),
        );

        if kind == HookKind::RateLimit {
            record_limit(paths, config, &payload, &session_id, None)?;
        }

        return Ok(0);
    };

    let transcript = payload.transcript_path.clone().unwrap_or_default();
    let cwd = payload.cwd.clone().unwrap_or_default();

    match kind {
        HookKind::StreamCut => {
            if !transcript.is_file() {
                log::line(
                    &session_id,
                    format!(
                        "hook: {error} but transcript {} is missing, not resumed",
                        transcript.display()
                    ),
                );
                return Ok(0);
            }

            let context = recover::Context {
                paths,
                config,
                agent: AgentId::Claude,
                session_id: &session_id,
                transcript: &transcript,
                pane: &pane,
            };
            context.stream_cut(&error)?;
        }
        HookKind::Overloaded => {
            let report = recover::OverloadReport {
                session_id: &session_id,
                cwd: &cwd,
                pane: &pane,
                transcript: &transcript,
                error: &error,
            };
            recover::schedule_overload(paths, config, &report)?;
        }
        HookKind::RateLimit => record_limit(paths, config, &payload, &session_id, Some(&pane))?,
        HookKind::NotRetryable => log::line(
            &session_id,
            format!("hook: {error} is not something a retry fixes, leaving it to the user"),
        ),
    }

    Ok(0)
}

fn record_limit(
    paths: &Paths,
    config: &Config,
    payload: &Payload,
    session_id: &str,
    pane: Option<&PaneAddress>,
) -> Result<()> {
    if session_id.is_empty() {
        return Err(Error::Invalid(
            "hook: rate_limit without a session id".to_owned(),
        ));
    }

    let cwd = payload.cwd.clone().unwrap_or_default();

    recover::record_limit(
        paths,
        config,
        session_id,
        &cwd,
        pane,
        DetectedVia::Hook,
        payload.transcript_path.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(error: &str, message: Option<&str>) -> HookKind {
        super::classify(error, message, &crate::config::Overload::default())
    }

    #[test]
    fn custom_capacity_errors_are_overloads() {
        let message = "API Error: Server is temporarily limiting requests (not your usage limit) · only reserved subscriptions are left and their shared part is used up";
        for error in ["rate_limit", "unknown", "server_error", "overloaded"] {
            assert_eq!(classify(error, Some(message)), HookKind::Overloaded);
        }

        let mut overload = crate::config::Overload::default();
        overload
            .message_contains
            .push("upstream pool exhausted".to_owned());

        assert_eq!(
            super::classify("rate_limit", Some("UPSTREAM POOL EXHAUSTED"), &overload),
            HookKind::Overloaded
        );
        assert_eq!(
            super::classify("authentication_failed", Some(message), &overload),
            HookKind::NotRetryable
        );
        assert_eq!(
            super::classify(
                "unknown",
                Some("upstream pool exhausted; please run /login"),
                &overload
            ),
            HookKind::NotRetryable
        );
    }

    #[test]
    fn errors_classify_by_kind_and_message() {
        assert_eq!(classify("server_error", None), HookKind::StreamCut);
        assert_eq!(classify("overloaded", None), HookKind::Overloaded);
        assert_eq!(classify("rate_limit", None), HookKind::RateLimit);
        assert_eq!(
            classify("unknown", Some("API Error: Stream idle timeout")),
            HookKind::StreamCut
        );
        assert_eq!(
            classify("unknown", Some("Please run /login")),
            HookKind::NotRetryable
        );
        assert_eq!(
            classify("authentication_failed", None),
            HookKind::NotRetryable
        );
        assert_eq!(
            classify("invalid_request", Some("anything")),
            HookKind::NotRetryable
        );
    }

    #[test]
    fn unreachable_api_waits_like_an_overload() {
        let refused =
            "API Error: Connection refused — a firewall or proxy may be blocking it (ECONNREFUSED)";
        assert_eq!(
            classify("server_error", Some(refused)),
            HookKind::Overloaded
        );
        assert_eq!(
            classify("unknown", Some("API Error: Connection error.")),
            HookKind::Overloaded
        );
        assert_eq!(classify("rate_limit", Some(refused)), HookKind::RateLimit);
    }

    #[test]
    fn other_events_and_subagents_are_ignored() {
        let paths = Paths::temp("hook-run");
        let config = Config::default();

        let mut stop = r#"{"hook_event_name":"Stop","error":"server_error"}"#.as_bytes();
        assert_eq!(run(&paths, &config, &mut stop).unwrap(), 0);

        let mut sub = r#"{"hook_event_name":"StopFailure","error":"server_error","agent_id":"a1"}"#
            .as_bytes();
        assert_eq!(run(&paths, &config, &mut sub).unwrap(), 0);

        let mut broken = "{".as_bytes();
        assert_eq!(run(&paths, &config, &mut broken).unwrap(), 0);
        assert!(!paths.state.exists());

        std::fs::remove_dir_all(&paths.home).unwrap();
    }
}
