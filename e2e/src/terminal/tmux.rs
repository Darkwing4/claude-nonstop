use std::path::Path;
use std::process::Command;

use super::{COLUMNS, Driver, Key, ListedPane, Multiplexer, Pane, ROWS, command, run, run_checked};

const SESSION: &str = "e2e";

pub struct Tmux {
    env: Vec<(String, String)>,
}

impl Tmux {
    pub fn new(env: Vec<(String, String)>) -> Tmux {
        Tmux { env }
    }

    fn tmux(&self) -> Command {
        let mut tmux = command(&self.env, "tmux");
        tmux.args(["-f", "/dev/null"]);

        tmux
    }

    fn has_session(&self) -> bool {
        run(self.tmux().args(["has-session", "-t", SESSION])).success
    }
}

impl Driver for Tmux {
    fn launch(&mut self, cwd: &Path, argv: &[String]) -> Pane {
        let mut tmux = self.tmux();

        if self.has_session() {
            tmux.args(["new-window", "-t", &format!("{SESSION}:")]);
        } else {
            let (columns, rows) = (COLUMNS.to_string(), ROWS.to_string());
            tmux.args([
                "new-session",
                "-d",
                "-s",
                SESSION,
                "-x",
                &columns,
                "-y",
                &rows,
            ]);
        }

        tmux.args(["-P", "-F", "#{pane_id}", "-c"])
            .arg(cwd)
            .arg("--")
            .args(argv);
        let id = run_checked(&mut tmux).trim().to_owned();
        assert!(id.starts_with('%'), "tmux reported pane id {id:?}");

        Pane {
            multiplexer: Multiplexer::Tmux,
            session: SESSION.to_owned(),
            id,
        }
    }

    fn follow(&mut self, pane: &Pane, feed: &Path) {
        let sink = format!("cat >> '{}'", feed.display());
        run_checked(self.tmux().args(["pipe-pane", "-o", "-t", &pane.id, &sink]));
    }

    fn capture(&self, pane: &Pane, scrollback: bool) -> Result<String, String> {
        let mut tmux = self.tmux();
        tmux.args(["capture-pane", "-p", "-t", &pane.id]);

        if scrollback {
            tmux.args(["-S", "-"]);
        }

        run(&mut tmux).into_result()
    }

    fn type_text(&self, pane: &Pane, text: &str) {
        run_checked(
            self.tmux()
                .args(["send-keys", "-t", &pane.id, "-l", "--", text]),
        );
    }

    fn press(&self, pane: &Pane, key: Key) {
        let name = match key {
            Key::Enter => "Enter",
            Key::Escape => "Escape",
            Key::Up => "Up",
            Key::Down => "Down",
            Key::CtrlC => "C-c",
        };

        run_checked(self.tmux().args(["send-keys", "-t", &pane.id, name]));
    }

    fn close(&self, pane: &Pane) {
        run_checked(self.tmux().args(["kill-pane", "-t", &pane.id]));
    }

    fn panes(&self) -> Vec<ListedPane> {
        let format = "#{session_name}\t#{pane_id}\t#{pane_current_command}\t#{pane_dead}";
        let listing = run(self.tmux().args(["list-panes", "-a", "-F", format]));

        if !listing.success {
            assert!(
                is_no_server(&listing.stderr),
                "tmux list-panes failed: {}",
                listing.stderr.trim()
            );
            return Vec::new();
        }

        listing
            .stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let session = fields.next()?.to_owned();
                let id = fields.next()?.to_owned();
                let command = fields
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                let alive = fields.next() == Some("0");

                Some(ListedPane {
                    pane: Pane {
                        multiplexer: Multiplexer::Tmux,
                        session,
                        id,
                    },
                    command,
                    alive,
                })
            })
            .collect()
    }

    fn shutdown(&mut self) {
        let killed = run(self.tmux().arg("kill-server"));

        if !killed.success && !is_no_server(&killed.stderr) {
            eprintln!("e2e: tmux kill-server failed: {}", killed.stderr.trim());
        }
    }
}

fn is_no_server(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("error connecting to")
}
