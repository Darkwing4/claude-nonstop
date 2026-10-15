use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use super::{Key, Multiplexer, PaneAddress};
use crate::error::{Error, Result};
use crate::platform::process::{CommandOutcome, run_with_timeout};

const ACTION_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Zellij;

#[derive(Deserialize)]
struct PaneRow {
    id: u32,
    is_plugin: bool,
    exited: bool,
    pane_command: Option<String>,
}

impl PaneRow {
    fn foreground_command(self) -> Option<String> {
        let command = self.pane_command?;
        let program = command.split_whitespace().next()?;

        Some(program.rsplit('/').next().unwrap_or(program).to_owned())
    }
}

impl Zellij {
    fn action(session: &str, args: &[&str]) -> Result<String> {
        let mut command = Command::new("zellij");
        command
            .arg("--session")
            .arg(session)
            .arg("action")
            .args(args);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ZELLIJ") {
                command.env_remove(name);
            }
        }
        let outcome = run_with_timeout(&mut command, None, ACTION_TIMEOUT)
            .map_err(|source| Error::command("zellij", source.to_string()))?;
        match outcome {
            CommandOutcome::Finished { status, stdout, .. } if status.success() => Ok(stdout),
            CommandOutcome::Finished { status, stderr, .. } => Err(Error::command(
                "zellij",
                format!(
                    "action {} exited with {status}: {}",
                    args.join(" "),
                    stderr.trim()
                ),
            )),
            CommandOutcome::TimedOut => Err(Error::command(
                "zellij",
                format!(
                    "action {} timed out after {}s",
                    args.join(" "),
                    ACTION_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    fn parts(pane: &PaneAddress) -> Result<(&str, String)> {
        match pane {
            PaneAddress::Zellij { session, pane_id } => Ok((session, pane_id.to_string())),
            other => Err(Error::Invalid(format!("{other} is not a zellij pane"))),
        }
    }

    fn row(pane: &PaneAddress) -> Result<Option<PaneRow>> {
        let (session, pane_id) = Self::parts(pane)?;
        let wanted = pane_id
            .parse::<u32>()
            .map_err(|_| Error::Invalid(format!("pane id {pane_id} is not a number")))?;

        if !Self::session_running(session)? {
            return Ok(None);
        }

        let listing = Self::action(session, &["list-panes", "-a", "-j"])?;
        let rows: Vec<PaneRow> = serde_json::from_str(&listing).map_err(|error| {
            Error::command("zellij", format!("list-panes output is not JSON: {error}"))
        })?;
        Ok(rows
            .into_iter()
            .find(|row| row.id == wanted && !row.is_plugin))
    }
}

impl Zellij {
    fn list_sessions(flag: &str) -> Result<String> {
        let mut command = Command::new("zellij");
        command.arg("list-sessions").arg(flag);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ZELLIJ") {
                command.env_remove(name);
            }
        }
        let outcome = run_with_timeout(&mut command, None, ACTION_TIMEOUT)
            .map_err(|source| Error::command("zellij", source.to_string()))?;
        match outcome {
            CommandOutcome::Finished { stdout, .. } => Ok(stdout),
            CommandOutcome::TimedOut => Err(Error::command("zellij", "list-sessions timed out")),
        }
    }

    fn session_exists(session: &str) -> Result<bool> {
        let listing = Self::list_sessions("-s")?;

        Ok(listing.lines().any(|line| line.trim() == session))
    }

    fn session_running(session: &str) -> Result<bool> {
        let listing = Self::list_sessions("-n")?;

        Ok(is_running_in(&listing, session))
    }

    fn open_window_in(
        session: &str,
        cwd: &std::path::Path,
        argv: &[std::ffi::OsString],
    ) -> Result<PaneAddress> {
        if !Self::session_exists(session)? {
            let mut command = Command::new("zellij");
            command.arg("attach").arg("-b").arg("-c").arg(session);
            for (name, _) in std::env::vars_os() {
                if name.to_string_lossy().starts_with("ZELLIJ") {
                    command.env_remove(name);
                }
            }
            match run_with_timeout(&mut command, None, ACTION_TIMEOUT)
                .map_err(|source| Error::command("zellij", source.to_string()))?
            {
                CommandOutcome::Finished { status, stderr, .. } if !status.success() => {
                    return Err(Error::command(
                        "zellij",
                        format!(
                            "attach -b -c {session} exited with {status}: {}",
                            stderr.trim()
                        ),
                    ));
                }
                CommandOutcome::TimedOut => {
                    return Err(Error::command(
                        "zellij",
                        format!("attach -b -c {session} timed out"),
                    ));
                }
                CommandOutcome::Finished { .. } => {}
            }
        }
        let mut command = Command::new("zellij");
        command
            .arg("--session")
            .arg(session)
            .arg("run")
            .arg("--close-on-exit")
            .arg("--cwd")
            .arg(cwd)
            .arg("--")
            .arg("/usr/bin/env");
        command.args(argv);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("ZELLIJ") {
                command.env_remove(name);
            }
        }
        let outcome = run_with_timeout(&mut command, None, ACTION_TIMEOUT)
            .map_err(|source| Error::command("zellij", source.to_string()))?;
        let stdout = match outcome {
            CommandOutcome::Finished { status, stdout, .. } if status.success() => stdout,
            CommandOutcome::Finished { status, stderr, .. } => {
                return Err(Error::command(
                    "zellij",
                    format!("run exited with {status}: {}", stderr.trim()),
                ));
            }
            CommandOutcome::TimedOut => return Err(Error::command("zellij", "run timed out")),
        };
        let pane_id = stdout
            .trim()
            .strip_prefix("terminal_")
            .and_then(|id| id.parse::<u32>().ok())
            .ok_or_else(|| {
                Error::command(
                    "zellij",
                    format!("run did not report a pane id: {stdout:?}"),
                )
            })?;
        Ok(PaneAddress::Zellij {
            session: session.to_owned(),
            pane_id,
        })
    }
}

impl Multiplexer for Zellij {
    fn pane_alive(&self, pane: &PaneAddress) -> Result<bool> {
        Ok(Self::row(pane)?.is_some_and(|row| !row.exited))
    }

    fn foreground_command(&self, pane: &PaneAddress) -> Result<Option<String>> {
        Ok(Self::row(pane)?.and_then(PaneRow::foreground_command))
    }

    fn capture(&self, pane: &PaneAddress, full: bool) -> Result<String> {
        let (session, pane_id) = Self::parts(pane)?;
        let mut args = vec!["dump-screen", "--pane-id", &pane_id];
        if full {
            args.push("--full");
        }
        Self::action(session, &args)
    }

    fn send_text(&self, pane: &PaneAddress, text: &str) -> Result<()> {
        let (session, pane_id) = Self::parts(pane)?;
        Self::action(session, &["write-chars", "--pane-id", &pane_id, "--", text]).map(|_| ())
    }

    fn send_key(&self, pane: &PaneAddress, key: Key) -> Result<()> {
        let (session, pane_id) = Self::parts(pane)?;
        let bytes: &[&str] = match key {
            Key::Enter => &["13"],
            Key::Up => &["27", "91", "65"],
            Key::Down => &["27", "91", "66"],
        };
        let mut args = vec!["write", "--pane-id", &pane_id];
        args.extend_from_slice(bytes);
        Self::action(session, &args).map(|_| ())
    }

    fn open_window(
        &self,
        session: &str,
        cwd: &std::path::Path,
        argv: &[std::ffi::OsString],
    ) -> Result<PaneAddress> {
        Zellij::open_window_in(session, cwd, argv)
    }
}

fn is_running_in(listing: &str, session: &str) -> bool {
    let prefix = format!("{session} [");

    listing
        .lines()
        .any(|line| line.starts_with(&prefix) && !line.contains("(EXITED"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_command_follows_the_running_process_not_the_launch_command() {
        let listing = r#"[
            {"id": 0, "is_plugin": false, "exited": false,
             "terminal_command": "/usr/local/bin/codex --remote unix://",
             "pane_command": "/home/me/.local/libexec/claude-native --session-id 1a17bf8a"},
            {"id": 1, "is_plugin": false, "exited": false,
             "terminal_command": null, "pane_command": "/bin/bash"},
            {"id": 2, "is_plugin": false, "exited": true, "terminal_command": "go run ."}
        ]"#;
        let rows: Vec<PaneRow> = serde_json::from_str(listing).unwrap();
        let commands: Vec<Option<String>> =
            rows.into_iter().map(PaneRow::foreground_command).collect();

        assert_eq!(
            commands,
            [
                Some("claude-native".to_owned()),
                Some("bash".to_owned()),
                None
            ]
        );
    }

    #[test]
    fn only_a_live_session_counts_as_running() {
        let listing = "Brain [Created 21h 11m 43s ago] \n\
            claude-nonstop [Created 9h 20m 34s ago] (current)\n\
            AI.Admin [Created 21h 12m 52s ago] (EXITED - attach to resurrect)\n";

        assert!(is_running_in(listing, "Brain"));
        assert!(is_running_in(listing, "claude-nonstop"));
        assert!(!is_running_in(listing, "AI.Admin"));
        assert!(!is_running_in(listing, "AI"));
    }
}
