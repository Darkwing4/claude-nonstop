use std::io::{self, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rustix::fs::{Mode, OFlags};

use crate::config::{Notify, NotifyChannel, NtfyPrivacy};
use crate::mux::PaneAddress;
use crate::platform::process::{CommandOutcome, run_with_timeout, wait_timeout};

const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const NTFY_TIMEOUT: Duration = Duration::from_secs(6);
const TITLE_LIMIT: usize = 100;
const BODY_LIMIT: usize = 200;
const TERSE_NTFY_MESSAGE: &str = "run: claude-nonstop status";
const OSC9_TERMINALS: [&str; 5] = ["iterm", "kitty", "wezterm", "ghostty", "warp"];
const UNSUPPORTED_TERMINALS: [&str; 4] = ["apple_terminal", "vscode", "alacritty", "zed"];

pub fn send(settings: &Notify, title: &str, body: &str, urgent: bool) -> String {
    send_for(settings, None, title, body, urgent)
}

pub fn send_for(
    settings: &Notify,
    pane: Option<&PaneAddress>,
    title: &str,
    body: &str,
    urgent: bool,
) -> String {
    if !settings.enabled {
        return "notifications off".to_owned();
    }
    let title = sanitize(title, TITLE_LIMIT);
    let body = sanitize(body, BODY_LIMIT);
    let tmux_pane = match pane {
        Some(PaneAddress::Tmux { pane_id }) => Some(pane_id.as_str()),
        _ => None,
    };
    let mut outcomes = Vec::new();
    match (settings.channel, tmux_pane) {
        (NotifyChannel::Native, _) => {
            outcomes.push(format!("native: {}", native(&title, &body, urgent)));
        }
        (NotifyChannel::Osc, Some(pane_id)) => {
            let report = osc(pane_id, &title, &body, false);
            outcomes.push(format!("osc: {}", report.describe()));
            if report.delivered == 0 {
                outcomes.push(format!("native: {}", native(&title, &body, urgent)));
            }
        }
        (NotifyChannel::Bell, Some(pane_id)) => match bell(pane_id) {
            Ok(()) => outcomes.push("bell: delivered".to_owned()),
            Err(error) => {
                outcomes.push(format!("bell: {error}"));
                outcomes.push(format!("native: {}", native(&title, &body, urgent)));
            }
        },
        (NotifyChannel::Auto, Some(pane_id)) => {
            let report = osc(pane_id, &title, &body, true);
            outcomes.push(format!("osc: {}", report.describe()));
            match bell(pane_id) {
                Ok(()) => outcomes.push("bell: delivered".to_owned()),
                Err(error) => outcomes.push(format!("bell: {error}")),
            }
            if report.delivered == 0 {
                outcomes.push(format!("native: {}", native(&title, &body, urgent)));
            }
        }
        (NotifyChannel::Osc | NotifyChannel::Bell, None) => {
            outcomes.push(format!(
                "native (no client tty for this pane): {}",
                native(&title, &body, urgent)
            ));
        }
        (NotifyChannel::Auto, None) => {
            outcomes.push(format!("native: {}", native(&title, &body, urgent)));
        }
    }
    if !settings.ntfy_topic.is_empty() {
        outcomes.push(format!("ntfy: {}", ntfy(settings, &title, &body, urgent)));
    }
    outcomes.join("; ")
}

fn native(title: &str, body: &str, urgent: bool) -> String {
    let mut command = if cfg!(target_os = "macos") {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape_applescript(body),
            escape_applescript(title)
        );
        let mut command = Command::new("osascript");
        command.arg("-e").arg(script);
        command
    } else {
        let mut command = Command::new("notify-send");
        command
            .arg("-a")
            .arg("claude-nonstop")
            .arg("-u")
            .arg(if urgent { "critical" } else { "normal" })
            .arg(title)
            .arg(body);
        command
    };
    describe_outcome(run_with_timeout(&mut command, SEND_TIMEOUT))
}

fn ntfy(settings: &Notify, title: &str, body: &str, urgent: bool) -> String {
    let message = ntfy_message(body, settings.ntfy_privacy);
    let payload = ntfy_payload(&settings.ntfy_topic, title, &message, urgent);
    let server = format!("{}/", settings.ntfy_server.trim_end_matches('/'));
    let mut command = Command::new("curl");
    command
        .arg("--fail")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg("5")
        .arg("-H")
        .arg("Content-Type: application/json");
    if !settings.ntfy_token.is_empty() {
        command
            .arg("-H")
            .arg(format!("Authorization: Bearer {}", settings.ntfy_token));
    }
    command.arg("--data").arg("@-").arg(server);
    describe_outcome(run_with_input(&mut command, &payload, NTFY_TIMEOUT))
}

fn ntfy_message(body: &str, privacy: NtfyPrivacy) -> String {
    match privacy {
        NtfyPrivacy::Full => body.to_owned(),
        NtfyPrivacy::Terse => TERSE_NTFY_MESSAGE.to_owned(),
    }
}

fn ntfy_payload(topic: &str, title: &str, message: &str, urgent: bool) -> String {
    serde_json::json!({
        "topic": topic,
        "title": title,
        "message": message,
        "priority": if urgent { 4 } else { 3 },
    })
    .to_string()
}

fn describe_outcome(outcome: io::Result<CommandOutcome>) -> String {
    match outcome {
        Ok(CommandOutcome::Finished { status, .. }) if status.success() => "delivered".to_owned(),
        Ok(CommandOutcome::Finished { status, stderr, .. }) => {
            format!("exited with {status}: {}", stderr.trim())
        }
        Ok(CommandOutcome::TimedOut) => "timed out".to_owned(),
        Err(error) => format!("not started: {error}"),
    }
}

fn run_with_input(
    command: &mut Command,
    input: &str,
    timeout: Duration,
) -> io::Result<CommandOutcome> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let input_result = match child.stdin.take() {
        Some(mut stdin) => stdin.write_all(input.as_bytes()),
        None => Ok(()),
    };
    let outcome = finish_child(&mut child, timeout)?;
    match input_result {
        Ok(()) => Ok(outcome),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("stdin write failed: {error}"),
        )),
    }
}

fn finish_child(child: &mut Child, timeout: Duration) -> io::Result<CommandOutcome> {
    match wait_timeout(child, timeout)? {
        Some(status) => {
            let mut stdout = String::new();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                io::Read::read_to_string(&mut pipe, &mut stdout)?;
            }
            if let Some(mut pipe) = child.stderr.take() {
                io::Read::read_to_string(&mut pipe, &mut stderr)?;
            }
            Ok(CommandOutcome::Finished {
                status,
                stdout,
                stderr,
            })
        }
        None => {
            child.kill()?;
            child.wait()?;
            Ok(CommandOutcome::TimedOut)
        }
    }
}

fn tmux(args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("tmux");
    command.args(args);
    match run_with_timeout(&mut command, SEND_TIMEOUT) {
        Ok(CommandOutcome::Finished { status, stdout, .. }) if status.success() => Ok(stdout),
        Ok(CommandOutcome::Finished { status, stderr, .. }) => Err(format!(
            "tmux {} exited with {status}: {}",
            args[0],
            stderr.trim()
        )),
        Ok(CommandOutcome::TimedOut) => Err(format!("tmux {} timed out", args[0])),
        Err(error) => Err(format!("tmux {} not started: {error}", args[0])),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalKind {
    Osc9,
    Osc777,
    Unsupported,
    Unknown,
}

fn classify_terminal(label: &str) -> TerminalKind {
    let label = label.to_ascii_lowercase();
    if label.contains("rxvt") {
        return TerminalKind::Osc777;
    }
    if OSC9_TERMINALS.iter().any(|name| label.contains(name)) {
        return TerminalKind::Osc9;
    }
    if UNSUPPORTED_TERMINALS
        .iter()
        .any(|name| label.contains(name))
    {
        return TerminalKind::Unsupported;
    }
    TerminalKind::Unknown
}

fn detect_terminal(termname: &str, environment_hints: &[String]) -> TerminalKind {
    let from_termname = classify_terminal(termname);
    if from_termname != TerminalKind::Unknown {
        return from_termname;
    }
    environment_hints
        .iter()
        .map(|hint| classify_terminal(hint))
        .find(|kind| *kind != TerminalKind::Unknown)
        .unwrap_or(TerminalKind::Unknown)
}

fn terminal_hints(tmux_environment: &str) -> Vec<String> {
    let mut hints = Vec::new();
    for line in tmux_environment.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "TERM_PROGRAM" | "LC_TERMINAL" => hints.push(value.to_owned()),
            "KITTY_WINDOW_ID" => hints.push("kitty".to_owned()),
            "WEZTERM_EXECUTABLE" => hints.push("wezterm".to_owned()),
            "GHOSTTY_RESOURCES_DIR" => hints.push("ghostty".to_owned()),
            _ => {}
        }
    }
    hints
}

struct OscReport {
    delivered: usize,
    skipped: usize,
    failures: Vec<String>,
    listing_error: Option<String>,
}

impl OscReport {
    fn describe(&self) -> String {
        if let Some(error) = &self.listing_error {
            return error.clone();
        }
        let mut text = format!(
            "{} client{}",
            self.delivered,
            if self.delivered == 1 { "" } else { "s" }
        );
        if self.skipped > 0 {
            text.push_str(&format!(", {} skipped", self.skipped));
        }
        if let Some(first) = self.failures.first() {
            text.push_str(&format!(", {} failed: {first}", self.failures.len()));
        }
        text
    }
}

fn osc(pane_id: &str, title: &str, body: &str, known_terminals_only: bool) -> OscReport {
    let mut report = OscReport {
        delivered: 0,
        skipped: 0,
        failures: Vec::new(),
        listing_error: None,
    };
    let clients = match tmux(&[
        "list-clients",
        "-t",
        pane_id,
        "-F",
        "#{client_tty}\t#{client_termname}",
    ]) {
        Ok(output) => output,
        Err(error) => {
            report.listing_error = Some(error);
            return report;
        }
    };
    let hints = match tmux(&["show-environment", "-g"]) {
        Ok(output) => terminal_hints(&output),
        Err(error) => {
            report.failures.push(error);
            Vec::new()
        }
    };
    for line in clients.lines() {
        let (tty, termname) = line.split_once('\t').unwrap_or((line, ""));
        if tty.is_empty() {
            continue;
        }
        let kind = detect_terminal(termname, &hints);
        let payload = match kind {
            TerminalKind::Osc777 => osc777_payload(title, body),
            TerminalKind::Osc9 => osc9_payload(title, body),
            TerminalKind::Unknown if !known_terminals_only => osc9_payload(title, body),
            TerminalKind::Unknown | TerminalKind::Unsupported => {
                report.skipped += 1;
                continue;
            }
        };
        match write_to_tty(tty, payload.as_bytes()) {
            Ok(()) => report.delivered += 1,
            Err(error) => report.failures.push(format!("{tty}: {error}")),
        }
    }
    report
}

fn osc9_payload(title: &str, body: &str) -> String {
    format!("\x1b]9;{title}: {body}\x07")
}

fn osc777_payload(title: &str, body: &str) -> String {
    format!(
        "\x1b]777;notify;{};{}\x07",
        title.replace(';', ""),
        body.replace(';', "")
    )
}

fn bell(pane_id: &str) -> Result<(), String> {
    let output = tmux(&["display-message", "-t", pane_id, "-p", "#{pane_tty}"])?;
    let tty = output.trim();
    if tty.is_empty() {
        return Err("pane has no tty".to_owned());
    }
    write_to_tty(tty, b"\x07").map_err(|error| format!("{tty}: {error}"))
}

fn write_to_tty(path: &str, payload: &[u8]) -> io::Result<()> {
    let fd = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let written = rustix::io::write(&fd, payload)?;
    if written != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short write: {written} of {} bytes", payload.len()),
        ));
    }
    Ok(())
}

fn sanitize(text: &str, limit: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(limit).collect()
}

fn escape_applescript(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    struct FakeTmux {
        dir: PathBuf,
        tty: PathBuf,
        pane_tty: PathBuf,
        saved_path: String,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl FakeTmux {
        fn install(name: &str, termname: &str, environment: &str) -> FakeTmux {
            let guard = PATH_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "claude-nonstop-notify-{}-{name}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            let tty = dir.join("tty");
            fs::write(&tty, b"").unwrap();
            let pane_tty = dir.join("pane-tty");
            fs::write(&pane_tty, b"").unwrap();
            let script = format!(
                "#!/bin/sh\ncase \"$1\" in\n  list-clients) printf '%s\\t%s\\n' '{tty}' '{termname}' ;;\n  show-environment) printf '%s\\n' '{environment}' ;;\n  display-message) printf '%s\\n' '{pane_tty}' ;;\n  *) echo \"unexpected: $*\" >&2; exit 1 ;;\nesac\n",
                tty = tty.display(),
                pane_tty = pane_tty.display()
            );
            let binary = dir.join("tmux");
            fs::write(&binary, script).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            let saved_path = std::env::var("PATH").unwrap_or_default();
            unsafe {
                std::env::set_var("PATH", format!("{}:{saved_path}", dir.display()));
            }
            FakeTmux {
                dir,
                tty,
                pane_tty,
                saved_path,
                _guard: guard,
            }
        }

        fn tty_contents(&self) -> String {
            fs::read_to_string(&self.tty).unwrap()
        }

        fn pane_tty_contents(&self) -> String {
            fs::read_to_string(&self.pane_tty).unwrap()
        }
    }

    impl Drop for FakeTmux {
        fn drop(&mut self) {
            unsafe {
                std::env::set_var("PATH", &self.saved_path);
            }
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    fn tmux_pane() -> PaneAddress {
        PaneAddress::Tmux {
            pane_id: "%7".to_owned(),
        }
    }

    #[test]
    fn text_is_sanitized_and_bounded() {
        assert_eq!(sanitize("a\x1b[31m b\n\tc", 100), "a [31m b c");
        assert_eq!(sanitize(&"x".repeat(300), 200).len(), 200);
        assert_eq!(
            escape_applescript(r#"say "hi" \ bye"#),
            r#"say \"hi\" \\ bye"#
        );
    }

    #[test]
    fn disabled_notifications_send_nothing() {
        let settings = Notify {
            enabled: false,
            ntfy_topic: "alerts".to_owned(),
            ..Notify::default()
        };
        assert_eq!(send(&settings, "t", "b", false), "notifications off");
        assert_eq!(
            send_for(&settings, Some(&tmux_pane()), "t", "b", true),
            "notifications off"
        );
    }

    #[test]
    fn osc_mode_writes_osc9_to_kitty_client() {
        let fake = FakeTmux::install("kitty", "xterm-kitty", "TERM=xterm-256color");
        let settings = Notify {
            channel: NotifyChannel::Osc,
            ..Notify::default()
        };
        let outcome = send_for(&settings, Some(&tmux_pane()), "Title", "the body", false);
        assert_eq!(outcome, "osc: 1 client");
        assert_eq!(fake.tty_contents(), "\x1b]9;Title: the body\x07");
    }

    #[test]
    fn osc_mode_writes_osc777_without_semicolons_to_rxvt_client() {
        let fake = FakeTmux::install("rxvt", "rxvt-unicode", "TERM=xterm-256color");
        let settings = Notify {
            channel: NotifyChannel::Osc,
            ..Notify::default()
        };
        let outcome = send_for(&settings, Some(&tmux_pane()), "Ti;tle", "bo;dy;", false);
        assert_eq!(outcome, "osc: 1 client");
        assert_eq!(fake.tty_contents(), "\x1b]777;notify;Title;body\x07");
    }

    #[test]
    fn auto_mode_with_tmux_pane_uses_environment_hints_and_bell() {
        let fake = FakeTmux::install("auto", "tmux-256color", "KITTY_WINDOW_ID=3");
        let settings = Notify::default();
        let outcome = send_for(&settings, Some(&tmux_pane()), "T", "B", false);
        assert_eq!(outcome, "osc: 1 client; bell: delivered");
        assert_eq!(fake.tty_contents(), "\x1b]9;T: B\x07");
        assert_eq!(fake.pane_tty_contents(), "\x07");
    }

    #[test]
    fn zellij_pane_in_auto_mode_falls_back_to_native() {
        let pane = PaneAddress::Zellij {
            session: "main".to_owned(),
            pane_id: 2,
        };
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "claude-nonstop-notify-{}-native",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        for name in ["notify-send", "osascript"] {
            let binary = dir.join(name);
            fs::write(
                &binary,
                format!("#!/bin/sh\ntouch '{}/called'\n", dir.display()),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let saved_path = std::env::var("PATH").unwrap_or_default();
        unsafe {
            std::env::set_var("PATH", format!("{}:{saved_path}", dir.display()));
        }

        let outcome = send_for(&Notify::default(), Some(&pane), "t", "b", false);

        let called = dir.join("called").exists();
        unsafe {
            std::env::set_var("PATH", saved_path);
        }
        fs::remove_dir_all(&dir).unwrap();
        assert!(outcome.starts_with("native"), "{outcome}");
        assert!(called);
    }

    #[test]
    fn ntfy_posts_json_on_curl_stdin_with_token_header() {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("claude-nonstop-notify-{}-curl", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "#!/bin/sh\ncat > '{dir}/stdin'\nprintf '%s\\n' \"$@\" > '{dir}/args'\n",
            dir = dir.display()
        );
        let binary = dir.join("curl");
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let saved_path = std::env::var("PATH").unwrap_or_default();
        unsafe {
            std::env::set_var("PATH", format!("{}:{saved_path}", dir.display()));
        }
        let settings = Notify {
            channel: NotifyChannel::Native,
            ntfy_topic: "alerts".to_owned(),
            ntfy_server: "https://ntfy.example/".to_owned(),
            ntfy_token: "tk_1".to_owned(),
            ntfy_privacy: NtfyPrivacy::Terse,
            ..Notify::default()
        };
        let outcome = ntfy(&settings, "T", "secret", true);
        let stdin = fs::read_to_string(dir.join("stdin")).unwrap();
        let args = fs::read_to_string(dir.join("args")).unwrap();
        unsafe {
            std::env::set_var("PATH", saved_path);
        }
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(outcome, "delivered");
        assert_eq!(
            stdin,
            r#"{"topic":"alerts","title":"T","message":"run: claude-nonstop status","priority":4}"#
        );
        assert!(args.contains("Authorization: Bearer tk_1\n"), "{args}");
        assert!(args.ends_with("@-\nhttps://ntfy.example/\n"), "{args}");
    }

    #[test]
    fn terse_privacy_replaces_the_body() {
        assert_eq!(ntfy_message("secret", NtfyPrivacy::Full), "secret");
        assert_eq!(
            ntfy_message("secret", NtfyPrivacy::Terse),
            "run: claude-nonstop status"
        );
    }

    #[test]
    fn ntfy_payload_is_json_with_priority() {
        assert_eq!(
            ntfy_payload("alerts", "claude-nonstop: limit", "resets at 9", true),
            r#"{"topic":"alerts","title":"claude-nonstop: limit","message":"resets at 9","priority":4}"#
        );
        assert_eq!(
            ntfy_payload("alerts", "t", "b", false),
            r#"{"topic":"alerts","title":"t","message":"b","priority":3}"#
        );
    }

    #[test]
    fn terminal_brand_is_detected_from_termname_or_environment() {
        assert_eq!(classify_terminal("xterm-ghostty"), TerminalKind::Osc9);
        assert_eq!(
            classify_terminal("rxvt-unicode-256color"),
            TerminalKind::Osc777
        );
        assert_eq!(
            classify_terminal("Apple_Terminal"),
            TerminalKind::Unsupported
        );
        assert_eq!(classify_terminal("xterm-256color"), TerminalKind::Unknown);
        let hints =
            terminal_hints("TERM=xterm-256color\nWEZTERM_EXECUTABLE=/usr/bin/wezterm\n-OLD");
        assert_eq!(hints, vec!["wezterm".to_owned()]);
        assert_eq!(detect_terminal("tmux-256color", &hints), TerminalKind::Osc9);
        assert_eq!(
            detect_terminal("tmux-256color", &["vscode".to_owned()]),
            TerminalKind::Unsupported
        );
        assert_eq!(detect_terminal("tmux-256color", &[]), TerminalKind::Unknown);
    }
}
