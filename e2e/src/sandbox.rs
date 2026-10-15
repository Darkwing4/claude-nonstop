use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, kill_process, pidfd_open};
use serde_json::{Value, json};

use crate::harness_env::harness_env;

const API_KEY: &str = "sk-ant-api03-e2e-mock-key-0000000000000000000000000000AAAA";
const OAUTH_TOKEN: &str = "sk-ant-oat01-e2e-mock-token-00000000000000000000000000BBBB";
const TIME_ZONE: &str = "Asia/Bangkok";
const DUMPED_HOOK_EVENTS: &[&str] = &["SessionStart", "Stop", "StopFailure"];
const KILL_GRACE: Duration = Duration::from_secs(10);
const SANDBOX_NAME_LIMIT: usize = 40;

static NEXT_SANDBOX: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    ApiKey,
    Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeBehavior {
    Succeed,
    Fail,
    Hang,
    Missing,
}

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    pub auth: Auth,
    pub env: Vec<(String, String)>,
    pub fake_commands: BTreeMap<String, FakeBehavior>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        let fake_commands = ["notify-send", "curl", "systemctl"]
            .into_iter()
            .map(|name| (name.to_owned(), FakeBehavior::Succeed))
            .collect();

        SandboxConfig {
            auth: Auth::ApiKey,
            env: Vec::new(),
            fake_commands,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HookCall {
    pub event: String,
    pub payload: Value,
}

#[derive(Clone, Debug)]
pub struct FakeCall {
    pub argv: Vec<String>,
    pub stdin: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub urgent: bool,
}

pub struct Sandbox {
    pub root: PathBuf,
    pub home: PathBuf,
    pub workdir: PathBuf,
    pub claude_config: PathBuf,
    pub state_dir: PathBuf,
    pub bin: PathBuf,
    env: Vec<(String, String)>,
}

impl Sandbox {
    pub fn create(name: &str, config: &SandboxConfig) -> Sandbox {
        let number = NEXT_SANDBOX.fetch_add(1, Ordering::Relaxed);
        let root = harness_env().sandboxes_dir.join(format!(
            "{}-{}-{number}",
            short_name(name),
            std::process::id()
        ));
        let home = root.join("home");
        let mut sandbox = Sandbox {
            workdir: root.join("work"),
            claude_config: home.join(".claude"),
            state_dir: home.join(".local/state/claude-nonstop"),
            bin: home.join(".local/bin"),
            env: Vec::new(),
            home,
            root,
        };

        sandbox.create_dirs();
        sandbox.env = sandbox.base_env(config);
        sandbox.install_binaries(&config.fake_commands);
        sandbox.write_claude_state(config.auth);
        sandbox.write_hook_dumpers();

        sandbox
    }

    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    pub fn use_api(&mut self, base_url: &str) {
        self.env.retain(|(key, _)| key != "ANTHROPIC_BASE_URL");
        self.env
            .push(("ANTHROPIC_BASE_URL".to_owned(), base_url.to_owned()));
    }

    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .current_dir(&self.workdir);

        command
    }

    pub fn nonstop(&self, args: &[&str]) -> Output {
        self.command(self.bin.join("claude-nonstop"))
            .args(args)
            .output()
            .expect("run claude-nonstop")
    }

    pub fn install_nonstop(&self) {
        let output = self.nonstop(&["install"]);

        assert!(
            output.status.success(),
            "claude-nonstop install failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub fn claude_argv(&self, extra: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "claude".to_owned(),
            "--debug-file".to_owned(),
            self.debug_log().display().to_string(),
        ];
        argv.extend(extra.iter().map(|arg| (*arg).to_owned()));

        argv
    }

    pub fn debug_log(&self) -> PathBuf {
        self.root.join("claude-debug.log")
    }

    pub fn hooks_dir(&self) -> PathBuf {
        self.root.join("hooks")
    }

    pub fn calls_dir(&self) -> PathBuf {
        self.root.join("calls")
    }

    pub fn mock_dir(&self) -> PathBuf {
        self.root.join("mock")
    }

    pub fn screens_dir(&self) -> PathBuf {
        self.root.join("screens")
    }

    pub fn settings_path(&self) -> PathBuf {
        self.claude_config.join("settings.json")
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.state_dir.join("ledger.json")
    }

    pub fn nonstop_log_path(&self) -> PathBuf {
        self.state_dir.join("log/claude-nonstop.log")
    }

    pub fn settings(&self) -> Value {
        read_json(&self.settings_path()).unwrap_or_else(|| json!({}))
    }

    pub fn write_settings(&self, settings: &Value) {
        write_json(&self.settings_path(), settings);
    }

    pub fn hooks(&self) -> Vec<HookCall> {
        let mut files: Vec<PathBuf> = list_dir(&self.hooks_dir())
            .into_iter()
            .filter(|path| path.extension() == Some(OsStr::new("json")))
            .collect();
        files.sort();

        files
            .iter()
            .filter_map(|path| read_json(path))
            .map(|payload| HookCall {
                event: payload["hook_event_name"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned(),
                payload,
            })
            .collect()
    }

    pub fn hooks_of(&self, event: &str) -> Vec<HookCall> {
        self.hooks()
            .into_iter()
            .filter(|hook| hook.event == event)
            .collect()
    }

    pub fn calls(&self, program: &str) -> Vec<FakeCall> {
        let log = self.calls_dir().join(format!("{program}.jsonl"));

        read_lines(&log)
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).expect("call log line is JSON"))
            .map(|value| FakeCall {
                argv: value["argv"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                stdin: value["stdin"].as_str().unwrap_or_default().to_owned(),
            })
            .collect()
    }

    pub fn notifications(&self) -> Vec<Notification> {
        self.calls("notify-send")
            .into_iter()
            .filter_map(|call| match call.argv.as_slice() {
                [.., urgency, title, body] => Some(Notification {
                    title: title.clone(),
                    body: body.clone(),
                    urgent: urgency == "critical",
                }),
                _ => None,
            })
            .collect()
    }

    pub fn ledger(&self) -> Option<Value> {
        read_json(&self.ledger_path())
    }

    pub fn ledger_session(&self, session_id: &str) -> Option<Value> {
        self.ledger()
            .map(|ledger| ledger["sessions"][session_id].clone())
            .filter(|record| !record.is_null())
    }

    pub fn nonstop_log(&self) -> String {
        read_text(&self.nonstop_log_path()).unwrap_or_default()
    }

    pub fn cutoff_counter(&self, session_id: &str) -> Option<Value> {
        read_json(
            &self
                .state_dir
                .join("cutoff")
                .join(format!("{session_id}.json")),
        )
    }

    pub fn transcript(&self, session_id: &str) -> Option<PathBuf> {
        list_dir(&self.claude_config.join("projects"))
            .into_iter()
            .map(|project| project.join(format!("{session_id}.jsonl")))
            .find(|path| path.is_file())
    }

    pub fn kill_processes(&self) {
        loop {
            let victims = self.own_processes();
            if victims.is_empty() {
                return;
            }

            let exits: Vec<OwnedFd> = victims.iter().filter_map(|pid| kill(*pid)).collect();
            wait_for_exits(&exits);
        }
    }

    fn own_processes(&self) -> Vec<Pid> {
        let marker = format!("HOME={}", self.home.display()).into_bytes();
        let me = std::process::id();

        list_dir(Path::new("/proc"))
            .into_iter()
            .filter_map(|entry| entry.file_name()?.to_str()?.parse::<u32>().ok())
            .filter(|pid| *pid != me)
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                    environ
                        .split(|byte| *byte == 0)
                        .any(|entry| entry == marker)
                })
            })
            .filter_map(|pid| Pid::from_raw(pid as i32))
            .collect()
    }

    fn create_dirs(&self) {
        for dir in [
            &self.root,
            &self.home,
            &self.workdir,
            &self.claude_config,
            &self.state_dir,
            &self.bin,
            &self.home.join(".config"),
            &self.root.join("tmp"),
            &self.hooks_dir(),
            &self.calls_dir(),
            &self.mock_dir(),
            &self.screens_dir(),
        ] {
            std::fs::create_dir_all(dir)
                .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
        }

        let runtime = self.root.join("run");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .unwrap_or_else(|error| panic!("create {}: {error}", runtime.display()));
    }

    fn base_env(&self, config: &SandboxConfig) -> Vec<(String, String)> {
        let path = |path: &Path| path.display().to_string();
        let mut env: BTreeMap<String, String> = [
            (
                "PATH",
                format!("{}:/usr/local/bin:/usr/bin:/bin", path(&self.bin)),
            ),
            ("HOME", path(&self.home)),
            ("SHELL", "/bin/bash".to_owned()),
            ("TERM", "xterm-256color".to_owned()),
            ("LANG", "C.UTF-8".to_owned()),
            ("TZ", TIME_ZONE.to_owned()),
            ("TMPDIR", path(&self.root.join("tmp"))),
            ("XDG_RUNTIME_DIR", path(&self.root.join("run"))),
            ("TMUX_TMPDIR", path(&self.root.join("run"))),
            ("XDG_CONFIG_HOME", path(&self.home.join(".config"))),
            ("XDG_STATE_HOME", path(&self.home.join(".local/state"))),
            ("XDG_DATA_HOME", path(&self.home.join(".local/share"))),
            ("XDG_CACHE_HOME", path(&self.home.join(".cache"))),
            ("CLAUDE_CONFIG_DIR", path(&self.claude_config)),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
            ("DISABLE_AUTOUPDATER", "1".to_owned()),
            ("DISABLE_TELEMETRY", "1".to_owned()),
            ("DISABLE_ERROR_REPORTING", "1".to_owned()),
            (
                "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
                "1".to_owned(),
            ),
            ("CLAUDE_NONSTOP_LIMIT_MARGIN_SECONDS", "0".to_owned()),
            ("CLAUDE_NONSTOP_LIMIT_VERIFY_SECONDS", "1".to_owned()),
            ("CLAUDE_NONSTOP_OVERLOAD_BACKOFF_SECONDS", "1,2".to_owned()),
            ("CLAUDE_NONSTOP_OVERLOAD_JITTER_PERCENT", "0".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();

        match config.auth {
            Auth::ApiKey => {
                env.insert("ANTHROPIC_API_KEY".to_owned(), API_KEY.to_owned());
            }
            Auth::Subscription => {
                env.insert("ANTHROPIC_API_KEY".to_owned(), String::new());
                env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), OAUTH_TOKEN.to_owned());
                env.insert("CLAUDE_CODE_SUBSCRIPTION_TYPE".to_owned(), "max".to_owned());
            }
        }

        env.extend(config.env.iter().cloned());

        env.into_iter().collect()
    }

    fn install_binaries(&self, fakes: &BTreeMap<String, FakeBehavior>) {
        let harness = harness_env();
        link(&harness.claude_bin, &self.bin.join("claude"));
        link(&harness.nonstop_bin, &self.bin.join("claude-nonstop"));

        let recorder = quoted(&harness.recorder_bin);
        let hooks = quoted(&self.hooks_dir());
        write_script(
            &self.bin.join("hook-dump"),
            &format!("exec {recorder} hook {hooks}"),
        );

        for (name, behavior) in fakes {
            let behavior = match behavior {
                FakeBehavior::Missing => continue,
                FakeBehavior::Succeed => "succeed",
                FakeBehavior::Fail => "fail",
                FakeBehavior::Hang => "hang",
            };
            let log = quoted(&self.calls_dir().join(format!("{name}.jsonl")));

            write_script(
                &self.bin.join(name),
                &format!("exec {recorder} call {log} {behavior} \"$@\""),
            );
        }
    }

    fn write_claude_state(&self, auth: Auth) {
        let approved_key = &API_KEY[API_KEY.len() - 20..];
        let mut projects = serde_json::Map::new();
        projects.insert(
            self.workdir.display().to_string(),
            json!({"hasTrustDialogAccepted": true}),
        );
        let mut state = json!({
            "hasCompletedOnboarding": true,
            "hasSeenAutoDefaultNotice": true,
            "projects": projects,
        });

        if auth == Auth::ApiKey {
            state["customApiKeyResponses"] = json!({"approved": [approved_key], "rejected": []});
        }

        write_json(&self.claude_config.join(".claude.json"), &state);
    }

    fn write_hook_dumpers(&self) {
        let dumper = self.bin.join("hook-dump").display().to_string();
        let hooks: serde_json::Map<String, Value> = DUMPED_HOOK_EVENTS
            .iter()
            .map(|event| {
                let group = json!([{"hooks": [{"type": "command", "command": dumper}]}]);
                ((*event).to_owned(), group)
            })
            .collect();
        self.write_settings(&json!({ "hooks": hooks }));
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.kill_processes();

        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            eprintln!("e2e: sandbox {} not removed: {error}", self.root.display());
        }
    }
}

fn kill(pid: Pid) -> Option<OwnedFd> {
    let exit = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(Errno::SRCH) => return None,
        Err(error) => panic!("pidfd_open {pid:?} failed: {error}"),
    };

    match kill_process(pid, Signal::KILL) {
        Ok(()) => Some(exit),
        Err(Errno::SRCH) => None,
        Err(error) => panic!("kill {pid:?} failed: {error}"),
    }
}

fn wait_for_exits(exits: &[OwnedFd]) {
    let timeout = Timespec {
        tv_sec: KILL_GRACE.as_secs() as i64,
        tv_nsec: 0,
    };

    for exit in exits {
        let mut fds = [PollFd::new(exit, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(1) => {}
            Ok(_) => panic!(
                "a sandbox process survived SIGKILL for {}s",
                KILL_GRACE.as_secs()
            ),
            Err(error) => panic!("waiting for a killed sandbox process failed: {error}"),
        }
    }
}

fn short_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let start = cleaned.len().saturating_sub(SANDBOX_NAME_LIMIT);

    cleaned[start..].to_owned()
}

fn quoted(path: &Path) -> String {
    let text = path.display().to_string();
    assert!(!text.contains('\''), "path {text} cannot be single-quoted");

    format!("'{text}'")
}

fn link(target: &Path, at: &Path) {
    symlink(target, at)
        .unwrap_or_else(|error| panic!("link {} -> {}: {error}", at.display(), target.display()));
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n"))
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

fn write_json(path: &Path, value: &Value) {
    let text = serde_json::to_string_pretty(value).expect("JSON serializes");
    std::fs::write(path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn read_text(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let text = read_text(path)?;

    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => panic!("{} is not JSON: {error}", path.display()),
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    read_text(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn list_dir(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("list {}: {error}", dir.display()),
    }
}
