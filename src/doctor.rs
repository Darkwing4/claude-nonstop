use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::config;
use crate::error::Result;
use crate::paths::Paths;
use crate::platform::process::{CommandOutcome, run_with_timeout};

pub const TESTED_CLAUDE_VERSION: (u32, u32, u32) = (2, 1, 287);
const MIN_TMUX: (u32, u32) = (3, 0);
const MIN_ZELLIJ: (u32, u32) = (0, 35);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const COMPETING_HOOKS: &[&str] = &["claude-api-retry-hook", "claude-auto-retry", "unsnooze"];

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub title: String,
    pub hint: Option<String>,
    pub fixed: bool,
}

impl Finding {
    fn new(id: &'static str, severity: Severity, title: impl Into<String>) -> Finding {
        Finding {
            id,
            severity,
            title: title.into(),
            hint: None,
            fixed: false,
        }
    }

    fn hint(mut self, hint: impl Into<String>) -> Finding {
        self.hint = Some(hint.into());
        self
    }
}

pub fn run(paths: &Paths, fix: bool) -> Result<Vec<Finding>> {
    let mut findings = vec![
        state_dir(paths, fix)?,
        configuration(paths),
        binary_on_path(),
    ];
    findings.extend(multiplexers());
    findings.push(claude_version());
    findings.extend(hook(paths)?);
    findings.push(daemon_unit(paths));
    Ok(findings)
}

pub fn worst(findings: &[Finding]) -> Severity {
    findings
        .iter()
        .map(|finding| finding.severity)
        .max()
        .unwrap_or(Severity::Info)
}

fn state_dir(paths: &Paths, fix: bool) -> Result<Finding> {
    let dir = &paths.state;
    if !dir.is_dir() {
        if fix {
            paths.ensure_state_dirs()?;
            return Ok(Finding {
                fixed: true,
                ..Finding::new(
                    "state-dir",
                    Severity::Info,
                    format!("state directory created at {}", dir.display()),
                )
            });
        }
        return Ok(Finding::new(
            "state-dir",
            Severity::Warn,
            format!("state directory {} does not exist yet", dir.display()),
        )
        .hint("created on first use or by doctor --fix"));
    }
    let mode = std::fs::metadata(dir)
        .map_err(|source| crate::error::Error::io(dir, "stat", source))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o700 {
        if fix {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|source| crate::error::Error::io(dir, "chmod", source))?;
            return Ok(Finding {
                fixed: true,
                ..Finding::new(
                    "state-dir",
                    Severity::Info,
                    format!("state directory mode set to 0700 at {}", dir.display()),
                )
            });
        }
        return Ok(Finding::new(
            "state-dir",
            Severity::Warn,
            format!(
                "state directory {} has mode {mode:o}, expected 700",
                dir.display()
            ),
        )
        .hint("doctor --fix"));
    }
    Ok(Finding::new(
        "state-dir",
        Severity::Info,
        format!("state directory {}", dir.display()),
    ))
}

fn configuration(paths: &Paths) -> Finding {
    match config::load(paths) {
        Ok(_) if paths.config_file.is_file() => Finding::new(
            "config",
            Severity::Info,
            format!("configuration {}", paths.config_file.display()),
        ),
        Ok(_) => Finding::new(
            "config",
            Severity::Info,
            "configuration: defaults (no file)",
        ),
        Err(error) => Finding::new("config", Severity::Error, format!("configuration: {error}"))
            .hint(format!(
                "edit {} or unset the CLAUDE_NONSTOP_ variable named above",
                paths.config_file.display()
            )),
    }
}

fn binary_on_path() -> Finding {
    let Ok(current) = std::env::current_exe() else {
        return Finding::new(
            "binary-path",
            Severity::Warn,
            "cannot resolve the running binary",
        );
    };
    let Some(found) = find_on_path("claude-nonstop") else {
        return Finding::new(
            "binary-path",
            Severity::Warn,
            format!(
                "claude-nonstop is not on PATH (running {})",
                current.display()
            ),
        )
        .hint("add its directory to PATH so the hook can find it");
    };
    if same_file(&found, &current) {
        Finding::new(
            "binary-path",
            Severity::Info,
            format!("binary {}", current.display()),
        )
    } else {
        Finding::new(
            "binary-path",
            Severity::Warn,
            format!(
                "PATH resolves claude-nonstop to {} but this is {}",
                found.display(),
                current.display()
            ),
        )
        .hint("remove the stale copy or reorder PATH")
    }
}

fn max_retries(settings: &serde_json::Value) -> Finding {
    let configured = settings
        .as_object()
        .and_then(crate::install_retry_limit::configured);
    match configured {
        Some(count) => Finding::new(
            "max-retries",
            Severity::Info,
            format!("Claude Code gives up after {count} retries"),
        ),
        None => Finding::new(
            "max-retries",
            Severity::Warn,
            format!(
                "{} is not set: Claude Code retries for minutes before the hook fires",
                crate::install_retry_limit::MAX_RETRIES_ENV
            ),
        )
        .hint("run claude-nonstop install"),
    }
}

fn multiplexers() -> Vec<Finding> {
    let mut findings = Vec::new();
    let tmux = version_of("tmux", &["-V"]);
    let zellij = version_of("zellij", &["--version"]);
    match tmux {
        Some(version) if version >= MIN_TMUX => findings.push(Finding::new(
            "mux",
            Severity::Info,
            format!("tmux {}.{}", version.0, version.1),
        )),
        Some(version) => findings.push(
            Finding::new(
                "mux",
                Severity::Warn,
                format!(
                    "tmux {}.{} is older than {}.{}",
                    version.0, version.1, MIN_TMUX.0, MIN_TMUX.1
                ),
            )
            .hint("claude-nonstop is tested with tmux 3.0 or newer"),
        ),
        None => {}
    }
    match zellij {
        Some(version) if version >= MIN_ZELLIJ => findings.push(Finding::new(
            "mux",
            Severity::Info,
            format!("zellij {}.{}", version.0, version.1),
        )),
        Some(version) => findings.push(
            Finding::new(
                "mux",
                Severity::Warn,
                format!(
                    "zellij {}.{} is older than {}.{}",
                    version.0, version.1, MIN_ZELLIJ.0, MIN_ZELLIJ.1
                ),
            )
            .hint("full screen dumps need zellij 0.35 or newer"),
        ),
        None => {}
    }
    if findings.is_empty() {
        findings.push(
            Finding::new(
                "mux",
                Severity::Info,
                "no tmux or zellij on PATH: sessions run headless",
            )
            .hint("install tmux or zellij for typing into a live pane"),
        );
    }
    findings
}

fn claude_version() -> Finding {
    let Some(output) = command_stdout("claude", &["--version"]) else {
        return Finding::new(
            "claude-version",
            Severity::Warn,
            "claude is not on PATH or did not answer --version",
        )
        .hint("install Claude Code first");
    };
    let Some(version) = parse_three(&output) else {
        return Finding::new(
            "claude-version",
            Severity::Warn,
            format!("cannot parse claude version from {output:?}"),
        );
    };
    let tested = TESTED_CLAUDE_VERSION;
    let label = format!("{}.{}.{}", version.0, version.1, version.2);
    if version > tested {
        Finding::new("claude-version", Severity::Warn, format!("claude {label} is newer than the tested {}.{}.{}", tested.0, tested.1, tested.2)).hint("hook and transcript formats are unverified for this version; watch the log for parse failures")
    } else {
        Finding::new("claude-version", Severity::Info, format!("claude {label}"))
    }
}

fn hook(paths: &Paths) -> Result<Vec<Finding>> {
    let settings_path = paths.claude_settings();
    let Some(bytes) = crate::atomic_file::read_optional(&settings_path)? else {
        return Ok(vec![
            Finding::new(
                "hook",
                Severity::Warn,
                format!("{} does not exist", settings_path.display()),
            )
            .hint("run claude-nonstop install"),
        ]);
    };
    let settings: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return Ok(vec![Finding::new(
                "hook",
                Severity::Error,
                format!("{}: {error}", settings_path.display()),
            )]);
        }
    };
    let mut findings = vec![max_retries(&settings)];
    if settings
        .get("disableAllHooks")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        findings.push(
            Finding::new(
                "hook",
                Severity::Error,
                "disableAllHooks is true in settings.json: the StopFailure hook never fires",
            )
            .hint("set disableAllHooks to false"),
        );
    }
    let commands: Vec<String> = settings
        .pointer("/hooks/StopFailure")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|entry| entry.get("command").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    let ours: Vec<&String> = commands
        .iter()
        .filter(|command| command.contains("claude-nonstop hook"))
        .collect();
    let current = std::env::current_exe().ok();
    match (ours.first(), current) {
        (None, _) => findings.push(
            Finding::new("hook", Severity::Warn, "StopFailure hook is not installed")
                .hint("run claude-nonstop install"),
        ),
        (Some(command), Some(current)) if !command.contains(&current.display().to_string()) => {
            findings.push(
                Finding::new(
                    "hook",
                    Severity::Warn,
                    format!("StopFailure hook points at another binary: {command}"),
                )
                .hint("run claude-nonstop install to update the path"),
            );
        }
        (Some(_), _) => findings.push(Finding::new(
            "hook",
            Severity::Info,
            "StopFailure hook installed",
        )),
    }
    for command in &commands {
        if let Some(name) = COMPETING_HOOKS.iter().find(|name| command.contains(*name)) {
            findings.push(
                Finding::new(
                    "hook",
                    Severity::Warn,
                    format!("another retry tool is hooked into StopFailure: {name}"),
                )
                .hint("two tools will type into the same pane; remove one"),
            );
        }
    }
    Ok(findings)
}

fn daemon_unit(paths: &Paths) -> Finding {
    if !crate::install_daemon::is_installed(paths) {
        return Finding::new(
            "daemon",
            Severity::Info,
            "daemon not installed: waits run in a transient resumer",
        )
        .hint("claude-nonstop install --daemon keeps waits alive across pane closes and reboots");
    }
    match crate::install_daemon::is_active() {
        Some(true) => Finding::new("daemon", Severity::Info, "daemon unit installed and active"),
        Some(false) => Finding::new(
            "daemon",
            Severity::Warn,
            "daemon unit installed but not active",
        )
        .hint("systemctl --user start claude-nonstop (or launchctl on macOS); check the unit log"),
        None => Finding::new(
            "daemon",
            Severity::Warn,
            "daemon unit installed, state unknown",
        )
        .hint("the service manager did not answer"),
    }
}

fn version_of(program: &str, args: &[&str]) -> Option<(u32, u32)> {
    let output = command_stdout(program, args)?;
    parse_three(&output).map(|version| (version.0, version.1))
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    match run_with_timeout(&mut command, PROBE_TIMEOUT) {
        Ok(CommandOutcome::Finished { status, stdout, .. }) if status.success() => Some(stdout),
        Ok(_) | Err(_) => None,
    }
}

fn parse_three(text: &str) -> Option<(u32, u32, u32)> {
    let token = text
        .split_whitespace()
        .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let numeric: String = token
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = numeric.split('.').map(|part| part.parse::<u32>().ok());
    let major = parts.next().flatten()?;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

pub fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

pub fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_from_tool_banners() {
        assert_eq!(parse_three("tmux 3.5a\n"), Some((3, 5, 0)));
        assert_eq!(parse_three("zellij 0.45.1\n"), Some((0, 45, 1)));
        assert_eq!(parse_three("2.1.263 (Claude Code)\n"), Some((2, 1, 263)));
        assert_eq!(parse_three("no digits here"), None);
    }

    #[test]
    fn worst_severity_wins() {
        let findings = vec![
            Finding::new("a", Severity::Info, "a"),
            Finding::new("b", Severity::Warn, "b"),
        ];
        assert_eq!(worst(&findings), Severity::Warn);
        assert_eq!(worst(&[]), Severity::Info);
    }
}
