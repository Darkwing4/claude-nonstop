mod tmux;
mod zellij;
mod zellij_client;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::events::Waiter;
use crate::sandbox::Sandbox;

pub const COLUMNS: u16 = 160;
pub const ROWS: u16 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Multiplexer {
    Tmux,
    Zellij,
}

impl Multiplexer {
    pub fn name(self) -> &'static str {
        match self {
            Multiplexer::Tmux => "tmux",
            Multiplexer::Zellij => "zellij",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub multiplexer: Multiplexer,
    pub session: String,
    pub id: String,
}

impl Pane {
    pub fn file_stem(&self) -> String {
        format!("{}-{}", self.session, self.id.trim_start_matches('%'))
    }
}

impl fmt::Display for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.multiplexer {
            Multiplexer::Tmux => write!(f, "tmux {}", self.id),
            Multiplexer::Zellij => write!(f, "zellij {}#{}", self.session, self.id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedPane {
    pub pane: Pane,
    pub command: Option<String>,
    pub alive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    Up,
    Down,
    CtrlC,
}

#[derive(Clone, Debug)]
pub struct Screen {
    text: String,
}

impl Screen {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    pub fn input_line(&self) -> Option<String> {
        let lines: Vec<&str> = self.text.lines().map(str::trim).collect();
        let start = lines.iter().rposition(|line| line.starts_with('❯'))?;
        let continuation = lines[start + 1..]
            .iter()
            .take_while(|line| !line.is_empty() && !line.starts_with('─'));
        let words: Vec<&str> = std::iter::once(&lines[start]['❯'.len_utf8()..])
            .chain(continuation.copied())
            .flat_map(str::split_whitespace)
            .collect();

        Some(words.join(" "))
    }

    pub fn is_busy(&self) -> bool {
        self.contains("esc to interrupt") || self.contains("Retrying in")
    }

    pub fn is_idle_prompt(&self) -> bool {
        self.input_line().is_some_and(|line| line.is_empty()) && !self.is_busy()
    }

    pub fn tail(&self, count: usize) -> String {
        let lines: Vec<&str> = self
            .text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .collect();

        lines[lines.len().saturating_sub(count)..].join("\n")
    }
}

impl fmt::Display for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

trait Driver {
    fn launch(&mut self, cwd: &Path, argv: &[String]) -> Pane;
    fn follow(&mut self, pane: &Pane, feed: &Path);
    fn capture(&self, pane: &Pane, scrollback: bool) -> Result<String, String>;
    fn type_text(&self, pane: &Pane, text: &str);
    fn press(&self, pane: &Pane, key: Key);
    fn close(&self, pane: &Pane);
    fn panes(&self) -> Vec<ListedPane>;
    fn shutdown(&mut self);
}

pub struct Terminal {
    multiplexer: Multiplexer,
    driver: Box<dyn Driver>,
    screens_dir: PathBuf,
    followed: Vec<Pane>,
}

impl Terminal {
    pub fn start(multiplexer: Multiplexer, sandbox: &Sandbox, waiter: &Waiter) -> Terminal {
        let env = sandbox.env().to_vec();
        let screens_dir = sandbox.screens_dir();
        let driver: Box<dyn Driver> = match multiplexer {
            Multiplexer::Tmux => Box::new(tmux::Tmux::new(env)),
            Multiplexer::Zellij => Box::new(zellij::Zellij::start(env, &screens_dir, waiter)),
        };

        Terminal {
            multiplexer,
            driver,
            screens_dir,
            followed: Vec::new(),
        }
    }

    pub fn multiplexer(&self) -> Multiplexer {
        self.multiplexer
    }

    pub fn launch(&mut self, cwd: &Path, argv: &[String]) -> Pane {
        let pane = self.driver.launch(cwd, argv);
        self.follow(&pane);

        pane
    }

    pub fn follow(&mut self, pane: &Pane) {
        if self.followed.contains(pane) {
            return;
        }

        let feed = self.screens_dir.join(format!("{}.feed", pane.file_stem()));
        self.driver.follow(pane, &feed);
        self.followed.push(pane.clone());
    }

    pub fn followed(&self) -> &[Pane] {
        &self.followed
    }

    pub fn screen(&self, pane: &Pane) -> Screen {
        match self.driver.capture(pane, false) {
            Ok(text) => Screen { text },
            Err(error) => panic!("cannot capture {pane}: {error}"),
        }
    }

    pub fn try_scrollback(&self, pane: &Pane) -> Result<Screen, String> {
        self.driver.capture(pane, true).map(|text| Screen { text })
    }

    pub fn type_text(&self, pane: &Pane, text: &str) {
        self.driver.type_text(pane, text);
    }

    pub fn press(&self, pane: &Pane, key: Key) {
        self.driver.press(pane, key);
    }

    pub fn close(&self, pane: &Pane) {
        self.driver.close(pane);
    }

    pub fn panes(&self) -> Vec<ListedPane> {
        self.driver.panes()
    }

    pub fn is_alive(&self, pane: &Pane) -> bool {
        self.panes()
            .iter()
            .any(|listed| listed.pane == *pane && listed.alive)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.driver.shutdown();
    }
}

fn command(env: &[(String, String)], program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)));

    command
}

struct Finished {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Finished {
    fn into_result(self) -> Result<String, String> {
        if self.success {
            Ok(self.stdout)
        } else {
            Err(self.stderr.trim().to_owned())
        }
    }
}

fn run(command: &mut Command) -> Finished {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{command:?} did not start: {error}"));

    Finished {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run_checked(command: &mut Command) -> String {
    let finished = run(command);

    match finished.into_result() {
        Ok(stdout) => stdout,
        Err(stderr) => panic!("{command:?} failed: {stderr}"),
    }
}
