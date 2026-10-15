use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::AgentId;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::ledger::SessionRecord;
use crate::log;
use crate::mux::{self, PaneAddress};
use crate::pane_input::Typing;
use crate::paths::Paths;
use crate::platform::process::spawn_detached;
use crate::screen;

pub const RESUME_SESSION: &str = "claude-nonstop";
const READY_STEP: Duration = Duration::from_secs(2);
const READY_LIMIT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Tmux,
    Zellij,
    Headless,
}

pub fn choose(config: &Config) -> Where {
    match config.general.multiplexer {
        crate::config::MultiplexerChoice::Tmux => Where::Tmux,
        crate::config::MultiplexerChoice::Zellij => Where::Zellij,
        crate::config::MultiplexerChoice::Headless => Where::Headless,
        crate::config::MultiplexerChoice::Auto => {
            if crate::doctor::find_on_path("tmux").is_some() {
                Where::Tmux
            } else if crate::doctor::find_on_path("zellij").is_some() {
                Where::Zellij
            } else {
                Where::Headless
            }
        }
    }
}

pub fn reopen(
    paths: &Paths,
    config: &Config,
    record: &SessionRecord,
    prompt: &str,
) -> Result<PaneAddress> {
    let target = choose(config);
    let session = record.session_id.as_deref();
    let settings = crate::recover::settings_for(config, record.agent);
    let mut argv = launch_argv(record.agent, session, &settings.resume_args);

    match target {
        Where::Headless => {
            argv.push(OsString::from(prompt));
            let name = record
                .session_id
                .clone()
                .unwrap_or_else(|| record.key.clone());

            spawn_detached_headless(paths, &name, &record.cwd, argv)
        }
        Where::Tmux | Where::Zellij => {
            start_in_pane(target, record.agent, &record.cwd, &argv, prompt)
        }
    }
}

pub fn start_in_pane(
    target: Where,
    agent: AgentId,
    cwd: &Path,
    argv: &[OsString],
    prompt: &str,
) -> Result<PaneAddress> {
    let pane = open_in(target, cwd, argv)?;
    let typing = Typing::new(&pane, agent);
    wait_for_prompt(typing.mux.as_ref(), &pane, typing.agent.prompt_glyph())?;
    typing.submit(prompt)?;

    Ok(pane)
}

pub fn launch_argv(
    agent: AgentId,
    session_id: Option<&str>,
    resume_args: &[String],
) -> Vec<OsString> {
    let mut argv = vec![OsString::from(agent.name())];
    match (agent, session_id) {
        (AgentId::Claude, Some(id)) => {
            argv.push(OsString::from("--resume"));
            argv.push(OsString::from(id));
        }
        (AgentId::Claude, None) => argv.push(OsString::from("-c")),
    }

    argv.extend(resume_args.iter().map(OsString::from));

    argv
}

fn open_in(target: Where, cwd: &Path, argv: &[OsString]) -> Result<PaneAddress> {
    match target {
        Where::Tmux => mux::tmux::Tmux::open_window(RESUME_SESSION, cwd, argv),
        Where::Zellij => mux::zellij::Zellij::open_window(RESUME_SESSION, cwd, argv),
        Where::Headless => Err(Error::Invalid(
            "headless launches do not open windows".to_owned(),
        )),
    }
}

pub fn spawn_detached_headless(
    paths: &Paths,
    name: &str,
    cwd: &Path,
    argv: Vec<OsString>,
) -> Result<PaneAddress> {
    let log_path = paths.headless_dir().join(format!("{name}.log"));
    let out = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|source| Error::io(&log_path, "open", source))?;
    let err = out
        .try_clone()
        .map_err(|source| Error::io(&log_path, "open", source))?;

    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::Invalid("empty launch command".to_owned()))?;
    let mut command = Command::new(program);
    command
        .args(rest)
        .current_dir(cwd)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));

    for (env_name, _) in std::env::vars_os() {
        let text = env_name.to_string_lossy();
        if text.starts_with("ZELLIJ") || text.starts_with("TMUX") {
            command.env_remove(env_name);
        }
    }

    let child =
        spawn_detached(&mut command).map_err(|source| Error::io(program, "spawn", source))?;
    log::line(
        name,
        format!(
            "headless process started as pid {}, output in {}",
            child.id(),
            log_path.display()
        ),
    );

    Ok(PaneAddress::Headless { pid: child.id() })
}

fn wait_for_prompt(driver: &dyn mux::Multiplexer, pane: &PaneAddress, glyph: char) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        let visible = driver.capture(pane, false)?;
        let text = screen::strip_ansi(visible.as_bytes());
        if screen::input_line(&text, glyph).is_some() && !screen::is_busy(&text) {
            return Ok(());
        }

        if started.elapsed() >= READY_LIMIT {
            return Err(Error::Invalid(format!(
                "{pane} showed no prompt within {}s",
                READY_LIMIT.as_secs()
            )));
        }

        std::thread::sleep(READY_STEP);
    }
}

pub fn prompt_for(config: &Config, record: &SessionRecord) -> String {
    record.resume_message.clone().unwrap_or_else(|| {
        crate::recover::settings_for(config, record.agent)
            .resume_message
            .clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_argv_selects_the_resume_form_per_agent() {
        let claude = launch_argv(
            AgentId::Claude,
            Some("abc"),
            &["--model".to_owned(), "opus".to_owned()],
        );
        let tail: Vec<String> = claude
            .iter()
            .skip(claude.len() - 4)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert_eq!(tail, vec!["--resume", "abc", "--model", "opus"]);
        assert_eq!(
            launch_argv(AgentId::Claude, None, &[]).last().unwrap(),
            "-c"
        );
    }
}
