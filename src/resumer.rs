use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};

use crate::agent;
use crate::claude_sessions;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::ledger::{
    self, DetectedVia, ResetSource, ResumedBy, SessionRecord, Status, StopKind, now_unix,
};
use crate::log;
use crate::mux;
use crate::notify;
use crate::overload_ladder;
use crate::pane_input::Typing;
use crate::paths::Paths;
use crate::platform::Reactor;
use crate::platform::process::spawn_detached;
use crate::recover;
use crate::transcript_claude;
use crate::transcript_watch::TranscriptWatch;

const MAX_BACKOFF: u64 = 30 * 60;

pub fn spawn_if_needed(paths: &Paths) -> Result<()> {
    let lock_path = paths.resumer_lock();
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::io(&lock_path, "open", source))?;
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(()),
        Err(source) => return Err(Error::io(&lock_path, "lock", source.into())),
    }
    drop(lock);
    let exe =
        std::env::current_exe().map_err(|source| Error::io("claude-nonstop", "resolve", source))?;
    let stderr_path = paths.log_dir().join("resumer-stderr.log");
    let stderr = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&stderr_path)
        .map_err(|source| Error::io(&stderr_path, "open", source))?;
    let mut command = Command::new(exe);
    command.arg("resumer").stdout(Stdio::null()).stderr(stderr);
    spawn_detached(&mut command)
        .map_err(|source| Error::io("claude-nonstop resumer", "spawn", source))?;
    log::line("", "resumer spawned");
    Ok(())
}

pub fn run(paths: &Paths, config: &Config, persistent: bool) -> Result<i32> {
    let lock_path = paths.resumer_lock();
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::io(&lock_path, "open", source))?;
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => {
            log::line("", "another resumer holds the lock, exiting");
            return Ok(0);
        }
        Err(source) => return Err(Error::io(&lock_path, "lock", source.into())),
    }
    ledger::update(paths, |book| book.resumer_pid = Some(std::process::id()))?;
    let mut reactor = Reactor::new().map_err(|source| Error::io("reactor", "create", source))?;
    reactor
        .watch_dir(&paths.state)
        .map_err(|source| Error::io(&paths.state, "watch", source))?;
    let mut verify_at: BTreeMap<String, u64> = BTreeMap::new();
    let mut transcripts = if persistent {
        Some(TranscriptWatch::start(
            &mut reactor,
            paths,
            &config.overload,
        )?)
    } else {
        None
    };
    log::line(
        "",
        format!(
            "resumer running{}",
            if persistent { " (persistent)" } else { "" }
        ),
    );
    loop {
        let next = pass(paths, config, &mut verify_at)?;
        match next {
            Some(at) => reactor
                .set_deadline(Some(UNIX_EPOCH + Duration::from_secs(at)))
                .map_err(|source| Error::io("reactor", "arm", source))?,
            None if persistent => reactor
                .set_deadline(None)
                .map_err(|source| Error::io("reactor", "arm", source))?,
            None => {
                ledger::update(paths, |book| book.resumer_pid = None)?;
                log::line("", "nothing left to wait for, resumer exiting");
                return Ok(0);
            }
        }
        let wakes = reactor
            .wait()
            .map_err(|source| Error::io("reactor", "wait", source))?;
        for wake in &wakes {
            if let Some(watch) = transcripts.as_mut() {
                for seen in watch.on_wake(wake, &mut reactor)? {
                    recover::record_limit(
                        paths,
                        config,
                        &seen.session_id,
                        &seen.cwd,
                        None,
                        DetectedVia::Transcript,
                        Some(&seen.transcript),
                    )?;
                }
            }
        }
    }
}

fn pass(
    paths: &Paths,
    config: &Config,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<Option<u64>> {
    let now = now_unix();
    let book = ledger::read(paths)?;
    let mut next: Option<u64> = None;
    for record in book.sessions.values() {
        let due = match record.status {
            Status::Stopped => record.reset_at,
            Status::Resuming => *verify_at.entry(record.key.clone()).or_insert(now),
            _ => continue,
        };
        if due <= now {
            let handled = match record.status {
                Status::Stopped => dispatch(paths, config, record, verify_at),
                Status::Resuming => verify(paths, config, record, verify_at),
                _ => Ok(()),
            };
            if let Err(error) = handled {
                postpone(paths, record, &error, verify_at)?;
            }
        } else {
            next = Some(next.map_or(due, |current| current.min(due)));
        }
    }
    if let Some(queue_next) = crate::prompt_queue::pass(paths, config, verify_at)? {
        next = Some(next.map_or(queue_next, |current| current.min(queue_next)));
    }
    let after = ledger::read(paths)?;
    for record in after.sessions.values() {
        let due = match record.status {
            Status::Stopped => record.reset_at,
            Status::Resuming => verify_at.get(&record.key).copied().unwrap_or(now),
            _ => continue,
        };
        if due > now {
            next = Some(next.map_or(due, |current| current.min(due)));
        } else if record.status == Status::Stopped {
            next = Some(now + 1);
        }
    }
    if after.prompts.iter().any(|item| {
        matches!(
            item.status,
            ledger::PromptStatus::Pending | ledger::PromptStatus::Launching
        )
    }) && next.is_none()
    {
        next = Some(now + 60);
    }
    Ok(next)
}

fn dispatch(
    paths: &Paths,
    config: &Config,
    record: &SessionRecord,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<()> {
    if record.stop == StopKind::Overload {
        return dispatch_overload(paths, config, record);
    }

    let key = record.key.clone();
    let session = record.session_id.clone().unwrap_or_default();
    let transcript = claude_sessions::transcript_path(paths, &session);
    if let Some(path) = &transcript {
        if transcript_claude::progressed_after(path, record.episode_at())
            .map_err(|source| Error::io(path, "read", source))?
        {
            finish(paths, &key, ResumedBy::Native)?;
            log::line(&session, "the session went on by itself, nothing to resume");
            return Ok(());
        }
    }
    if record.reset_source == ResetSource::Fallback
        && record.probe_count < probe_ceiling(config, record)
    {
        return probe(paths, config, record, transcript.as_deref());
    }
    if record.attempts >= config.limit.max_attempts {
        let name = display_name(paths, &session, record);
        let outcome = notify::send(
            &config.notify,
            "claude-nonstop: gave up",
            &format!("{name}: {} attempts, still limited", record.attempts),
            true,
        );
        ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
            current.status = Status::Failed;
            current.last_error = Some(format!("{} attempts without progress", current.attempts));
        })?;
        log::line(
            &session,
            format!("giving up after {} attempts ({outcome})", record.attempts),
        );
        return Ok(());
    }
    let live_pane = match record.pane.clone() {
        Some(pane) if !pane.is_headless() && mux::for_pane(&pane).pane_alive(&pane)? => Some(pane),
        _ => None,
    };
    let Some(pane) = live_pane else {
        return reopen_session(paths, config, record, verify_at);
    };
    let driver = mux::for_pane(&pane);
    let agent = agent::by_id(record.agent);
    let typing = Typing {
        mux: driver.as_ref(),
        pane: &pane,
        agent: agent.as_ref(),
    };
    if config.general.menu_auto_answer {
        recover::answer_menu(&typing, &session)?;
    }
    let message = record.resume_message.clone().unwrap_or_else(|| {
        recover::settings_for(config, record.agent)
            .resume_message
            .clone()
    });
    let now = now_unix();
    match typing.submit(&message) {
        Ok(()) => {
            let attempts = record.attempts + 1;
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.status = Status::Resuming;
                current.attempts = attempts;
                current.last_attempt_at = Some(now);
                current.resume_episode_at = Some(now);
            })?;
            verify_at.insert(key, now + u64::from(config.limit.verify_seconds));
            log::line(
                &session,
                format!(
                    "resume {attempts}/{} written to {pane}",
                    config.limit.max_attempts
                ),
            );
        }
        Err(error) => {
            let retry_at = now + backoff(record.attempts + 1);
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.attempts += 1;
                current.last_attempt_at = Some(now);
                current.last_error = Some(error.to_string());
                current.reset_at = retry_at;
            })?;
            log::line(
                &session,
                format!(
                    "resume to {pane} refused: {error}; next try in {}s",
                    retry_at - now
                ),
            );
        }
    }
    Ok(())
}

fn postpone(
    paths: &Paths,
    record: &SessionRecord,
    error: &Error,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<()> {
    let retry_at = now_unix() + backoff(record.attempts + 1);
    ledger::cas(
        paths,
        &record.key,
        &[Status::Stopped, Status::Resuming],
        None,
        |current| {
            current.attempts += 1;
            current.reset_at = retry_at;
            current.last_error = Some(error.to_string());
        },
    )?;
    if record.status == Status::Resuming {
        verify_at.insert(record.key.clone(), retry_at);
    }

    log::line(
        record.session_id.as_deref().unwrap_or(&record.key),
        format!("{error}; next try at {}", stamp(retry_at)),
    );
    Ok(())
}

fn dispatch_overload(paths: &Paths, config: &Config, record: &SessionRecord) -> Result<()> {
    let key = record.key.clone();
    let session = record.session_id.clone().unwrap_or_default();
    let last_sent = record.last_attempt_at.unwrap_or(record.detected_at);
    if let Some(path) = claude_sessions::transcript_path(paths, &session) {
        if transcript_claude::progressed_after(&path, last_sent)
            .map_err(|source| Error::io(&path, "read", source))?
        {
            finish(paths, &key, ResumedBy::Native)?;
            log::line(&session, "overloaded: the session went on by itself");
            return Ok(());
        }
    }

    let now = now_unix();
    let cap = u64::from(config.overload.max_total_minutes) * 60;
    if now.saturating_sub(record.detected_at) >= cap {
        let name = display_name(paths, &session, record);
        let outcome = notify::send(
            &config.notify,
            "claude-nonstop: gave up",
            &format!(
                "{name}: overloaded for {} min",
                config.overload.max_total_minutes
            ),
            true,
        );
        ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
            current.status = Status::Failed;
        })?;
        log::line(
            &session,
            format!(
                "overloaded: waited the full {} min, giving up ({outcome})",
                config.overload.max_total_minutes
            ),
        );
        return Ok(());
    }

    let live_pane = match record.pane.clone() {
        Some(pane) if !pane.is_headless() && mux::for_pane(&pane).pane_alive(&pane)? => Some(pane),
        _ => None,
    };
    let Some(pane) = live_pane else {
        ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
            current.status = Status::Cancelled;
            current.last_error = Some("pane is gone".to_owned());
        })?;
        log::line(&session, "overloaded: the pane is gone, nothing to resume");
        return Ok(());
    };

    let driver = mux::for_pane(&pane);
    let agent = agent::by_id(record.agent);
    let typing = Typing {
        mux: driver.as_ref(),
        pane: &pane,
        agent: agent.as_ref(),
    };
    let message = recover::settings_for(config, record.agent)
        .overload_message
        .clone();
    match typing.submit(&message) {
        Ok(()) => {
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.status = Status::Resumed;
                current.resumed_by = Some(ResumedBy::Nonstop);
                current.attempts += 1;
                current.last_attempt_at = Some(now);
            })?;
            let name = display_name(paths, &session, record);
            let outcome = notify::send(
                &config.notify,
                "claude-nonstop: resumed",
                &format!("{name}: overloaded, try {}", record.attempts + 1),
                false,
            );
            log::line(
                &session,
                format!(
                    "overloaded: try {} written to {pane} ({name}), {outcome}",
                    record.attempts + 1
                ),
            );
        }
        Err(error) => {
            let wait = overload_ladder::wait(
                &config.overload,
                record.attempts,
                overload_ladder::jitter_unit(),
            );
            let retry_at = now + wait.as_secs().max(1);
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.reset_at = retry_at;
                current.last_error = Some(error.to_string());
            })?;
            log::line(
                &session,
                format!(
                    "overloaded: resume to {pane} refused: {error}; next try in {}s",
                    retry_at - now
                ),
            );
        }
    }
    Ok(())
}

fn reopen_session(
    paths: &Paths,
    config: &Config,
    record: &SessionRecord,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<()> {
    let key = record.key.clone();
    let session = record.session_id.clone().unwrap_or_default();
    if let Some(pane) = &record.pane {
        if pane.is_headless() && mux::for_pane(pane).pane_alive(pane)? {
            let name = display_name(paths, &session, record);
            let outcome = notify::send(
                &config.notify,
                "claude-nonstop: needs you",
                &format!("{name}: limit reset, but the session runs outside a terminal pane"),
                true,
            );
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.status = Status::Failed;
                current.last_error = Some("running outside a pane, cannot type".to_owned());
            })?;
            log::line(
                &session,
                format!("limit reset but {pane} is a live agent without a pane ({outcome})"),
            );
            return Ok(());
        }
    }
    if let Some(entry) = claude_sessions::by_session_id(paths, &session)? {
        if crate::platform::process::alive(entry.pid) {
            let name = display_name(paths, &session, record);
            let outcome = notify::send(
                &config.notify,
                "claude-nonstop: needs you",
                &format!("{name}: limit reset, but its pane is unknown to claude-nonstop"),
                true,
            );
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.status = Status::Failed;
                current.last_error = Some(format!(
                    "session still runs as pid {} in an unknown pane",
                    entry.pid
                ));
            })?;
            log::line(
                &session,
                format!(
                    "limit reset but the session still runs as pid {} in a pane claude-nonstop cannot reach ({outcome})",
                    entry.pid
                ),
            );
            return Ok(());
        }
    }
    let prompt = crate::reopen::prompt_for(config, record);
    let now = now_unix();
    match crate::reopen::reopen(paths, config, record, &prompt) {
        Ok(pane) => {
            let attempts = record.attempts + 1;
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.status = Status::Resuming;
                current.pane = Some(pane.clone());
                current.attempts = attempts;
                current.last_attempt_at = Some(now);
                current.resume_episode_at = Some(now);
            })?;
            verify_at.insert(key, now + u64::from(config.limit.verify_seconds));
            let name = display_name(paths, &session, record);
            let outcome = notify::send(
                &config.notify,
                "claude-nonstop: reopened",
                &format!("{name}: continued in {pane}"),
                false,
            );
            log::line(
                &session,
                format!(
                    "reopened in {pane}, resume {attempts}/{} ({outcome})",
                    config.limit.max_attempts
                ),
            );
        }
        Err(error) => {
            let retry_at = now + backoff(record.attempts + 1);
            ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                current.attempts += 1;
                current.last_attempt_at = Some(now);
                current.last_error = Some(error.to_string());
                current.reset_at = retry_at;
            })?;
            log::line(
                &session,
                format!("reopen failed: {error}; next try in {}s", retry_at - now),
            );
        }
    }
    Ok(())
}

fn verify(
    paths: &Paths,
    config: &Config,
    record: &SessionRecord,
    verify_at: &mut BTreeMap<String, u64>,
) -> Result<()> {
    let key = record.key.clone();
    let session = record.session_id.clone().unwrap_or_default();
    let episode = record.resume_episode_at.unwrap_or(record.episode_at());
    verify_at.remove(&key);
    if let Some(path) = claude_sessions::transcript_path(paths, &session) {
        if transcript_claude::progressed_after(&path, episode)
            .map_err(|source| Error::io(&path, "read", source))?
        {
            finish(paths, &key, ResumedBy::Nonstop)?;
            let name = display_name(paths, &session, record);
            let outcome = notify::send(
                &config.notify,
                "claude-nonstop: resumed",
                &format!("{name}: limit reset, continuing"),
                false,
            );
            log::line(&session, format!("resumed after the limit ({outcome})"));
            return Ok(());
        }
        if let Some(latest) = transcript_claude::latest_limit_record(&path, &config.overload)
            .map_err(|source| Error::io(&path, "read", source))?
        {
            if latest.at > episode {
                let retry_at = recover::resolve_limit(config, &latest)
                    .0
                    .max(now_unix() + backoff(record.attempts));
                ledger::cas(paths, &key, &[Status::Resuming], None, |current| {
                    current.status = Status::Stopped;
                    current.reset_at = retry_at;
                    current.banner_at = Some(latest.at);
                    current.limit = latest.kind;
                })?;
                log::line(
                    &session,
                    format!(
                        "still limited after the resume, next try at {}",
                        stamp(retry_at)
                    ),
                );
                return Ok(());
            }
        }
    }
    finish(paths, &key, ResumedBy::Nonstop)?;
    log::line(&session, "resume delivered, no new limit seen");
    Ok(())
}

fn probe(
    paths: &Paths,
    config: &Config,
    record: &SessionRecord,
    transcript: Option<&std::path::Path>,
) -> Result<()> {
    let key = record.key.clone();
    let session = record.session_id.clone().unwrap_or_default();
    if let Some(path) = transcript {
        if let Some(latest) = transcript_claude::latest_limit_record(path, &config.overload)
            .map_err(|source| Error::io(path, "read", source))?
        {
            let (at, source) = recover::resolve_limit(config, &latest);
            if source != ResetSource::Fallback {
                ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
                    current.reset_at = at;
                    current.reset_source = source;
                    current.banner_at = Some(latest.at);
                    current.limit = latest.kind;
                })?;
                log::line(
                    &session,
                    format!("reset time learned from the transcript: {}", stamp(at)),
                );
                return Ok(());
            }
        }
    }
    let step = config.limit.probe_minutes
        [(record.probe_count as usize).min(config.limit.probe_minutes.len() - 1)];
    let next = now_unix() + u64::from(step) * 60;
    ledger::cas(paths, &key, &[Status::Stopped], None, |current| {
        current.probe_count += 1;
        current.reset_at = next;
    })?;
    log::line(
        &session,
        format!("no reset time known, probing again in {step} min"),
    );
    Ok(())
}

fn probe_ceiling(config: &Config, record: &SessionRecord) -> u32 {
    let deadline = record.detected_at + u64::from(config.limit.fallback_hours) * 3600;
    if now_unix() >= deadline { 0 } else { u32::MAX }
}

fn finish(paths: &Paths, key: &str, by: ResumedBy) -> Result<()> {
    ledger::cas(
        paths,
        key,
        &[Status::Stopped, Status::Resuming],
        None,
        |current| {
            current.status = Status::Resumed;
            current.resumed_by = Some(by);
        },
    )?;
    Ok(())
}

pub fn backoff(attempts: u32) -> u64 {
    let exponent = attempts.saturating_sub(1).min(20);
    (60u64 << exponent).min(MAX_BACKOFF)
}

fn display_name(paths: &Paths, session: &str, record: &SessionRecord) -> String {
    let fallback = record
        .pane
        .as_ref()
        .map(|pane| pane.label())
        .unwrap_or_else(|| record.cwd.display().to_string());
    if session.is_empty() {
        fallback
    } else {
        claude_sessions::display_name(paths, session, &fallback)
    }
}

pub fn stamp(at: u64) -> String {
    let instant = SystemTime::UNIX_EPOCH + Duration::from_secs(at);
    match jiff::Timestamp::try_from(instant) {
        Ok(timestamp) => timestamp
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M %d.%m")
            .to_string(),
        Err(_) => at.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_thirty() {
        assert_eq!(backoff(0), 60);
        assert_eq!(backoff(1), 60);
        assert_eq!(backoff(2), 120);
        assert_eq!(backoff(5), 960);
        assert_eq!(backoff(6), 1800);
        assert_eq!(backoff(40), 1800);
    }
}
