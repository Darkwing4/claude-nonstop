use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::harness_env::harness_env;
use crate::sandbox::Sandbox;
use crate::terminal::{Pane, Terminal};

const TAIL_LINES: usize = 25;

pub struct PreserveOnPanic<'a> {
    test: &'a str,
    sandbox: &'a Sandbox,
}

impl<'a> PreserveOnPanic<'a> {
    pub fn new(test: &'a str, sandbox: &'a Sandbox) -> Self {
        PreserveOnPanic { test, sandbox }
    }
}

impl Drop for PreserveOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            copy_and_report(self.test, self.sandbox, None);
        }
    }
}

pub fn preserve(test: &str, sandbox: &Sandbox, terminal: &Terminal, claude: Option<&Pane>) {
    let claude_tail = claude.map(|pane| save_screen(sandbox, terminal, pane));

    for listed in terminal.panes() {
        if Some(&listed.pane) != claude {
            save_screen(sandbox, terminal, &listed.pane);
        }
    }

    copy_and_report(test, sandbox, claude_tail.as_deref());
}

fn copy_and_report(test: &str, sandbox: &Sandbox, claude_tail: Option<&str>) {
    let destination = harness_env().artifacts_dir.join(test.replace("::", "-"));

    match fs::remove_dir_all(&destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "e2e: old artifacts {} not removed: {error}",
            destination.display()
        ),
    }

    if let Err(error) = copy_tree(&sandbox.root, &destination) {
        eprintln!(
            "e2e: artifacts not copied to {}: {error}",
            destination.display()
        );
    }

    report(&destination, sandbox, claude_tail);
}

fn save_screen(sandbox: &Sandbox, terminal: &Terminal, pane: &Pane) -> String {
    let file = sandbox
        .screens_dir()
        .join(format!("{}.txt", pane.file_stem()));
    let (text, tail) = match terminal.try_scrollback(pane) {
        Ok(screen) => (screen.text().to_owned(), screen.tail(TAIL_LINES)),
        Err(error) => {
            let note = format!("pane {pane} could not be captured: {error}");
            (note.clone(), note)
        }
    };

    if let Err(error) = fs::write(&file, text) {
        eprintln!("e2e: screen of {pane} not saved: {error}");
    }

    tail
}

fn report(destination: &Path, sandbox: &Sandbox, claude_tail: Option<&str>) {
    let log = sandbox.nonstop_log();
    let log_lines: Vec<&str> = log.lines().collect();
    let log_tail = log_lines[log_lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let relative = |path: PathBuf| {
        path.strip_prefix(&sandbox.root)
            .map(Path::to_path_buf)
            .unwrap_or(path)
    };
    let key_files = [
        sandbox.screens_dir(),
        sandbox.debug_log(),
        sandbox.mock_dir().join("requests.jsonl"),
        sandbox.hooks_dir(),
        sandbox.calls_dir(),
        sandbox.settings_path(),
        sandbox.ledger_path(),
        sandbox.nonstop_log_path(),
        sandbox.claude_config.join("projects"),
    ];

    eprintln!("\n=== e2e artifacts: {}", destination.display());
    for file in key_files.into_iter().filter(|file| file.exists()) {
        eprintln!("    {}", relative(file).display());
    }

    eprintln!(
        "--- claude pane, last {TAIL_LINES} lines\n{}",
        claude_tail.unwrap_or("(claude was not launched)")
    );
    eprintln!("--- claude-nonstop log, last {TAIL_LINES} lines\n{log_tail}\n===");
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;

    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());

        match copy_entry(&entry, &target) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            other => other?,
        }
    }

    Ok(())
}

fn copy_entry(entry: &fs::DirEntry, target: &Path) -> io::Result<()> {
    let kind = entry.file_type()?;

    if kind.is_dir() {
        copy_tree(&entry.path(), target)
    } else if kind.is_symlink() {
        symlink(fs::read_link(entry.path())?, target)
    } else if kind.is_file() {
        fs::copy(entry.path(), target).map(|_| ())
    } else {
        Ok(())
    }
}
