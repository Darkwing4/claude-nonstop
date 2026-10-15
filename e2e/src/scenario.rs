use std::fs::File;
use std::process::{Child, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::artifacts::{self, PreserveOnPanic};
use crate::events::Waiter;
use crate::mock_api::{MockApi, Reply, Request};
use crate::sandbox::{
    Auth, FakeBehavior, HookCall, Notification, Sandbox, SandboxConfig, process_exits_within,
};
use crate::terminal::{Key, ListedPane, Multiplexer, Pane, Screen, Terminal};

pub const STARTUP: Duration = Duration::from_secs(60);
pub const TURN: Duration = Duration::from_secs(60);
const TYPING: Duration = Duration::from_secs(10);

pub struct ScenarioBuilder {
    multiplexer: Multiplexer,
    config: SandboxConfig,
    replies: Vec<Reply>,
    fallback: Option<Reply>,
    install: bool,
}

impl ScenarioBuilder {
    pub fn subscription(mut self) -> Self {
        self.config.auth = Auth::Subscription;
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.config.env.push((key.to_owned(), value.to_owned()));
        self
    }

    pub fn fake_command(mut self, name: &str, behavior: FakeBehavior) -> Self {
        self.config.fake_commands.insert(name.to_owned(), behavior);
        self
    }

    pub fn replies(mut self, replies: impl IntoIterator<Item = Reply>) -> Self {
        self.replies.extend(replies);
        self
    }

    pub fn fallback_reply(mut self, reply: Reply) -> Self {
        self.fallback = Some(reply);
        self
    }

    pub fn login_in_settings(mut self) -> Self {
        self.config.login_in_settings = true;
        self
    }

    pub fn without_install(mut self) -> Self {
        self.install = false;
        self
    }

    pub fn start(self) -> Scenario {
        let name = test_name();
        let mut sandbox = Sandbox::create(&name, &self.config);
        let mock = MockApi::start(&sandbox.mock_dir());
        sandbox.use_api(&mock.base_url());
        mock.enqueue(self.replies);

        if let Some(reply) = self.fallback {
            mock.set_fallback(reply);
        }

        let (waiter, terminal) = {
            let _preserve = PreserveOnPanic::new(&name, &sandbox);

            if self.install {
                sandbox.install_nonstop();
            }

            let waiter = Waiter::watching(&sandbox.root);
            let terminal = Terminal::start(self.multiplexer, &sandbox, &waiter);

            (waiter, terminal)
        };

        Scenario {
            terminal,
            mock,
            sandbox,
            waiter,
            name,
            claude: None,
            daemon: None,
        }
    }
}

pub struct Scenario {
    pub terminal: Terminal,
    pub mock: MockApi,
    pub sandbox: Sandbox,
    waiter: Waiter,
    name: String,
    claude: Option<Pane>,
    daemon: Option<Child>,
}

impl Scenario {
    pub fn builder(multiplexer: Multiplexer) -> ScenarioBuilder {
        ScenarioBuilder {
            multiplexer,
            config: SandboxConfig::default(),
            replies: Vec::new(),
            fallback: None,
            install: true,
        }
    }

    pub fn in_tmux() -> ScenarioBuilder {
        Scenario::builder(Multiplexer::Tmux)
    }

    pub fn in_zellij() -> ScenarioBuilder {
        Scenario::builder(Multiplexer::Zellij)
    }

    pub fn launch_claude(&mut self) -> Pane {
        self.launch_claude_with(&[])
    }

    pub fn launch_claude_with(&mut self, extra_args: &[&str]) -> Pane {
        self.launch_claude_under(&[], extra_args)
    }

    pub fn launch_claude_outside_multiplexer(&mut self) -> Pane {
        let multiplexer_env: &[&str] = match self.terminal.multiplexer() {
            Multiplexer::Tmux => &["TMUX", "TMUX_PANE"],
            Multiplexer::Zellij => &["ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"],
        };
        let mut env_without = vec!["env"];
        for name in multiplexer_env {
            env_without.extend(["-u", name]);
        }

        self.launch_claude_under(&env_without, &[])
    }

    pub fn launch_claude_under(&mut self, wrapper: &[&str], extra_args: &[&str]) -> Pane {
        let starts_before = self.sandbox.hooks_of("SessionStart").len();
        let argv: Vec<String> = wrapper
            .iter()
            .map(|word| (*word).to_owned())
            .chain(self.sandbox.claude_argv(extra_args))
            .collect();
        let pane = self.terminal.launch(&self.sandbox.workdir, &argv);
        self.claude = Some(pane.clone());

        self.wait_for_hooks("SessionStart", starts_before + 1, STARTUP);
        self.wait_for_screen(
            "claude to show an empty prompt",
            STARTUP,
            Screen::is_idle_prompt,
        );

        pane
    }

    pub fn adopt_claude(&mut self, pane: Pane) {
        self.terminal.follow(&pane);
        self.claude = Some(pane);
    }

    pub fn pane_at(&self, address: &Value) -> Option<ListedPane> {
        let is_addressed = |pane: &Pane| match address["mux"].as_str() {
            Some("tmux") => {
                pane.multiplexer == Multiplexer::Tmux && address["pane_id"] == pane.id.as_str()
            }
            Some("zellij") => {
                pane.multiplexer == Multiplexer::Zellij
                    && address["session"] == pane.session.as_str()
                    && address["pane_id"].as_u64() == pane.id.parse().ok()
            }
            _ => false,
        };

        self.terminal
            .panes()
            .into_iter()
            .find(|listed| is_addressed(&listed.pane))
    }

    pub fn claude(&self) -> &Pane {
        self.claude
            .as_ref()
            .expect("claude is not launched: call launch_claude first")
    }

    pub fn screen(&self) -> Screen {
        self.terminal.screen(self.claude())
    }

    pub fn submit(&self, text: &str) {
        let pane = self.claude();
        self.terminal.type_text(pane, text);

        self.wait_for_screen(&format!("{text:?} in the input line"), TYPING, |screen| {
            screen.input_line().as_deref() == Some(collapse(text).as_str())
        });
        self.terminal.press(pane, Key::Enter);
    }

    pub fn session_id(&self) -> String {
        let start = self.wait_for_hook("SessionStart", STARTUP);

        start.payload["session_id"]
            .as_str()
            .expect("SessionStart carries a session id")
            .to_owned()
    }

    pub fn nonstop_setting(&self, key: &str) -> String {
        let output = self.sandbox.nonstop(&["config", "get", key]);
        assert!(
            output.status.success(),
            "claude-nonstop config get {key} failed"
        );

        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    pub fn start_daemon(&mut self) {
        assert!(self.daemon.is_none(), "the daemon is already running");

        let output = self.sandbox.root.join("daemon.out");
        let sink = File::create(&output).expect("create the daemon output file");
        let daemon = self
            .sandbox
            .command(self.sandbox.bin.join("claude-nonstop"))
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(sink.try_clone().expect("share the daemon output file"))
            .stderr(sink)
            .spawn()
            .expect("start claude-nonstop daemon");
        self.daemon = Some(daemon);
    }

    pub fn daemon_pid(&self) -> u32 {
        self.daemon
            .as_ref()
            .expect("the daemon is not running: call start_daemon first")
            .id()
    }

    pub fn wait_for<T>(
        &self,
        what: &str,
        timeout: Duration,
        mut check: impl FnMut(&Scenario) -> Option<T>,
    ) -> T {
        self.waiter.until(what, timeout, || check(self))
    }

    pub fn wait_for_hooks(&self, event: &str, count: usize, timeout: Duration) -> Vec<HookCall> {
        self.wait_for(
            &format!("{count} {event} hook call(s)"),
            timeout,
            |scenario| {
                let hooks = scenario.sandbox.hooks_of(event);
                (hooks.len() >= count).then_some(hooks)
            },
        )
    }

    pub fn wait_for_hook(&self, event: &str, timeout: Duration) -> HookCall {
        self.wait_for_hooks(event, 1, timeout).remove(0)
    }

    pub fn wait_for_request(
        &self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&Request) -> bool,
    ) -> Request {
        self.wait_for(what, timeout, |scenario| {
            scenario
                .mock
                .requests()
                .into_iter()
                .find(|request| matches(request))
        })
    }

    pub fn wait_for_prompt_request(&self, prompt: &str, timeout: Duration) -> Request {
        self.wait_for_request(
            &format!("an API request with the prompt {prompt:?}"),
            timeout,
            |request| request.is_main_loop() && request.last_user_text().as_deref() == Some(prompt),
        )
    }

    pub fn wait_for_log(&self, fragment: &str, timeout: Duration) -> String {
        self.wait_for_log_line(
            &format!("claude-nonstop log line with {fragment:?}"),
            timeout,
            |line| line.contains(fragment),
        )
    }

    pub fn wait_for_log_line(
        &self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        self.wait_for(what, timeout, |scenario| {
            scenario
                .sandbox
                .nonstop_log()
                .lines()
                .find(|line| matches(line))
                .map(str::to_owned)
        })
    }

    pub fn wait_for_screen(
        &self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&Screen) -> bool,
    ) -> Screen {
        self.wait_for(what, timeout, |scenario| {
            let screen = scenario.screen();
            matches(&screen).then_some(screen)
        })
    }

    pub fn wait_for_ledger_session(
        &self,
        session_id: &str,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&Value) -> bool,
    ) -> Value {
        self.wait_for(what, timeout, |scenario| {
            scenario
                .sandbox
                .ledger_session(session_id)
                .filter(|record| matches(record))
        })
    }

    pub fn wait_for_exit(&self, what: &str, pid: u32, timeout: Duration) {
        assert!(
            process_exits_within(pid, timeout),
            "timed out after {}s waiting for {what} (pid {pid}) to exit",
            timeout.as_secs()
        );
    }

    pub fn wait_for_notification(&self, title: &str, timeout: Duration) -> Notification {
        self.wait_for(&format!("notification {title:?}"), timeout, |scenario| {
            scenario
                .sandbox
                .notifications()
                .into_iter()
                .find(|notification| notification.title == title)
        })
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        if std::thread::panicking() {
            artifacts::preserve(
                &self.name,
                &self.sandbox,
                &self.terminal,
                self.claude.as_ref(),
            );
        }

        if let Some(mut daemon) = self.daemon.take() {
            if let Err(error) = daemon.kill().and_then(|()| daemon.wait().map(|_| ())) {
                eprintln!("e2e: claude-nonstop daemon not stopped: {error}");
            }
        }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
}

fn test_name() -> String {
    std::thread::current()
        .name()
        .filter(|name| *name != "main")
        .unwrap_or("scenario")
        .to_owned()
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
