use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

use crate::atomic_file;
use crate::error::{Error, Result};
pub use crate::mux::PaneAddress;
use crate::paths::Paths;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Ledger {
    pub version: u32,
    pub seq: u64,
    pub resumer_pid: Option<u32>,
    pub sessions: BTreeMap<String, SessionRecord>,
    pub prompts: Vec<QueuedPrompt>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentId {
    Claude,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Stopped,
    Resuming,
    Resumed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopKind {
    StreamCut,
    Overload,
    UsageLimit,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitType {
    FiveHour,
    Weekly,
    Model,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DetectedVia {
    Hook,
    Transcript,
    Screen,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ResetSource {
    Fallback,
    Relative,
    Absolute,
    Manual,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumedBy {
    Nonstop,
    Native,
    User,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionRecord {
    pub key: String,
    pub agent: AgentId,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub pane: Option<PaneAddress>,
    pub mux_session: Option<String>,
    pub status: Status,
    pub stop: StopKind,
    pub limit: LimitType,
    pub detected_via: DetectedVia,
    pub detected_at: u64,
    pub banner_at: Option<u64>,
    pub reset_at: u64,
    pub reset_source: ResetSource,
    pub attempts: u32,
    pub probe_count: u32,
    pub last_attempt_at: Option<u64>,
    pub last_error: Option<String>,
    pub resume_episode_at: Option<u64>,
    pub resumed_by: Option<ResumedBy>,
    pub resume_message: Option<String>,
    pub manual: bool,
    pub env: BTreeMap<String, String>,
}

impl Default for SessionRecord {
    fn default() -> Self {
        SessionRecord {
            key: String::new(),
            agent: AgentId::Claude,
            session_id: None,
            cwd: PathBuf::new(),
            name: None,
            pane: None,
            mux_session: None,
            status: Status::Stopped,
            stop: StopKind::UsageLimit,
            limit: LimitType::Unknown,
            detected_via: DetectedVia::Hook,
            detected_at: 0,
            banner_at: None,
            reset_at: 0,
            reset_source: ResetSource::Fallback,
            attempts: 0,
            probe_count: 0,
            last_attempt_at: None,
            last_error: None,
            resume_episode_at: None,
            resumed_by: None,
            resume_message: None,
            manual: false,
            env: BTreeMap::new(),
        }
    }
}

impl SessionRecord {
    pub fn episode_at(&self) -> u64 {
        self.banner_at.unwrap_or(self.detected_at)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.status, Status::Stopped | Status::Resuming)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PromptWhen {
    NextReset,
    At,
    Now,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptStatus {
    Pending,
    Launching,
    Delivered,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct QueuedPrompt {
    pub id: String,
    pub agent: AgentId,
    pub cwd: PathBuf,
    pub prompt: String,
    pub when: PromptWhen,
    pub at: Option<u64>,
    pub status: PromptStatus,
    pub attempts: u32,
    pub not_before: Option<u64>,
    pub created_at: u64,
    pub launched_at: Option<u64>,
    pub sent_at: Option<u64>,
    pub delivered_at: Option<u64>,
    pub pane: Option<PaneAddress>,
    pub mux_session: Option<String>,
    pub last_error: Option<String>,
}

impl Default for QueuedPrompt {
    fn default() -> Self {
        QueuedPrompt {
            id: String::new(),
            agent: AgentId::Claude,
            cwd: PathBuf::new(),
            prompt: String::new(),
            when: PromptWhen::NextReset,
            at: None,
            status: PromptStatus::Pending,
            attempts: 0,
            not_before: None,
            created_at: 0,
            launched_at: None,
            sent_at: None,
            delivered_at: None,
            pane: None,
            mux_session: None,
            last_error: None,
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub fn read(paths: &Paths) -> Result<Ledger> {
    let path = paths.ledger();
    let Some(bytes) = atomic_file::read_optional(&path)? else {
        return Ok(Ledger {
            version: VERSION,
            ..Ledger::default()
        });
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return quarantine(paths, error.to_string()),
    };
    let sessions = value.get("sessions").and_then(serde_json::Value::as_object);
    let prompts = value.get("prompts").and_then(serde_json::Value::as_array);
    if sessions
        .into_iter()
        .flat_map(|items| items.values())
        .chain(prompts.into_iter().flatten())
        .any(|item| item.get("agent").and_then(serde_json::Value::as_str) == Some("codex"))
    {
        return Err(Error::Invalid(format!(
            "{} contains unsupported Codex records; state left unchanged",
            path.display()
        )));
    }
    match serde_json::from_value::<Ledger>(value) {
        Ok(ledger) if ledger.version == VERSION => Ok(ledger),
        Ok(ledger) => quarantine(
            paths,
            format!("version {} is not {VERSION}", ledger.version),
        ),
        Err(error) => quarantine(paths, error.to_string()),
    }
}

fn quarantine(paths: &Paths, reason: String) -> Result<Ledger> {
    let path = paths.ledger();
    let aside = paths
        .state
        .join(format!("ledger.json.corrupt.{}", now_unix()));
    std::fs::rename(&path, &aside).map_err(|source| Error::io(&path, "quarantine", source))?;
    crate::log::line(
        "",
        format!(
            "ledger {} set aside as {}: {reason}",
            path.display(),
            aside.display()
        ),
    );
    Ok(Ledger {
        version: VERSION,
        ..Ledger::default()
    })
}

pub fn update<T>(paths: &Paths, change: impl FnOnce(&mut Ledger) -> T) -> Result<T> {
    let lock_path = paths.ledger_lock();
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::io(&lock_path, "open", source))?;
    flock(&lock, FlockOperation::LockExclusive)
        .map_err(|source| Error::io(&lock_path, "lock", source.into()))?;
    let mut ledger = read(paths)?;
    let outcome = change(&mut ledger);
    ledger.seq += 1;
    ledger.version = VERSION;
    let bytes = serde_json::to_vec_pretty(&ledger).map_err(|source| Error::Json {
        path: paths.ledger(),
        source,
    })?;
    atomic_file::write(&paths.ledger(), &bytes, 0o600)?;
    Ok(outcome)
}

pub fn cas(
    paths: &Paths,
    key: &str,
    expect: &[Status],
    episode_at: Option<u64>,
    change: impl FnOnce(&mut SessionRecord),
) -> Result<bool> {
    update(paths, |ledger| {
        let Some(record) = ledger.sessions.get_mut(key) else {
            return false;
        };
        if !expect.contains(&record.status) {
            return false;
        }
        if episode_at.is_some_and(|expected| expected != record.episode_at()) {
            return false;
        }
        change(record);
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "claude-nonstop-ledger-{name}-{}",
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
    fn legacy_codex_state_is_rejected_without_quarantine_or_overwrite() {
        let paths = paths_in("legacy-codex");
        for value in [
            serde_json::json!({"version": VERSION, "sessions": {"old": {"agent": "codex"}}}),
            serde_json::json!({"version": VERSION, "prompts": [{"agent": "codex"}]}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            std::fs::write(paths.ledger(), &bytes).unwrap();
            let error = update(&paths, |book| book.sessions.clear()).unwrap_err();
            assert!(error.to_string().contains("unsupported Codex records"));
            assert_eq!(std::fs::read(paths.ledger()).unwrap(), bytes);
            assert!(!std::fs::read_dir(&paths.state).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("corrupt")
            }));
        }
        std::fs::remove_dir_all(&paths.state).unwrap();
    }

    #[test]
    fn update_persists_and_bumps_seq() {
        let paths = paths_in("update");
        let first = update(&paths, |ledger| {
            ledger.sessions.insert(
                "s1".to_owned(),
                SessionRecord {
                    key: "s1".to_owned(),
                    detected_at: 10,
                    ..SessionRecord::default()
                },
            );
            ledger.seq
        })
        .unwrap();
        assert_eq!(first, 0);
        let ledger = read(&paths).unwrap();
        assert_eq!(ledger.seq, 1);
        assert_eq!(ledger.sessions["s1"].detected_at, 10);
        std::fs::remove_dir_all(&paths.state).unwrap();
    }

    #[test]
    fn cas_rejects_wrong_status_or_episode() {
        let paths = paths_in("cas");
        update(&paths, |ledger| {
            ledger.sessions.insert(
                "s1".to_owned(),
                SessionRecord {
                    key: "s1".to_owned(),
                    detected_at: 10,
                    banner_at: Some(20),
                    ..SessionRecord::default()
                },
            );
        })
        .unwrap();
        assert!(!cas(&paths, "s1", &[Status::Resuming], None, |_| {}).unwrap());
        assert!(!cas(&paths, "s1", &[Status::Stopped], Some(10), |_| {}).unwrap());
        assert!(
            cas(&paths, "s1", &[Status::Stopped], Some(20), |record| {
                record.status = Status::Resuming
            })
            .unwrap()
        );
        assert_eq!(
            read(&paths).unwrap().sessions["s1"].status,
            Status::Resuming
        );
        assert!(!cas(&paths, "missing", &[Status::Stopped], None, |_| {}).unwrap());
        std::fs::remove_dir_all(&paths.state).unwrap();
    }

    #[test]
    fn corrupt_ledger_is_set_aside_not_fatal() {
        let paths = paths_in("corrupt");
        std::fs::write(paths.ledger(), b"{not json").unwrap();
        let ledger = read(&paths).unwrap();
        assert!(ledger.sessions.is_empty());
        assert!(std::fs::read_dir(&paths.state).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("ledger.json.corrupt.")
        }));
        std::fs::remove_dir_all(&paths.state).unwrap();
    }
}
