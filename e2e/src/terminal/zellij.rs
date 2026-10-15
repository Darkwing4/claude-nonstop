use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use super::zellij_client::ZellijClient;
use super::zellij_pane_feed::PaneFeed;
use super::{COLUMNS, Driver, Finished, Key, ListedPane, Multiplexer, Pane, ROWS, command, run};
use crate::events::Waiter;

const SESSION: &str = "e2e";
const CONFIG: &str = "pane_frames false\nshow_startup_tips false\nshow_release_notes false\nsession_serialization false\n";
const DEFAULT_LAYOUT: &str = "layout {\n    pane\n}\n";
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_ATTEMPTS: usize = 3;

pub struct Zellij {
    env: Vec<(String, String)>,
    client: ZellijClient,
    feeds: Vec<PaneFeed>,
    launched: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    WhenSessionNotFound,
    WhenSessionNotFoundOrSilent,
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
            launched: Vec::new(),
        }
    }

    fn zellij(&self) -> Command {
        command(&self.env, "zellij")
    }

    fn call(&self, repeat: Repeat, arguments: impl Fn(&mut Command) -> &mut Command) -> Finished {
        let mut finished = run(arguments(&mut self.zellij()));

        for _ in 1..CALL_ATTEMPTS {
            let not_found = !finished.success && is_no_session(&finished.stderr);
            let silent = finished.success && finished.stdout.trim().is_empty();
            let may_repeat_silent = repeat == Repeat::WhenSessionNotFoundOrSilent;

            if !not_found && !(silent && may_repeat_silent) {
                break;
            }

            finished = run(arguments(&mut self.zellij()));
        }

        finished
    }

    fn call_checked(&self, arguments: impl Fn(&mut Command) -> &mut Command) -> String {
        let finished = self.call(Repeat::WhenSessionNotFound, &arguments);

        match finished.into_result() {
            Ok(stdout) => stdout,
            Err(stderr) => panic!("{:?} failed: {stderr}", arguments(&mut self.zellij())),
        }
    }

    fn act(&self, repeat: Repeat, session: &str, arguments: &[&str]) -> Finished {
        self.call(repeat, |zellij| {
            zellij
                .args(["--session", session, "action"])
                .args(arguments)
        })
    }

    fn act_checked(&self, session: &str, arguments: &[&str]) -> String {
        self.call_checked(|zellij| {
            zellij
                .args(["--session", session, "action"])
                .args(arguments)
        })
    }

    fn sessions(&self) -> Vec<String> {
        let listing = self.call(Repeat::WhenSessionNotFoundOrSilent, |zellij| {
            zellij.args(["list-sessions", "--short", "--no-formatting"])
        });

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
        let listing = self.act(
            Repeat::WhenSessionNotFoundOrSilent,
            session,
            &["list-panes", "--all", "--json"],
        );

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

    fn unclaimed_floating_pane(&self) -> String {
        let unclaimed: Vec<String> = self
            .rows(SESSION)
            .iter()
            .filter(|row| row["is_plugin"] == false && row["is_floating"] == true)
            .map(|row| row["id"].to_string())
            .filter(|id| !self.launched.contains(id))
            .collect();

        match unclaimed.as_slice() {
            [id] => id.clone(),
            _ => panic!(
                "zellij run printed no pane id and the session has {unclaimed:?} as new panes"
            ),
        }
    }
}

impl Driver for Zellij {
    fn launch(&mut self, cwd: &Path, argv: &[String]) -> Pane {
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
        let reported = self.call_checked(|zellij| {
            zellij
                .args(["--session", SESSION, "run"])
                .args(full_screen_floating)
                .arg("--cwd")
                .arg(cwd)
                .arg("--")
                .args(argv)
        });
        let id = match reported.trim().strip_prefix("terminal_") {
            Some(id) => id.to_owned(),
            None if reported.trim().is_empty() => self.unclaimed_floating_pane(),
            None => panic!("zellij run reported {reported:?}"),
        };
        self.launched.push(id.clone());

        Pane {
            multiplexer: Multiplexer::Zellij,
            session: SESSION.to_owned(),
            id,
        }
    }

    fn follow(&mut self, pane: &Pane, feed: &Path) {
        let env = self.env.clone();
        let (session, id) = (pane.session.clone(), pane.id.clone());
        let subscribe = move || {
            let mut zellij = command(&env, "zellij");
            zellij.args(["--session", &session, "subscribe", "--pane-id", &id]);

            zellij
        };

        self.feeds.push(PaneFeed::follow(pane, subscribe, feed));
    }

    fn capture(&self, pane: &Pane, scrollback: bool) -> Result<String, String> {
        let live = self.feeds.iter().find(|feed| feed.pane() == pane);

        if let Some(feed) = live.filter(|_| !scrollback) {
            return Ok(feed.viewport());
        }

        let mut dump = vec!["dump-screen", "--pane-id", &pane.id];

        if scrollback {
            dump.push("--full");
        }

        self.act(Repeat::WhenSessionNotFoundOrSilent, &pane.session, &dump)
            .into_result()
    }

    fn type_text(&self, pane: &Pane, text: &str) {
        self.act_checked(
            &pane.session,
            &["write-chars", "--pane-id", &pane.id, "--", text],
        );
    }

    fn press(&self, pane: &Pane, key: Key) {
        let bytes: &[&str] = match key {
            Key::Enter => &["13"],
            Key::Escape => &["27"],
            Key::Up => &["27", "91", "65"],
            Key::Down => &["27", "91", "66"],
            Key::CtrlC => &["3"],
        };

        let write = [&["write", "--pane-id", &pane.id][..], bytes].concat();
        self.act_checked(&pane.session, &write);
    }

    fn close(&self, pane: &Pane) {
        self.act_checked(&pane.session, &["close-pane", "--pane-id", &pane.id]);
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
        for feed in &mut self.feeds {
            feed.stop();
        }

        let killed = self.call(Repeat::WhenSessionNotFound, |zellij| {
            zellij.args(["kill-all-sessions", "--yes"])
        });
        if !killed.success && !is_no_session(&killed.stderr) {
            eprintln!(
                "e2e: zellij kill-all-sessions failed: {}",
                killed.stderr.trim()
            );
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
