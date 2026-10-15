use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const ROTATE_AT: u64 = 5 * 1024 * 1024;
const KEEP_GENERATIONS: u32 = 3;

struct Logger {
    path: PathBuf,
    component: &'static str,
    mirror_stderr: bool,
}

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);

pub fn init(path: PathBuf, component: &'static str, mirror_stderr: bool) {
    let mut slot = LOGGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(Logger {
        path,
        component,
        mirror_stderr,
    });
}

pub fn line(session: &str, message: impl AsRef<str>) {
    let message = message.as_ref();
    let slot = LOGGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let Some(logger) = slot.as_ref() else {
        eprintln!("claude-nonstop: {message}");
        return;
    };

    let short = if session.is_empty() {
        "-"
    } else {
        session.get(..8).unwrap_or(session)
    };

    let stamp = jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%:z");
    let text = format!(
        "{stamp} {}[{}] {short} {message}\n",
        logger.component,
        std::process::id()
    );

    if logger.mirror_stderr {
        eprintln!("claude-nonstop: {message}");
    }

    if let Err(error) = append(&logger.path, &text) {
        eprintln!(
            "claude-nonstop: log not written to {}: {error}",
            logger.path.display()
        );
    }
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    rotate_if_large(path)?;
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;

    file.write_all(text.as_bytes())
}

fn rotate_if_large(path: &Path) -> std::io::Result<()> {
    let size = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if size < ROTATE_AT {
        return Ok(());
    }

    for generation in (1..KEEP_GENERATIONS).rev() {
        let from = numbered(path, generation);
        if from.exists() {
            fs::rename(&from, numbered(path, generation + 1))?;
        }
    }

    fs::rename(path, numbered(path, 1))?;

    File::create(path).map(|_| ())
}

fn numbered(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{generation}"));

    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_when_large_and_keeps_three_generations() {
        let dir = tempdir();
        let path = dir.join("test.log");
        fs::write(&path, vec![b'x'; ROTATE_AT as usize]).unwrap();

        append(&path, "one\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
        assert_eq!(fs::metadata(numbered(&path, 1)).unwrap().len(), ROTATE_AT);

        fs::write(&path, vec![b'y'; ROTATE_AT as usize]).unwrap();
        append(&path, "two\n").unwrap();
        fs::write(&path, vec![b'z'; ROTATE_AT as usize]).unwrap();
        append(&path, "three\n").unwrap();

        assert!(numbered(&path, 3).exists());
        assert!(!numbered(&path, 4).exists());
        assert_eq!(fs::read(numbered(&path, 3)).unwrap()[0], b'x');

        fs::remove_dir_all(dir).unwrap();
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("claude-nonstop-log-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        dir
    }
}
