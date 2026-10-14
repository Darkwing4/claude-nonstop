use std::io::Write;

use crate::error::{Error, Result};
use crate::log_tail::LogTail;
use crate::paths::Paths;
use crate::platform::{Reactor, Wake};

pub fn show(paths: &Paths, lines: usize, follow: bool) -> Result<i32> {
    let path = paths.log_file();
    let text = match crate::atomic_file::read_optional(&path)? {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => String::new(),
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    let mut stdout = std::io::stdout().lock();
    for line in &all[start..] {
        writeln!(stdout, "{line}").map_err(|source| Error::io("stdout", "write", source))?;
    }
    if !follow {
        return Ok(0);
    }
    if !path.exists() {
        crate::atomic_file::write(&path, b"", 0o600)?;
    }
    let mut reactor = Reactor::new().map_err(|source| Error::io("reactor", "create", source))?;
    reactor
        .watch_file(&path)
        .map_err(|source| Error::io(&path, "watch", source))?;
    let mut tail = LogTail::from_end(&path).map_err(|source| Error::io(&path, "open", source))?;
    loop {
        let wakes = reactor
            .wait()
            .map_err(|source| Error::io("reactor", "wait", source))?;
        for wake in wakes {
            match wake {
                Wake::FileWritten(_) => {
                    for line in tail
                        .lines()
                        .map_err(|source| Error::io(&path, "read", source))?
                    {
                        writeln!(stdout, "{line}")
                            .map_err(|source| Error::io("stdout", "write", source))?;
                    }
                    stdout
                        .flush()
                        .map_err(|source| Error::io("stdout", "flush", source))?;
                }
                Wake::FileGone(_) => {
                    reactor
                        .watch_file(&path)
                        .map_err(|source| Error::io(&path, "watch", source))?;
                    tail = LogTail::from_end(&path)
                        .map_err(|source| Error::io(&path, "open", source))?;
                }
                _ => {}
            }
        }
    }
}
