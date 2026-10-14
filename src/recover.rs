use std::path::Path;
use std::time::Duration;

use crate::config::{AgentConfig, Config};
use crate::continue_counter::{self, Decision};
use crate::error::Result;
use crate::ledger::{
    self, AgentId, DetectedVia, LimitType, ResetSource, SessionRecord, Status, StopKind, now_unix,
};
use crate::limit_detect::{self, MenuRead};
use crate::log;
use crate::mux::{Multiplexer, PaneAddress};
use crate::notify;
use crate::overload_ladder::{self, Episode, Step};
use crate::pane_input::Typing;
use crate::paths::Paths;
use crate::reset_time;
use crate::transcript_claude::{self, LimitRecord};

const EPISODE_WINDOW: u64 = 120;
const MENU_STEP_GAP: Duration = Duration::from_millis(100);

pub struct Context<'a> {
    pub paths: &'a Paths,
    pub config: &'a Config,
    pub agent: AgentId,
    pub session_id: &'a str,
    pub transcript: &'a Path,
    pub pane: &'a PaneAddress,
    pub mux: &'a dyn Multiplexer,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Typed { count: u32 },
    Exhausted { count: u32 },
    Skipped(String),
}

impl Context<'_> {
    fn settings(&self) -> &AgentConfig {
        match self.agent {
            AgentId::Claude => &self.config.agents.claude,
        }
    }

    fn prompts(&self) -> Vec<&str> {
        let settings = self.settings();
        vec![
            settings.cutoff_message.as_str(),
            settings.overload_message.as_str(),
            settings.resume_message.as_str(),
        ]
    }

    fn display_name(&self) -> String {
        crate::claude_sessions::display_name(self.paths, self.session_id, &self.pane.label())
    }

    pub fn continue_after(&self, reason: &str, message: &str) -> Result<Outcome> {
        if self.pane.is_headless() {
            return Ok(Outcome::Skipped(format!("{reason}: no pane to type into")));
        }
        let max = self.config.cutoff.max_in_a_row;
        let decision = continue_counter::evaluate(
            self.paths,
            self.session_id,
            self.transcript,
            &self.prompts(),
            max,
        )?;
        let next = match decision {
            Decision::Allowed { next } => next,
            Decision::Exhausted { count } => {
                let name = self.display_name();
                let outcome = notify::send(
                    &self.config.notify,
                    "claude-nonstop: gave up",
                    &format!("{name}: {count} resumes without progress"),
                    true,
                );
                log::line(
                    self.session_id,
                    format!(
                        "{reason}: {count} resumes in a row without progress, giving up ({outcome})"
                    ),
                );
                return Ok(Outcome::Exhausted { count });
            }
        };
        let agent = crate::agent::by_id(self.agent);
        let typing = Typing {
            mux: self.mux,
            pane: self.pane,
            agent: agent.as_ref(),
        };
        if let Err(error) = typing.submit(message) {
            log::line(
                self.session_id,
                format!("{reason}: resume to {} failed: {error}", self.pane),
            );
            return Ok(Outcome::Skipped(error.to_string()));
        }
        continue_counter::record(self.paths, self.session_id, self.transcript, next)?;
        let name = self.display_name();
        let outcome = notify::send(
            &self.config.notify,
            "claude-nonstop: resumed",
            &format!("{name}: {reason}, resume {next}/{max}"),
            false,
        );
        log::line(
            self.session_id,
            format!(
                "{reason}: resume {next}/{max} written to {} ({name}), {outcome}",
                self.pane
            ),
        );
        Ok(Outcome::Typed { count: next })
    }

    pub fn stream_cut(&self, error: &str) -> Result<Outcome> {
        let message = self.settings().cutoff_message.clone();
        self.continue_after(&format!("turn cut off ({error})"), &message)
    }
}

pub fn settings_for(config: &Config, agent: AgentId) -> &AgentConfig {
    match agent {
        AgentId::Claude => &config.agents.claude,
    }
}

pub fn resolve_limit(config: &Config, latest: &LimitRecord) -> (u64, ResetSource) {
    let spec = reset_time::parse_reset(&latest.text);
    let now = jiff::Timestamp::now();
    let banner = jiff::Timestamp::from_second(latest.at as i64).ok();
    let margin = Duration::from_secs(u64::from(config.limit.margin_seconds));
    let probe = Duration::from_secs(u64::from(config.limit.probe_minutes[0]) * 60);
    let resolved = reset_time::resolve(
        spec.as_ref(),
        now,
        banner,
        &jiff::tz::TimeZone::system(),
        margin,
        probe,
    );
    let mut at = resolved.at;
    if latest.kind == LimitType::FiveHour && resolved.source == ResetSource::Absolute {
        at = reset_time::stale_five_hour_clamp(at, banner.unwrap_or(now), margin);
    }
    (u64::try_from(at.as_second()).unwrap_or(0), resolved.source)
}

pub fn record_limit(
    paths: &Paths,
    config: &Config,
    session_id: &str,
    cwd: &Path,
    pane: Option<&PaneAddress>,
    via: DetectedVia,
    transcript: Option<&Path>,
) -> Result<()> {
    let agent = AgentId::Claude;
    let now = now_unix();
    let latest = match transcript.filter(|path| path.is_file()) {
        Some(path) => transcript_claude::latest_limit_record(path, &config.overload)
            .map_err(|source| crate::error::Error::io(path, "read", source))?,
        None => None,
    };
    let (reset_at, reset_source, kind, banner_at) = match &latest {
        Some(record) => {
            let (at, source) = resolve_limit(config, record);
            (at, source, record.kind, Some(record.at))
        }
        None => (
            now + u64::from(config.limit.probe_minutes[0]) * 60,
            ResetSource::Fallback,
            LimitType::Unknown,
            None,
        ),
    };
    ledger::update(paths, |ledger| {
        let record = ledger
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| SessionRecord {
                key: session_id.to_owned(),
                ..SessionRecord::default()
            });
        let same_episode = record.is_open()
            && record.stop == StopKind::UsageLimit
            && banner_at.unwrap_or(now).abs_diff(record.episode_at()) <= EPISODE_WINDOW;
        if same_episode {
            if pane.is_some() {
                record.pane = pane.cloned();
            }
            let better = reset_source > record.reset_source
                || (reset_source == record.reset_source && reset_at < record.reset_at);
            if better {
                record.reset_at = reset_at;
                record.reset_source = reset_source;
            }
            if record.limit == LimitType::Unknown {
                record.limit = kind;
            }
            return;
        }
        *record = SessionRecord {
            key: session_id.to_owned(),
            agent,
            session_id: Some(session_id.to_owned()),
            cwd: cwd.to_path_buf(),
            pane: pane.cloned(),
            status: Status::Stopped,
            stop: StopKind::UsageLimit,
            limit: kind,
            detected_via: via,
            detected_at: now,
            banner_at,
            reset_at,
            reset_source,
            ..SessionRecord::default()
        };
    })?;
    if let Some(pane) = pane {
        if config.general.menu_auto_answer && !pane.is_headless() {
            let driver = crate::mux::for_pane(pane);
            let driver_agent = crate::agent::by_id(agent);
            let typing = Typing {
                mux: driver.as_ref(),
                pane,
                agent: driver_agent.as_ref(),
            };
            if let Err(error) = answer_menu(&typing, session_id) {
                log::line(session_id, format!("rate limit menu not answered: {error}"));
            }
        }
    }
    let name = crate::claude_sessions::display_name(
        paths,
        session_id,
        &pane
            .map(|p| p.label())
            .unwrap_or_else(|| cwd.display().to_string()),
    );
    let body = if reset_source == ResetSource::Fallback {
        format!(
            "{name}: no reset time, probing every {} min",
            config.limit.probe_minutes[0]
        )
    } else {
        format!(
            "{name}: resets {} (in {})",
            crate::resumer::stamp(reset_at),
            human_wait(reset_at.saturating_sub(now))
        )
    };
    let outcome = notify::send(&config.notify, "claude-nonstop: limit", &body, false);
    log::line(
        session_id,
        format!(
            "usage limit ({:?}) recorded, wake at {} ({:?}), {outcome}",
            kind,
            crate::resumer::stamp(reset_at),
            reset_source
        ),
    );
    crate::resumer::spawn_if_needed(paths)
}

pub struct OverloadReport<'a> {
    pub session_id: &'a str,
    pub cwd: &'a Path,
    pub pane: &'a PaneAddress,
    pub transcript: &'a Path,
    pub error: &'a str,
}

pub fn schedule_overload(paths: &Paths, config: &Config, report: &OverloadReport) -> Result<()> {
    let session_id = report.session_id;
    if session_id.is_empty() {
        log::line(
            "",
            format!(
                "overloaded ({}) without a session id, not resumed",
                report.error
            ),
        );
        return Ok(());
    }

    let current = ledger::read(paths)?.sessions.get(session_id).cloned();
    if let Some(record) = current
        .as_ref()
        .filter(|record| record.is_open() && record.stop == StopKind::UsageLimit)
    {
        log::line(
            session_id,
            format!(
                "overloaded ({}) while a usage limit waits until {}, left to the limit",
                report.error,
                crate::resumer::stamp(record.reset_at)
            ),
        );
        return Ok(());
    }

    let previous = current
        .filter(|record| record.stop == StopKind::Overload)
        .map(|record| Episode {
            started_at: record.detected_at,
            sent: record.attempts,
            last_sent_at: record.last_attempt_at,
        });
    let progressed = match &previous {
        Some(episode) if report.transcript.is_file() => transcript_claude::progressed_after(
            report.transcript,
            episode.last_sent_at.unwrap_or(episode.started_at),
        )
        .map_err(|source| crate::error::Error::io(report.transcript, "read", source))?,
        _ => false,
    };
    let now = now_unix();
    let step = overload_ladder::next_step(
        &config.overload,
        previous,
        progressed,
        now,
        overload_ladder::jitter_unit(),
    );

    match step {
        Step::GiveUp { episode } => {
            write_overload(paths, report, &episode, Status::Failed, now)?;
            let name =
                crate::claude_sessions::display_name(paths, session_id, &report.pane.label());
            let outcome = notify::send(
                &config.notify,
                "claude-nonstop: gave up",
                &format!(
                    "{name}: overloaded for {} min",
                    config.overload.max_total_minutes
                ),
                true,
            );
            log::line(
                session_id,
                format!(
                    "overloaded ({}): waited the full {} min, giving up ({outcome})",
                    report.error, config.overload.max_total_minutes
                ),
            );
            Ok(())
        }
        Step::Wait { episode, wait } => {
            let reset_at = now + wait.as_secs().max(1);
            write_overload(paths, report, &episode, Status::Stopped, reset_at)?;
            log::line(
                session_id,
                format!(
                    "overloaded ({}): resume in {}s, level {}, {:.0} min spent waiting so far",
                    report.error,
                    reset_at - now,
                    episode.sent + 1,
                    now.saturating_sub(episode.started_at) as f64 / 60.0
                ),
            );
            crate::resumer::spawn_if_needed(paths)
        }
    }
}

fn write_overload(
    paths: &Paths,
    report: &OverloadReport,
    episode: &Episode,
    status: Status,
    reset_at: u64,
) -> Result<()> {
    let record = SessionRecord {
        key: report.session_id.to_owned(),
        agent: AgentId::Claude,
        session_id: Some(report.session_id.to_owned()),
        cwd: report.cwd.to_path_buf(),
        pane: Some(report.pane.clone()),
        status,
        stop: StopKind::Overload,
        detected_via: DetectedVia::Hook,
        detected_at: episode.started_at,
        reset_at,
        attempts: episode.sent,
        last_attempt_at: episode.last_sent_at,
        last_error: Some(report.error.to_owned()),
        ..SessionRecord::default()
    };

    ledger::update(paths, |ledger| {
        ledger.sessions.insert(record.key.clone(), record);
    })
}

pub fn answer_menu(typing: &Typing, session_id: &str) -> Result<()> {
    let visible = typing.mux.capture(typing.pane, false)?;
    let text = crate::screen::strip_ansi(visible.as_bytes());
    match limit_detect::rate_limit_menu(&text) {
        MenuRead::NoMenu => Ok(()),
        MenuRead::Unreadable => {
            log::line(
                session_id,
                "rate limit menu is on screen but its cursor cannot be read, not pressing enter",
            );
            Ok(())
        }
        MenuRead::Steps(steps) => {
            let key = if steps < 0 {
                crate::mux::Key::Up
            } else {
                crate::mux::Key::Down
            };
            for _ in 0..steps.unsigned_abs() {
                typing.press(key)?;
                std::thread::sleep(MENU_STEP_GAP);
            }
            typing.press(crate::mux::Key::Enter)?;
            log::line(
                session_id,
                format!("rate limit menu answered with stop and wait ({steps} steps)"),
            );
            Ok(())
        }
    }
}

pub fn human_wait(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}
