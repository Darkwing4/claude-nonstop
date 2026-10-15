use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::atomic_file;
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::transcript_claude;

pub const TTL: Duration = Duration::from_secs(7 * 24 * 3600);
static SESSION_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f-]{8,64}$").expect("session id regex"));

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CounterState {
    pub count: u32,
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed { next: u32 },
    Exhausted { count: u32 },
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.state.join("cutoff")
}

fn path(paths: &Paths, session_id: &str) -> Result<PathBuf> {
    if !SESSION_ID.is_match(session_id) {
        return Err(Error::Invalid(format!(
            "session id {session_id:?} is not an id"
        )));
    }
    Ok(dir(paths).join(format!("{session_id}.json")))
}

pub fn load(paths: &Paths, session_id: &str) -> Result<Option<CounterState>> {
    let target = path(paths, session_id)?;
    let Some(bytes) = atomic_file::read_optional(&target)? else {
        return Ok(None);
    };
    match serde_json::from_slice::<CounterState>(&bytes) {
        Ok(state) => Ok(Some(state)),
        Err(error) => {
            crate::log::line(
                session_id,
                format!("cut-off counter unreadable, starting over: {error}"),
            );
            Ok(None)
        }
    }
}

pub fn evaluate(
    paths: &Paths,
    session_id: &str,
    transcript: &Path,
    prompts: &[&str],
    max: u32,
) -> Result<Decision> {
    let count = match load(paths, session_id)? {
        Some(state) => {
            let progressed = transcript_claude::progressed_since(transcript, state.offset, prompts)
                .map_err(|source| Error::io(transcript, "read", source))?;
            if progressed { 0 } else { state.count }
        }
        None => 0,
    };
    if count >= max {
        Ok(Decision::Exhausted { count })
    } else {
        Ok(Decision::Allowed { next: count + 1 })
    }
}

pub fn record(paths: &Paths, session_id: &str, transcript: &Path, count: u32) -> Result<()> {
    let target = path(paths, session_id)?;
    crate::paths::create_private_dir(&dir(paths))?;
    let offset = std::fs::metadata(transcript)
        .map(|meta| meta.len())
        .map_err(|source| Error::io(transcript, "stat", source))?;
    let state = CounterState { count, offset };
    let bytes = serde_json::to_vec(&state).map_err(|source| Error::Json {
        path: target.clone(),
        source,
    })?;
    atomic_file::write(&target, &bytes, 0o600)?;
    sweep(paths)
}

fn sweep(paths: &Paths) -> Result<()> {
    for entry in std::fs::read_dir(dir(paths))
        .map_err(|source| Error::io(dir(paths), "read", source))?
        .flatten()
    {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > TTL);
        if stale {
            if let Err(error) = std::fs::remove_file(entry.path()) {
                crate::log::line(
                    "",
                    format!(
                        "stale counter {} not removed: {error}",
                        entry.path().display()
                    ),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "claude-nonstop-counter-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Paths {
            home: dir.clone(),
            state: dir.clone(),
            config_file: dir.join("config.toml"),
            claude_config: dir.join(".claude"),
        }
    }

    #[test]
    fn counts_up_to_the_cap_and_resets_on_progress() {
        let paths = paths_in("cap");
        let transcript = paths.state.join("t.jsonl");
        let user = |text: &str| {
            serde_json::json!({"type":"user","message":{"content":text}}).to_string() + "\n"
        };
        std::fs::write(&transcript, user("do X")).unwrap();
        let prompts = ["continue please"];
        assert_eq!(
            evaluate(&paths, "abcdef12-1", &transcript, &prompts, 3).unwrap(),
            Decision::Allowed { next: 1 }
        );
        record(&paths, "abcdef12-1", &transcript, 1).unwrap();
        assert_eq!(
            evaluate(&paths, "abcdef12-1", &transcript, &prompts, 3).unwrap(),
            Decision::Allowed { next: 2 }
        );
        record(&paths, "abcdef12-1", &transcript, 2).unwrap();
        record(&paths, "abcdef12-1", &transcript, 3).unwrap();
        assert_eq!(
            evaluate(&paths, "abcdef12-1", &transcript, &prompts, 3).unwrap(),
            Decision::Exhausted { count: 3 }
        );
        let mut text = std::fs::read_to_string(&transcript).unwrap();
        text.push_str(&user("continue please"));
        text.push_str(&serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}).to_string());
        text.push('\n');
        std::fs::write(&transcript, text).unwrap();
        assert_eq!(
            evaluate(&paths, "abcdef12-1", &transcript, &prompts, 3).unwrap(),
            Decision::Allowed { next: 1 }
        );
        assert!(evaluate(&paths, "../escape", &transcript, &prompts, 3).is_err());
        std::fs::remove_dir_all(&paths.state).unwrap();
    }
}
