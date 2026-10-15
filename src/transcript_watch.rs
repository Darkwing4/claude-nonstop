use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::log;
use crate::paths::Paths;
use crate::platform::{Reactor, SourceId, Wake};
use crate::transcript_claude::{self, LimitRecord};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitSeen {
    pub session_id: String,
    pub cwd: PathBuf,
    pub transcript: PathBuf,
    pub record: LimitRecord,
}

pub struct TranscriptWatch {
    overload: crate::config::Overload,
    root: PathBuf,
    root_source: SourceId,
    dirs: BTreeMap<SourceId, PathBuf>,
    offsets: BTreeMap<PathBuf, u64>,
}

impl TranscriptWatch {
    pub fn start(
        reactor: &mut Reactor,
        paths: &Paths,
        overload: &crate::config::Overload,
    ) -> Result<TranscriptWatch> {
        let root = paths.claude_projects_dir();
        std::fs::create_dir_all(&root).map_err(|source| Error::io(&root, "create", source))?;
        let root_source = reactor
            .watch_dir(&root)
            .map_err(|source| Error::io(&root, "watch", source))?;
        let mut watch = TranscriptWatch {
            overload: overload.clone(),
            root: root.clone(),
            root_source,
            dirs: BTreeMap::new(),
            offsets: BTreeMap::new(),
        };
        for entry in std::fs::read_dir(&root)
            .map_err(|source| Error::io(&root, "read", source))?
            .flatten()
        {
            if entry.path().is_dir() {
                watch.add_dir(reactor, &entry.path());
            }
        }
        log::line(
            "",
            format!(
                "watching {} transcript directories under {}",
                watch.dirs.len(),
                root.display()
            ),
        );
        Ok(watch)
    }

    fn add_dir(&mut self, reactor: &mut Reactor, dir: &Path) {
        match reactor.watch_dir(dir) {
            Ok(source) => {
                for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if is_transcript(&entry.file_name()) {
                        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                        self.offsets.insert(path, size);
                    }
                }
                self.dirs.insert(source, dir.to_path_buf());
            }
            Err(error) => log::line(
                "",
                format!(
                    "transcript directory {} not watched: {error}",
                    dir.display()
                ),
            ),
        }
    }

    pub fn on_wake(&mut self, wake: &Wake, reactor: &mut Reactor) -> Result<Vec<LimitSeen>> {
        let Wake::DirEntry(source, name) = wake else {
            return Ok(Vec::new());
        };
        if *source == self.root_source {
            let candidate = self.root.join(name);
            if candidate.is_dir() && !self.dirs.values().any(|dir| *dir == candidate) {
                self.add_dir(reactor, &candidate);
            }
            return Ok(Vec::new());
        }
        let Some(dir) = self.dirs.get(source).cloned() else {
            return Ok(Vec::new());
        };
        if !is_transcript(name) {
            return Ok(Vec::new());
        }
        let path = dir.join(name);
        self.read_new_records(&path)
    }

    fn read_new_records(&mut self, path: &Path) -> Result<Vec<LimitSeen>> {
        let size = match std::fs::metadata(path) {
            Ok(meta) => meta.len(),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                self.offsets.remove(path);
                return Ok(Vec::new());
            }
            Err(source) => return Err(Error::io(path, "stat", source)),
        };
        let offset = self.offsets.get(path).copied().unwrap_or(0);
        let start = if size < offset { 0 } else { offset };
        let bytes = read_range(path, start, size)?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        self.offsets
            .insert(path.to_path_buf(), start + complete as u64);
        let mut seen = Vec::new();
        for raw in bytes[..complete].split(|byte| *byte == b'\n') {
            let Some((record, limit)) = transcript_claude::parse_limit_line(raw, &self.overload)
            else {
                continue;
            };
            let session_id = record
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            let cwd = record
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default();
            seen.push(LimitSeen {
                session_id,
                cwd,
                transcript: path.to_path_buf(),
                record: limit,
            });
        }
        Ok(seen)
    }
}

fn is_transcript(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext == "jsonl")
}

fn read_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|source| Error::io(path, "open", source))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|source| Error::io(path, "seek", source))?;
    let mut bytes = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut bytes)
        .map_err(|source| Error::io(path, "read", source))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn limit_line(stamp: &str) -> String {
        serde_json::json!({"type":"assistant","timestamp":stamp,"sessionId":"s-1","cwd":"/home/me/proj","isApiErrorMessage":true,"error":"rate_limit","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 5:30pm (Asia/Bangkok)"}]}}).to_string() + "\n"
    }

    #[test]
    fn only_records_written_after_start_are_reported() {
        let paths = Paths::temp("twatch-new");
        let dir = paths.claude_projects_dir().join("-home-me-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("s-1.jsonl");
        std::fs::write(&transcript, limit_line("2026-09-03T06:00:00.000Z")).unwrap();
        let mut reactor = Reactor::new().unwrap();
        let mut watch =
            TranscriptWatch::start(&mut reactor, &paths, &crate::config::Overload::default())
                .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        file.write_all(limit_line("2026-09-03T06:43:54.740Z").as_bytes())
            .unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            for wake in reactor.wait().unwrap() {
                seen.extend(watch.on_wake(&wake, &mut reactor).unwrap());
            }
            if !seen.is_empty() {
                break;
            }
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].session_id, "s-1");
        assert_eq!(seen[0].cwd, PathBuf::from("/home/me/proj"));
        assert_eq!(seen[0].record.at, 1788417834);
        let new_dir = paths.claude_projects_dir().join("-home-me-other");
        std::fs::create_dir(&new_dir).unwrap();
        for wake in reactor.wait().unwrap() {
            watch.on_wake(&wake, &mut reactor).unwrap();
        }
        assert!(watch.dirs.values().any(|dir| *dir == new_dir));
        std::fs::remove_dir_all(&paths.home).unwrap();
    }
}
