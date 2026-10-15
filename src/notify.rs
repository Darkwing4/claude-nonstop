use std::io::{self, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::config::{Notify, NtfyPrivacy};
use crate::platform::process::{CommandOutcome, run_with_timeout, wait_timeout};

const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const NTFY_TIMEOUT: Duration = Duration::from_secs(6);
const TITLE_LIMIT: usize = 100;
const BODY_LIMIT: usize = 200;
const TERSE_NTFY_MESSAGE: &str = "run: claude-nonstop status";

pub fn send(settings: &Notify, title: &str, body: &str, urgent: bool) -> String {
    if !settings.enabled {
        return "notifications off".to_owned();
    }

    let title = sanitize(title, TITLE_LIMIT);
    let body = sanitize(body, BODY_LIMIT);
    let mut outcomes = vec![format!("native: {}", native(&title, &body, urgent))];
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
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    struct FakeBin {
        dir: PathBuf,
        saved_path: String,
        _guard: MutexGuard<'static, ()>,
    }

    impl FakeBin {
        fn on_path(name: &str) -> FakeBin {
            let guard = PATH_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "claude-nonstop-notify-{}-{name}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            let saved_path = std::env::var("PATH").unwrap_or_default();
            unsafe {
                std::env::set_var("PATH", format!("{}:{saved_path}", dir.display()));
            }
            FakeBin {
                dir,
                saved_path,
                _guard: guard,
            }
        }

        fn install(&self, program: &str, script: &str) {
            let binary = self.dir.join(program);
            fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    impl Drop for FakeBin {
        fn drop(&mut self) {
            unsafe {
                std::env::set_var("PATH", &self.saved_path);
            }
            fs::remove_dir_all(&self.dir).unwrap();
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
    }

    #[test]
    fn send_delivers_a_native_notification() {
        let fake = FakeBin::on_path("native");
        let script = format!("touch '{}/called'", fake.dir.display());
        for program in ["notify-send", "osascript"] {
            fake.install(program, &script);
        }

        let outcome = send(&Notify::default(), "t", "b", false);

        assert_eq!(outcome, "native: delivered");
        assert!(fake.dir.join("called").exists());
    }

    #[test]
    fn ntfy_posts_json_on_curl_stdin_with_token_header() {
        let fake = FakeBin::on_path("curl");
        let script = format!(
            "cat > '{dir}/stdin'\nprintf '%s\\n' \"$@\" > '{dir}/args'",
            dir = fake.dir.display()
        );
        fake.install("curl", &script);
        let settings = Notify {
            ntfy_topic: "alerts".to_owned(),
            ntfy_server: "https://ntfy.example/".to_owned(),
            ntfy_token: "tk_1".to_owned(),
            ntfy_privacy: NtfyPrivacy::Terse,
            ..Notify::default()
        };
        let outcome = ntfy(&settings, "T", "secret", true);
        let stdin = fs::read_to_string(fake.dir.join("stdin")).unwrap();
        let args = fs::read_to_string(fake.dir.join("args")).unwrap();
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
}
