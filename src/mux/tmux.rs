use std::process::Command;
use std::time::Duration;

use super::{Key, Multiplexer, PaneAddress};
use crate::error::{Error, Result};
use crate::platform::process::{CommandOutcome, run_with_timeout};

const ACTION_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Tmux;

impl Tmux {
    fn run(args: &[&str]) -> Result<String> {
        let mut command = Command::new("tmux");
        command.args(args);
        let outcome = run_with_timeout(&mut command, ACTION_TIMEOUT)
            .map_err(|source| Error::command("tmux", source.to_string()))?;
        match outcome {
            CommandOutcome::Finished { status, stdout, .. } if status.success() => Ok(stdout),
            CommandOutcome::Finished { status, stderr, .. } => Err(Error::command(
                "tmux",
                format!("{} exited with {status}: {}", args.join(" "), stderr.trim()),
            )),
            CommandOutcome::TimedOut => Err(Error::command(
                "tmux",
                format!(
                    "{} timed out after {}s",
                    args.join(" "),
                    ACTION_TIMEOUT.as_secs()
                ),
            )),
        }
    }

    fn id(pane: &PaneAddress) -> Result<&str> {
        match pane {
            PaneAddress::Tmux { pane_id } => Ok(pane_id),
            other => Err(Error::Invalid(format!("{other} is not a tmux pane"))),
        }
    }
}

impl Multiplexer for Tmux {
    fn pane_alive(&self, pane: &PaneAddress) -> Result<bool> {
        let id = Self::id(pane)?;
        match Self::run(&["display-message", "-t", id, "-p", "#{pane_id}"]) {
            Ok(output) => Ok(output.trim() == id),
            Err(Error::Command { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn foreground_command(&self, pane: &PaneAddress) -> Result<Option<String>> {
        let id = Self::id(pane)?;
        let output = Self::run(&["display-message", "-t", id, "-p", "#{pane_current_command}"])?;
        let command = output.trim();
        Ok(if command.is_empty() {
            None
        } else {
            Some(command.to_owned())
        })
    }

    fn capture(&self, pane: &PaneAddress, full: bool) -> Result<String> {
        let id = Self::id(pane)?;
        if full {
            Self::run(&["capture-pane", "-t", id, "-p", "-S", "-"])
        } else {
            Self::run(&["capture-pane", "-t", id, "-p"])
        }
    }

    fn send_text(&self, pane: &PaneAddress, text: &str) -> Result<()> {
        let id = Self::id(pane)?;
        Self::run(&["send-keys", "-t", id, "-l", "--", text]).map(|_| ())
    }

    fn send_key(&self, pane: &PaneAddress, key: Key) -> Result<()> {
        let id = Self::id(pane)?;
        let name = match key {
            Key::Enter => "Enter",
            Key::Up => "Up",
            Key::Down => "Down",
        };
        Self::run(&["send-keys", "-t", id, name]).map(|_| ())
    }

    fn open_window(
        &self,
        session: &str,
        cwd: &std::path::Path,
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
    ) -> Result<PaneAddress> {
        Tmux::open_window_in(session, cwd, argv, env)
    }
}

impl Tmux {
    pub fn open_window_in(
        session: &str,
        cwd: &std::path::Path,
        argv: &[std::ffi::OsString],
        env: &[(String, String)],
    ) -> Result<PaneAddress> {
        let cwd_text = cwd.display().to_string();
        let has_session = Self::run(&["has-session", "-t", session]).is_ok();
        let mut args: Vec<String> = if has_session {
            vec![
                "new-window".into(),
                "-t".into(),
                format!("{session}:"),
                "-c".into(),
                cwd_text,
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
            ]
        } else {
            vec![
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                session.into(),
                "-c".into(),
                cwd_text,
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
            ]
        };
        for (name, value) in env {
            args.push("-e".into());
            args.push(format!("{name}={value}"));
        }
        args.push("--".into());
        for arg in argv {
            args.push(arg.to_string_lossy().into_owned());
        }
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = Self::run(&borrowed)?;
        let pane_id = output.trim().to_owned();
        if !pane_id.starts_with('%') {
            return Err(Error::command(
                "tmux",
                format!("new window did not report a pane id: {output:?}"),
            ));
        }
        Ok(PaneAddress::Tmux { pane_id })
    }
}
