use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::paths::Paths;

#[derive(Clone, Debug, Deserialize)]
pub struct SessionEntry {
    #[serde(default)]
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "nameSource")]
    pub name_source: Option<String>,
}

pub fn by_session_id(paths: &Paths, session_id: &str) -> Result<Option<SessionEntry>> {
    let dir = paths.claude_sessions_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::io(&dir, "read", source)),
    };
    for entry in entries {
        let entry = entry.map_err(|source| Error::io(&dir, "read", source))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_slice::<SessionEntry>(&bytes) {
            if parsed.session_id == session_id {
                return Ok(Some(parsed));
            }
        }
    }
    Ok(None)
}

pub fn transcript_path(paths: &Paths, session_id: &str) -> Option<PathBuf> {
    let projects = paths.claude_projects_dir();
    let entries = fs::read_dir(&projects).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn display_name(paths: &Paths, session_id: &str, fallback: &str) -> String {
    let mut derived = None;
    if let Ok(Some(entry)) = by_session_id(paths, session_id) {
        if let Some(name) = entry.name.filter(|name| !name.is_empty()) {
            if entry.name_source.as_deref() == Some("derived") {
                derived = Some(name);
            } else {
                return name;
            }
        }
    }
    if let Some(path) = transcript_path(paths, session_id) {
        if let Some(title) = transcript_title(&path) {
            return title;
        }
    }
    derived.unwrap_or_else(|| fallback.to_owned())
}

fn transcript_title(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut custom = None;
    let mut ai = None;
    for line in BufReader::new(file).split(b'\n').flatten() {
        let has_custom = line.windows(14).any(|window| window == b"\"custom-title\"");
        let has_ai = line.windows(10).any(|window| window == b"\"ai-title\"");
        if !has_custom && !has_ai {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        match record.get("type").and_then(serde_json::Value::as_str) {
            Some("custom-title") => {
                custom = record
                    .get("customTitle")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .or(custom)
            }
            Some("ai-title") => {
                ai = record
                    .get("aiTitle")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .or(ai)
            }
            _ => {}
        }
    }
    custom
        .filter(|title| !title.is_empty())
        .or(ai.filter(|title| !title.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "claude-nonstop-sessions-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join(".claude/sessions")).unwrap();
        fs::create_dir_all(dir.join(".claude/projects/-home-me-proj")).unwrap();
        Paths {
            home: dir.clone(),
            state: dir.join("state"),
            config_file: dir.join("config.toml"),
            claude_config: dir.join(".claude"),
        }
    }

    #[test]
    fn pid_lookup_and_name_resolution_order() {
        let paths = paths_in("names");
        let sessions = paths.claude_sessions_dir();
        fs::write(sessions.join("100.json"), r#"{"pid":100,"sessionId":"s-custom","cwd":"/home/me/proj","name":"My task","nameSource":"custom"}"#).unwrap();
        fs::write(sessions.join("101.json"), r#"{"pid":101,"sessionId":"s-derived","cwd":"/home/me/proj","name":"proj-1a","nameSource":"derived"}"#).unwrap();
        fs::write(sessions.join("102.key"), "not json").unwrap();
        let entry = by_session_id(&paths, "s-custom").unwrap().unwrap();
        assert_eq!(entry.pid, 100);
        assert!(by_session_id(&paths, "s-missing").unwrap().is_none());
        assert_eq!(display_name(&paths, "s-custom", "pane 7"), "My task");
        let transcript = paths
            .claude_projects_dir()
            .join("-home-me-proj/s-derived.jsonl");
        fs::write(&transcript, "{\"type\":\"user\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Model title\"}\n{\"type\":\"custom-title\",\"customTitle\":\"Mine\"}\n").unwrap();
        assert_eq!(
            transcript_path(&paths, "s-derived"),
            Some(transcript.clone())
        );
        assert_eq!(display_name(&paths, "s-derived", "pane 7"), "Mine");
        fs::write(
            &transcript,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Model title\"}\n",
        )
        .unwrap();
        assert_eq!(display_name(&paths, "s-derived", "pane 7"), "Model title");
        fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(display_name(&paths, "s-derived", "pane 7"), "proj-1a");
        assert_eq!(display_name(&paths, "s-unknown", "pane 7"), "pane 7");
        fs::remove_dir_all(&paths.home).unwrap();
    }
}
