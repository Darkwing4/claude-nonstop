use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::error::{Error, Result};
use crate::paths::Paths;

pub fn show(paths: &Paths, lines: usize, follow: bool) -> Result<i32> {
    let path = paths.log_file();
    let exists = path
        .try_exists()
        .map_err(|source| Error::io(&path, "stat", source))?;

    if !exists {
        if !follow {
            return Ok(0);
        }
        crate::atomic_file::write(&path, b"", 0o600)?;
    }

    let mut tail = Command::new("tail");
    tail.arg("-n").arg(lines.to_string());
    if follow {
        tail.arg("-F");
    }
    tail.arg("--").arg(&path);

    let source = tail.exec();
    Err(Error::io("tail", "exec", source))
}
