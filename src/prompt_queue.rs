use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::agent;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::ledger::{self, AgentId, PromptStatus, PromptWhen, QueuedPrompt, Status, now_unix};
use crate::log;
use crate::mux::{self, PaneAddress};
use crate::notify;
use crate::paths::Paths;
use crate::reopen;
use crate::screen;

pub const CAP: usize = 50;
pub const MAX_LENGTH: usize = 4000;
pub const MAX_ATTEMPTS: u32 = 5;
const STRANDED_AFTER: u64 = 90;
const PRUNE_AFTER: u64 = 7 * 24 * 3600;

pub fn add(
    paths: &Paths,
    config: &Config,
    agent: AgentId,
    cwd: &Path,
    text: &str,
    when: PromptWhen,
    at: Option<u64>,
) -> Result<QueuedPrompt> {
    let prompt = sanitize(text)?;
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(Error::Invalid(format!(
            "{} is not an existing absolute directory",
            cwd.display()
        )));
    }
    if !crate::recover::settings_for(config, agent).enabled {
        return Err(Error::Invalid(format!(
            "agent {} is disabled in the configuration",
            agent.name()
        )));
    }
    let entry = QueuedPrompt {
        id: format!("p-{}", &agent::claude::session_uuid()[..8]),
        agent,
        cwd: cwd.to_path_buf(),
        prompt: prompt.clone(),
        when,
        at,
        status: PromptStatus::Pending,
        created_at: now_unix(),
        ..QueuedPrompt::default()
    };
    let stored = ledger::update(paths, |book| {
        let live = book
            .prompts
            .iter()
            .filter(|item| matches!(item.status, PromptStatus::Pending | PromptStatus::Launching))
            .count();
        if live >= CAP {
            return Err(Error::Invalid(format!(
                "the queue already holds {CAP} pending prompts"
            )));
        }
        let duplicate = book.prompts.iter().any(|item| {
            item.status == PromptStatus::Pending
                && item.cwd == entry.cwd
                && item.agent == entry.agent
                && item.prompt == entry.prompt
        });
        if duplicate {
            return Err(Error::Invalid(
                "an identical prompt is already pending for that directory".to_owned(),
            ));
        }
        book.prompts.push(entry.clone());
        Ok(entry.clone())
    })??;
    log::line(
        "",
        format!(
            "queued prompt {} for {} in {} ({:?})",
            stored.id,
            agent.name(),
            cwd.display(),
            when
        ),
    );
    crate::resumer::spawn_if_needed(paths)?;
    Ok(stored)
}

pub fn list(paths: &Paths) -> Result<Vec<QueuedPrompt>> {
    Ok(ledger::read(paths)?.prompts)
}

pub fn remove(paths: &Paths, id: &str) -> Result<bool> {
    ledger::update(paths, |book| {
        let mut found = false;
        for item in &mut book.prompts {
            if item.id == id
                && matches!(item.status, PromptStatus::Pending | PromptStatus::Launching)
            {
                item.status = PromptStatus::Cancelled;
                found = true;
            }
        }
        found
    })
}

pub fn clear(paths: &Paths) -> Result<usize> {
    ledger::update(paths, |book| {
        let mut count = 0;
        for item in &mut book.prompts {
            if matches!(item.status, PromptStatus::Pending | PromptStatus::Launching) {
                item.status = PromptStatus::Cancelled;
                count += 1;
            }
        }
        count
    })
}

pub fn sanitize(text: &str) -> Result<String> {
    let stripped = screen::strip_ansi(text.as_bytes());
    let cleaned: String = stripped
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err(Error::Invalid("the prompt is empty".to_owned()));
    }
    if trimmed.chars().count() > MAX_LENGTH {
        return Err(Error::Invalid(format!(
            "the prompt is longer than {MAX_LENGTH} characters"
        )));
    }
    Ok(trimmed.to_owned())
}

pub fn pass(
    paths: &Paths,
    config: &Config,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<Option<u64>> {
    let now = now_unix();
    let book = ledger::read(paths)?;
    let mut next: Option<u64> = None;
    let consider = |at: u64, next: &mut Option<u64>| {
        *next = Some(next.map_or(at, |current| current.min(at)));
    };
    for item in &book.prompts {
        match item.status {
            PromptStatus::Pending => {
                let due = due_at(&book, item);
                match due {
                    Some(at) if at <= now => launch(paths, config, item, verify_at)?,
                    Some(at) => consider(at, &mut next),
                    None => {}
                }
            }
            PromptStatus::Launching => {
                let verify = verify_at.get(&item.id).copied();
                match (verify, item.launched_at) {
                    (Some(at), _) if at <= now => verify_delivery(paths, config, item, verify_at)?,
                    (Some(at), _) => consider(at, &mut next),
                    (None, Some(started)) if started + STRANDED_AFTER <= now => {
                        reclaim(paths, item, "stranded launch")?
                    }
                    (None, Some(started)) => consider(started + STRANDED_AFTER, &mut next),
                    (None, None) => reclaim(paths, item, "launch without a start time")?,
                }
            }
            _ => {}
        }
    }
    prune(paths, now)?;
    Ok(next)
}

fn due_at(book: &ledger::Ledger, item: &QueuedPrompt) -> Option<u64> {
    let gate = item.not_before.unwrap_or(0);
    let base = match item.when {
        PromptWhen::Now => Some(item.created_at),
        PromptWhen::At => item.at,
        PromptWhen::NextReset => {
            let waiting = book
                .sessions
                .values()
                .filter(|record| {
                    record.agent == item.agent
                        && matches!(record.status, Status::Stopped | Status::Resuming)
                })
                .map(|record| record.reset_at)
                .max();
            Some(waiting.map_or(item.created_at, |reset| reset.max(item.created_at)))
        }
    };
    base.map(|at| at.max(gate))
}

fn launch(
    paths: &Paths,
    config: &Config,
    item: &QueuedPrompt,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<()> {
    let now = now_unix();
    let claimed =
        ledger::update(paths, |book| {
            let Some(current) = book.prompts.iter_mut().find(|candidate| {
                candidate.id == item.id && candidate.status == PromptStatus::Pending
            }) else {
                return false;
            };
            current.status = PromptStatus::Launching;
            current.launched_at = Some(now);
            current.attempts += 1;
            true
        })?;
    if !claimed {
        return Ok(());
    }
    match start_session(paths, config, item) {
        Ok(pane) => {
            ledger::update(paths, |book| {
                if let Some(current) = book
                    .prompts
                    .iter_mut()
                    .find(|candidate| candidate.id == item.id)
                {
                    current.sent_at = Some(now_unix());
                    current.pane = Some(pane.clone());
                    current.mux_session = Some(reopen::RESUME_SESSION.to_owned());
                }
            })?;
            verify_at.insert(
                item.id.clone(),
                now_unix() + u64::from(config.limit.verify_seconds),
            );
            log::line("", format!("prompt {} started in {pane}", item.id));
        }
        Err(error) => {
            fail_attempt(paths, config, item, error.to_string())?;
        }
    }
    Ok(())
}

fn start_session(paths: &Paths, config: &Config, item: &QueuedPrompt) -> Result<PaneAddress> {
    let settings = crate::recover::settings_for(config, item.agent);
    let mut argv = vec![OsString::from(item.agent.name())];
    argv.extend(settings.launch_args.iter().map(OsString::from));
    let target = reopen::choose(config);
    match target {
        reopen::Where::Headless => {
            match item.agent {
                AgentId::Claude => {
                    argv.push(OsString::from("-p"));
                    argv.push(OsString::from(&item.prompt));
                }
            }
            reopen::spawn_detached_headless(paths, &item.id, &item.cwd, argv, &[])
        }
        reopen::Where::Tmux | reopen::Where::Zellij => {
            let pane = reopen::open_in(target, &item.cwd, &argv, &[])?;
            let driver = mux::for_pane(&pane);
            let agent = agent::by_id(item.agent);
            reopen::wait_for_prompt(driver.as_ref(), &pane, agent.prompt_glyph())?;
            let typing = crate::pane_input::Typing {
                mux: driver.as_ref(),
                pane: &pane,
                agent: agent.as_ref(),
            };
            typing.submit(&item.prompt)?;
            Ok(pane)
        }
    }
}

fn verify_delivery(
    paths: &Paths,
    config: &Config,
    item: &QueuedPrompt,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<()> {
    verify_at.remove(&item.id);
    let limited = match &item.pane {
        Some(pane) if !pane.is_headless() => {
            let driver = mux::for_pane(pane);
            match driver.capture(pane, false) {
                Ok(visible) => {
                    let text = screen::strip_ansi(visible.as_bytes());
                    crate::limit_detect::find_limit(&text, item.agent)
                        || crate::limit_detect::rate_limit_menu(&text)
                            != crate::limit_detect::MenuRead::NoMenu
                }
                Err(_) => false,
            }
        }
        _ => false,
    };
    if limited {
        return fail_attempt(
            paths,
            config,
            item,
            "hit a usage limit right after starting".to_owned(),
        );
    }
    ledger::update(paths, |book| {
        if let Some(current) = book
            .prompts
            .iter_mut()
            .find(|candidate| candidate.id == item.id)
        {
            current.status = PromptStatus::Delivered;
            current.delivered_at = Some(now_unix());
        }
    })?;
    let outcome = notify::send(
        &config.notify,
        "claude-nonstop: prompt started",
        &format!(
            "{}: {} session opened in {}",
            item.cwd.display(),
            item.agent.name(),
            item.pane.as_ref().map(|p| p.label()).unwrap_or_default()
        ),
        false,
    );
    log::line("", format!("prompt {} delivered ({outcome})", item.id));
    Ok(())
}

fn fail_attempt(paths: &Paths, config: &Config, item: &QueuedPrompt, reason: String) -> Result<()> {
    let now = now_unix();
    let exhausted = item.attempts >= MAX_ATTEMPTS;
    ledger::update(paths, |book| {
        if let Some(current) = book
            .prompts
            .iter_mut()
            .find(|candidate| candidate.id == item.id)
        {
            current.last_error = Some(reason.clone());
            if exhausted {
                current.status = PromptStatus::Failed;
            } else {
                current.status = PromptStatus::Pending;
                current.not_before = Some(now + crate::resumer::backoff(item.attempts));
                current.pane = None;
            }
        }
    })?;
    if exhausted {
        let outcome = notify::send(
            &config.notify,
            "claude-nonstop: prompt failed",
            &format!("{}: {} attempts", item.cwd.display(), item.attempts),
            true,
        );
        log::line(
            "",
            format!(
                "prompt {} failed after {} attempts: {reason} ({outcome})",
                item.id, item.attempts
            ),
        );
    } else {
        log::line(
            "",
            format!(
                "prompt {} attempt {} failed: {reason}; retry in {}s",
                item.id,
                item.attempts,
                crate::resumer::backoff(item.attempts)
            ),
        );
    }
    Ok(())
}

fn reclaim(paths: &Paths, item: &QueuedPrompt, reason: &str) -> Result<()> {
    ledger::update(paths, |book| {
        if let Some(current) = book.prompts.iter_mut().find(|candidate| {
            candidate.id == item.id && candidate.status == PromptStatus::Launching
        }) {
            current.status = PromptStatus::Pending;
            current.last_error = Some(reason.to_owned());
        }
    })?;
    log::line("", format!("prompt {} back to pending: {reason}", item.id));
    Ok(())
}

fn prune(paths: &Paths, now: u64) -> Result<()> {
    let keep_prompt = |item: &QueuedPrompt| match item.status {
        PromptStatus::Pending | PromptStatus::Launching => true,
        _ => {
            item.delivered_at
                .or(item.launched_at)
                .unwrap_or(item.created_at)
                .saturating_add(PRUNE_AFTER)
                > now
        }
    };
    let keep_session = |record: &ledger::SessionRecord| match record.status {
        Status::Stopped | Status::Resuming => true,
        _ => {
            record
                .last_attempt_at
                .unwrap_or(record.detected_at)
                .saturating_add(PRUNE_AFTER)
                > now
        }
    };
    let book = ledger::read(paths)?;
    if book.prompts.iter().all(keep_prompt) && book.sessions.values().all(keep_session) {
        return Ok(());
    }
    ledger::update(paths, |book| {
        book.prompts.retain(keep_prompt);
        book.sessions.retain(|_, record| keep_session(record));
    })
}

pub fn describe_when(item: &QueuedPrompt) -> String {
    match item.when {
        PromptWhen::Now => "now".to_owned(),
        PromptWhen::NextReset => "next reset".to_owned(),
        PromptWhen::At => item
            .at
            .map(crate::resumer::stamp)
            .unwrap_or_else(|| "?".to_owned()),
    }
}

pub fn cwd_or_current(cwd: Option<PathBuf>) -> Result<PathBuf> {
    match cwd {
        Some(path) => {
            std::fs::canonicalize(&path).map_err(|source| Error::io(&path, "resolve", source))
        }
        None => std::env::current_dir().map_err(|source| Error::io(".", "resolve", source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_writes_only_when_expired_records_exist() {
        let dir = std::env::temp_dir().join(format!("nonstop-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            home: dir.clone(),
            state: dir.clone(),
            config_file: dir.join("config.toml"),
            claude_config: dir.join("claude"),
        };
        let now = PRUNE_AFTER + 100;
        prune(&paths, now).unwrap();
        assert!(!paths.ledger().exists());
        ledger::update(&paths, |book| {
            for (id, status, at) in [
                ("active", PromptStatus::Pending, 0),
                ("launching", PromptStatus::Launching, 0),
                ("recent", PromptStatus::Delivered, now),
                ("expired", PromptStatus::Delivered, 100),
            ] {
                book.prompts.push(QueuedPrompt {
                    id: id.into(),
                    status,
                    created_at: at,
                    ..QueuedPrompt::default()
                });
            }
            for (id, status, at) in [
                ("active", Status::Stopped, 0),
                ("resuming", Status::Resuming, 0),
                ("recent", Status::Resumed, now),
                ("expired", Status::Failed, 100),
            ] {
                book.sessions.insert(
                    id.into(),
                    ledger::SessionRecord {
                        key: id.into(),
                        status,
                        detected_at: at,
                        ..ledger::SessionRecord::default()
                    },
                );
            }
        })
        .unwrap();
        let before = ledger::read(&paths).unwrap().seq;
        prune(&paths, now).unwrap();
        let book = ledger::read(&paths).unwrap();
        assert_eq!(book.seq, before + 1);
        assert_eq!(book.prompts.len(), 3);
        assert!(!book.prompts.iter().any(|item| item.id == "expired"));
        assert_eq!(book.sessions.len(), 3);
        assert!(!book.sessions.contains_key("expired"));
        let bytes = std::fs::read(paths.ledger()).unwrap();
        let modified = std::fs::metadata(paths.ledger())
            .unwrap()
            .modified()
            .unwrap();
        for _ in 0..10 {
            prune(&paths, now).unwrap();
        }
        assert_eq!(std::fs::read(paths.ledger()).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(paths.ledger())
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prompts_are_sanitized_and_bounded() {
        assert_eq!(
            sanitize("  run \x1b[31mtests\x1b[0m\n\tnow \x07 ").unwrap(),
            "run tests\n\tnow"
        );
        assert!(sanitize("\x1b[0m   ").is_err());
        assert!(sanitize(&"x".repeat(MAX_LENGTH + 1)).is_err());
    }

    #[test]
    fn next_reset_waits_for_the_latest_open_limit() {
        let mut book = ledger::Ledger::default();
        let item = QueuedPrompt {
            id: "p-1".into(),
            agent: AgentId::Claude,
            when: PromptWhen::NextReset,
            created_at: 100,
            ..QueuedPrompt::default()
        };
        assert_eq!(due_at(&book, &item), Some(100));
        book.sessions.insert(
            "a".into(),
            ledger::SessionRecord {
                key: "a".into(),
                agent: AgentId::Claude,
                status: Status::Stopped,
                reset_at: 500,
                ..ledger::SessionRecord::default()
            },
        );
        book.sessions.insert(
            "b".into(),
            ledger::SessionRecord {
                key: "b".into(),
                agent: AgentId::Claude,
                status: Status::Resumed,
                reset_at: 900,
                ..ledger::SessionRecord::default()
            },
        );
        assert_eq!(due_at(&book, &item), Some(500));
        let gated = QueuedPrompt {
            not_before: Some(700),
            ..item.clone()
        };
        assert_eq!(due_at(&book, &gated), Some(700));
        let at = QueuedPrompt {
            when: PromptWhen::At,
            at: Some(300),
            ..item.clone()
        };
        assert_eq!(due_at(&book, &at), Some(300));
    }
}
