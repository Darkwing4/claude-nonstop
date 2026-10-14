use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::error::{Error, Result};

pub fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    match result {
        Ok(()) => Ok(()),
        Err(source) => {
            if let Err(cleanup) = fs::remove_file(&tmp) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "claude-nonstop: temp file {} not removed: {cleanup}",
                        tmp.display()
                    );
                }
            }
            Err(Error::io(path, "write", source))
        }
    }
}

pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io(path, "read", source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_with_mode_and_leaves_no_temp_file() {
        let dir =
            std::env::temp_dir().join(format!("claude-nonstop-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        write(&path, b"{}", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(read_optional(&path).unwrap(), Some(b"{}".to_vec()));
        assert_eq!(read_optional(&dir.join("missing")).unwrap(), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
