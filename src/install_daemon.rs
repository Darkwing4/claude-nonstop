use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::atomic_file;
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::platform::process::{CommandOutcome, run_with_timeout};

const UNIT_NAME: &str = "claude-nonstop.service";
const LAUNCHD_LABEL: &str = "io.github.claude-nonstop";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

pub fn unit_path(paths: &Paths) -> PathBuf {
    crate::paths::xdg("XDG_CONFIG_HOME", &paths.home, ".config")
        .join("systemd/user")
        .join(UNIT_NAME)
}

pub fn plist_path(paths: &Paths) -> PathBuf {
    paths
        .home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

pub fn render_unit(binary: &Path, path_env: &str) -> String {
    let escaped_path = path_env.replace('%', "%%");
    format!(
        "[Unit]\nDescription=claude-nonstop daemon\nStartLimitIntervalSec=0\n\n[Service]\nExecStart={} daemon\nEnvironment=PATH={escaped_path}\nRestart=always\nRestartSec=5\nKillMode=process\nTimeoutStopSec=10\n\n[Install]\nWantedBy=default.target\n",
        binary.display()
    )
}

pub fn render_plist(binary: &Path, path_env: &str, log_file: &Path) -> String {
    let binary = xml_escape(&binary.display().to_string());
    let path_env = xml_escape(path_env);
    let log_file = xml_escape(&log_file.display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LAUNCHD_LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{binary}</string>\n    <string>daemon</string>\n  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key><string>{path_env}</string>\n  </dict>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>5</integer>\n  <key>AbandonProcessGroup</key><true/>\n  <key>ProcessType</key><string>Background</string>\n  <key>StandardOutPath</key><string>{log_file}</string>\n  <key>StandardErrorPath</key><string>{log_file}</string>\n</dict>\n</plist>\n"
    )
}

pub fn install(paths: &Paths, binary: &Path) -> Result<Vec<String>> {
    let path_env =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned());
    if cfg!(target_os = "macos") {
        let plist = plist_path(paths);
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| Error::io(parent, "create", source))?;
        }
        let daemon_log = paths.log_dir().join("daemon.log");
        atomic_file::write(
            &plist,
            render_plist(binary, &path_env, &daemon_log).as_bytes(),
            0o644,
        )?;
        let target = format!("gui/{}", rustix::process::getuid().as_raw());
        control(
            "launchctl",
            &["bootout", &format!("{target}/{LAUNCHD_LABEL}")],
        )
        .ok();
        control(
            "launchctl",
            &["bootstrap", &target, &plist.display().to_string()],
        )?;
        return Ok(vec![
            format!("launch agent {}", plist.display()),
            "daemon started (launchctl)".to_owned(),
        ]);
    }
    let unit = unit_path(paths);
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::io(parent, "create", source))?;
    }
    atomic_file::write(&unit, render_unit(binary, &path_env).as_bytes(), 0o644)?;
    control("systemctl", &["--user", "daemon-reload"])?;
    control("systemctl", &["--user", "enable", "--now", UNIT_NAME])?;
    Ok(vec![
        format!("systemd unit {}", unit.display()),
        "daemon started (systemctl --user)".to_owned(),
        "on a server without a login session run: loginctl enable-linger $USER".to_owned(),
    ])
}

pub fn uninstall(paths: &Paths) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    if cfg!(target_os = "macos") {
        let plist = plist_path(paths);
        if plist.exists() {
            let target = format!("gui/{}/{LAUNCHD_LABEL}", rustix::process::getuid().as_raw());
            control("launchctl", &["bootout", &target]).ok();
            std::fs::remove_file(&plist).map_err(|source| Error::io(&plist, "remove", source))?;
            messages.push(format!("launch agent removed {}", plist.display()));
        }
        return Ok(messages);
    }
    let unit = unit_path(paths);
    if unit.exists() {
        control("systemctl", &["--user", "disable", "--now", UNIT_NAME]).ok();
        std::fs::remove_file(&unit).map_err(|source| Error::io(&unit, "remove", source))?;
        control("systemctl", &["--user", "daemon-reload"]).ok();
        messages.push(format!("systemd unit removed {}", unit.display()));
    }
    Ok(messages)
}

pub fn is_installed(paths: &Paths) -> bool {
    if cfg!(target_os = "macos") {
        plist_path(paths).exists()
    } else {
        unit_path(paths).exists()
    }
}

pub fn is_active() -> Option<bool> {
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{LAUNCHD_LABEL}", rustix::process::getuid().as_raw());
        return Some(control("launchctl", &["print", &target]).is_ok());
    }
    match control("systemctl", &["--user", "is-active", UNIT_NAME]) {
        Ok(output) => Some(output.trim() == "active"),
        Err(Error::Command { detail, .. })
            if detail.contains("inactive") || detail.contains("failed") =>
        {
            Some(false)
        }
        Err(_) => None,
    }
}

fn control(program: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    match run_with_timeout(&mut command, CONTROL_TIMEOUT)
        .map_err(|source| Error::command(program, source.to_string()))?
    {
        CommandOutcome::Finished { status, stdout, .. } if status.success() => Ok(stdout),
        CommandOutcome::Finished {
            status,
            stdout,
            stderr,
        } => Err(Error::command(
            program,
            format!(
                "{} exited with {status}: {}{}",
                args.join(" "),
                stdout.trim(),
                stderr.trim()
            ),
        )),
        CommandOutcome::TimedOut => Err(Error::command(
            program,
            format!("{} timed out", args.join(" ")),
        )),
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_and_plist_render_with_escaping() {
        let unit = render_unit(
            Path::new("/home/me/.local/bin/claude-nonstop"),
            "/home/me/bin:/usr/bin:%h/x",
        );
        assert!(unit.contains("ExecStart=/home/me/.local/bin/claude-nonstop daemon\n"));
        assert!(unit.contains("Environment=PATH=/home/me/bin:/usr/bin:%%h/x\n"));
        assert!(unit.contains("KillMode=process\n"));
        let plist = render_plist(
            Path::new("/Users/me/bin/claude-nonstop"),
            "/a:/b&c",
            Path::new("/Users/me/log/daemon.log"),
        );
        assert!(plist.contains("<string>/Users/me/bin/claude-nonstop</string>"));
        assert!(plist.contains("<string>/a:/b&amp;c</string>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key><true/>"));
    }
}
