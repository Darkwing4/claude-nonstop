use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::zellij_client::ZellijClient;
use super::{COLUMNS, Driver, Key, ListedPane, Multiplexer, Pane, ROWS, command, run, run_checked};
use crate::events::Waiter;

const SESSION: &str = "e2e";
const CONFIG: &str = "pane_frames false\nshow_startup_tips false\nshow_release_notes false\nsession_serialization false\n";
const DEFAULT_LAYOUT: &str = "layout {\n    pane\n}\n";
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Zellij {
    env: Vec<(String, String)>,
    client: ZellijClient,
    feeds: Vec<Child>,
}

impl Zellij {
    pub fn start(env: Vec<(String, String)>, screens_dir: &Path, waiter: &Waiter) -> Zellij {
        let config_dir = write_config(&env);
        let size_report = screens_dir.join("zellij-first-pane.size");
        let layout = config_dir.join("e2e-layout.kdl");
        std::fs::write(&layout, size_reporting_layout(&size_report))
            .expect("write the e2e zellij layout");

        let mut attach = command(&env, "setsid");
        attach
            .args([
                "--ctty",
                "zellij",
                "--session",
                SESSION,
                "--new-session-with-layout",
            ])
            .arg(&layout);
        let client = ZellijClient::attach(
            attach,
            COLUMNS,
            ROWS,
            &screens_dir.join("zellij-client.feed"),
        );

        waiter.until(
            &format!("zellij session {SESSION} with a {COLUMNS}-column first pane"),
            ATTACH_TIMEOUT,
            || reported_columns(&size_report).filter(|columns| *columns == COLUMNS),
        );

        Zellij {
            env,
            client,
            feeds: Vec::new(),
        }
    }

    fn zellij(&self) -> Command {
        command(&self.env, "zellij")
    }

    fn action(&self, session: &str) -> Command {
        let mut zellij = self.zellij();
        zellij.args(["--session", session, "action"]);

        zellij
    }

    fn sessions(&self) -> Vec<String> {
        let listing = run(self
            .zellij()
            .args(["list-sessions", "--short", "--no-formatting"]));

        if !listing.success {
            assert!(
                is_no_session(&listing.stderr),
                "zellij list-sessions failed: {}",
                listing.stderr.trim()
            );
            return Vec::new();
        }

        listing
            .stdout
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn rows(&self, session: &str) -> Vec<Value> {
        let listing = run(self.action(session).args(["list-panes", "--all", "--json"]));

        if !listing.success {
            assert!(
                is_no_session(&listing.stderr),
                "zellij list-panes failed: {}",
                listing.stderr.trim()
            );
            return Vec::new();
        }

        match serde_json::from_str::<Value>(&listing.stdout) {
            Ok(Value::Array(rows)) => rows,
            Ok(other) => panic!("zellij list-panes printed {other}"),
            Err(error) => panic!("zellij list-panes printed non-JSON: {error}"),
        }
    }
}

impl Driver for Zellij {
    fn launch(&mut self, cwd: &Path, argv: &[String]) -> Pane {
        let mut zellij = self.zellij();
        let full_screen_floating = [
            "--floating",
            "-x",
            "0",
            "-y",
            "0",
            "--width",
            "100%",
            "--height",
            "100%",
        ];
        zellij
            .args(["--session", SESSION, "run"])
            .args(full_screen_floating)
            .arg("--cwd")
            .arg(cwd)
            .arg("--")
            .args(argv);

        let reported = run_checked(&mut zellij);
        let id = reported
            .trim()
            .strip_prefix("terminal_")
            .unwrap_or_else(|| panic!("zellij run reported {reported:?}"))
            .to_owned();

        Pane {
            multiplexer: Multiplexer::Zellij,
            session: SESSION.to_owned(),
            id,
        }
    }

    fn follow(&mut self, pane: &Pane, feed: &Path) {
        let sink = OpenOptions::new()
            .create(true)
            .append(true)
            .open(feed)
            .unwrap_or_else(|error| panic!("open {}: {error}", feed.display()));

        let subscriber = self
            .zellij()
            .args([
                "--session",
                &pane.session,
                "subscribe",
                "--pane-id",
                &pane.id,
            ])
            .stdin(Stdio::null())
            .stdout(sink)
            .stderr(Stdio::null())
            .spawn()
            .expect("start zellij subscribe");
        self.feeds.push(subscriber);
    }

    fn capture(&self, pane: &Pane, scrollback: bool) -> Result<String, String> {
        let mut zellij = self.action(&pane.session);
        zellij.args(["dump-screen", "--pane-id", &pane.id]);

        if scrollback {
            zellij.arg("--full");
        }

        run(&mut zellij).into_result()
    }

    fn type_text(&self, pane: &Pane, text: &str) {
        run_checked(self.action(&pane.session).args([
            "write-chars",
            "--pane-id",
            &pane.id,
            "--",
            text,
        ]));
    }

    fn press(&self, pane: &Pane, key: Key) {
        let bytes: &[&str] = match key {
            Key::Enter => &["13"],
            Key::Escape => &["27"],
            Key::Up => &["27", "91", "65"],
            Key::Down => &["27", "91", "66"],
            Key::CtrlC => &["3"],
        };

        run_checked(
            self.action(&pane.session)
                .args(["write", "--pane-id", &pane.id])
                .args(bytes),
        );
    }

    fn close(&self, pane: &Pane) {
        run_checked(
            self.action(&pane.session)
                .args(["close-pane", "--pane-id", &pane.id]),
        );
    }

    fn panes(&self) -> Vec<ListedPane> {
        let mut listed = Vec::new();

        for session in self.sessions() {
            for row in self.rows(&session) {
                if row["is_plugin"] == false {
                    listed.push(listed_pane(&session, &row));
                }
            }
        }

        listed
    }

    fn shutdown(&mut self) {
        let killed = run(self.zellij().args(["kill-all-sessions", "--yes"]));
        if !killed.success && !is_no_session(&killed.stderr) {
            eprintln!(
                "e2e: zellij kill-all-sessions failed: {}",
                killed.stderr.trim()
            );
        }

        for feed in &mut self.feeds {
            if let Err(error) = feed.kill().and_then(|()| feed.wait().map(|_| ())) {
                eprintln!("e2e: zellij subscribe not stopped: {error}");
            }
        }

        self.client.stop();
    }
}

fn listed_pane(session: &str, row: &Value) -> ListedPane {
    let program = row["pane_command"]
        .as_str()
        .and_then(|line| line.split_whitespace().next());
    let command = program.map(|path| path.rsplit('/').next().unwrap_or(path).to_owned());

    ListedPane {
        pane: Pane {
            multiplexer: Multiplexer::Zellij,
            session: session.to_owned(),
            id: row["id"].to_string(),
        },
        command,
        alive: row["exited"] == false,
    }
}

fn is_no_session(stderr: &str) -> bool {
    [
        "No active zellij sessions found",
        "There is no active session",
        "not found. The following sessions are active",
    ]
    .iter()
    .any(|message| stderr.contains(message))
}

fn write_config(env: &[(String, String)]) -> PathBuf {
    let config_dir = env
        .iter()
        .find(|(key, _)| key == "XDG_CONFIG_HOME")
        .map(|(_, value)| Path::new(value).join("zellij"))
        .expect("sandbox env sets XDG_CONFIG_HOME");
    let layouts = config_dir.join("layouts");

    std::fs::create_dir_all(&layouts).expect("create the zellij config directory");
    std::fs::write(config_dir.join("config.kdl"), CONFIG).expect("write the zellij config");
    std::fs::write(layouts.join("default.kdl"), DEFAULT_LAYOUT)
        .expect("write the zellij default layout");

    config_dir
}

fn size_reporting_layout(size_report: &Path) -> String {
    let report = size_report.display();
    let script = format!(
        "report() {{ stty size > '{report}'; }}; trap report WINCH; report; \
         while :; do sleep 86400 & wait $!; done"
    );

    format!("layout {{\n    pane command=\"sh\" {{\n        args \"-c\" \"{script}\"\n    }}\n}}\n")
}

fn reported_columns(size_report: &Path) -> Option<u16> {
    let size = match std::fs::read_to_string(size_report) {
        Ok(size) => size,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("read {}: {error}", size_report.display()),
    };

    size.split_whitespace().nth(1)?.parse().ok()
}
