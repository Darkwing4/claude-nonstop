use serde::Serialize;

use crate::error::{Error, Result};
use crate::ledger::{self, PromptStatus, SessionRecord, Status, now_unix};
use crate::paths::Paths;
use crate::prompt_queue;
use crate::recover::human_wait;
use crate::resumer::stamp;

#[derive(Serialize)]
struct StatusReport<'a> {
    version: u32,
    resumer_pid: Option<u32>,
    resumer_alive: bool,
    sessions: Vec<&'a SessionRecord>,
    prompts: Vec<&'a ledger::QueuedPrompt>,
}

pub fn status(paths: &Paths, json: bool) -> Result<String> {
    let book = ledger::read(paths)?;
    let resumer_alive = book
        .resumer_pid
        .is_some_and(crate::platform::process::alive);
    if json {
        let report = StatusReport {
            version: 1,
            resumer_pid: book.resumer_pid,
            resumer_alive,
            sessions: book.sessions.values().collect(),
            prompts: book.prompts.iter().collect(),
        };
        return serde_json::to_string_pretty(&report).map_err(|source| Error::Json {
            path: "status".into(),
            source,
        });
    }
    let now = now_unix();
    let mut out = String::new();
    let open = book
        .sessions
        .values()
        .filter(|record| record.is_open())
        .count();
    out.push_str(&format!(
        "claude-nonstop: {open} waiting session(s), {} tracked",
        book.sessions.len()
    ));
    match (book.resumer_pid, resumer_alive) {
        (Some(pid), true) => out.push_str(&format!("  (resumer pid {pid})\n")),
        (Some(pid), false) => out.push_str(&format!("  (resumer pid {pid} is gone)\n")),
        (None, _) => out.push('\n'),
    }
    let mut records: Vec<&SessionRecord> = book.sessions.values().collect();
    records.sort_by_key(|record| (!record.is_open(), record.reset_at));
    for record in records {
        let name = record
            .name
            .clone()
            .or_else(|| {
                record
                    .session_id
                    .as_ref()
                    .map(|id| id.chars().take(8).collect())
            })
            .unwrap_or_else(|| record.key.clone());
        out.push_str(&format!(
            "\n  [{:<9}] {name}  {} {}  {}\n",
            status_label(record.status),
            record.agent.name(),
            limit_label(record),
            record.cwd.display()
        ));
        let mut details = Vec::new();
        if let Some(pane) = &record.pane {
            details.push(pane.label());
        }
        details.push(format!("via {:?}", record.detected_via).to_lowercase());
        if record.is_open() {
            let wait = if record.reset_at > now {
                format!("in {}", human_wait(record.reset_at - now))
            } else {
                "due now".to_owned()
            };
            details.push(
                format!(
                    "wake {} ({wait}, {:?})",
                    stamp(record.reset_at),
                    record.reset_source
                )
                .to_lowercase(),
            );
        }
        details.push(format!("attempts {}", record.attempts));
        if let Some(by) = record.resumed_by {
            details.push(format!("resumed by {:?}", by).to_lowercase());
        }
        if let Some(error) = &record.last_error {
            details.push(format!("last error: {error}"));
        }
        if let Some(message) = &record.resume_message {
            details.push(format!("message: {message:?}"));
        }
        out.push_str(&format!("             {}\n", details.join(" · ")));
    }
    let pending = book
        .prompts
        .iter()
        .filter(|item| matches!(item.status, PromptStatus::Pending | PromptStatus::Launching))
        .count();
    if !book.prompts.is_empty() {
        out.push_str(&format!(
            "\nqueued prompts: {pending} pending, {} total\n",
            book.prompts.len()
        ));
        for item in &book.prompts {
            let preview: String = item.prompt.chars().take(60).collect();
            out.push_str(&format!(
                "  {}  {:<6} {:<12} {:<9} {}  {preview:?}\n",
                item.id,
                item.agent.name(),
                prompt_queue::describe_when(item),
                format!("{:?}", item.status).to_lowercase(),
                item.cwd.display()
            ));
        }
    }
    Ok(out)
}

pub fn resume_now(paths: &Paths, target: &Target) -> Result<usize> {
    let now = now_unix();
    let touched = ledger::update(paths, |book| {
        let mut count = 0;
        for record in book.sessions.values_mut() {
            if !target.matches(record)
                || record.status == Status::Cancelled
                || record.status == Status::Resumed
            {
                continue;
            }
            record.status = Status::Stopped;
            record.reset_at = now;
            record.reset_source = ledger::ResetSource::Manual;
            record.manual = true;
            record.last_error = None;
            count += 1;
        }
        count
    })?;
    if touched > 0 {
        crate::resumer::spawn_if_needed(paths)?;
    }
    Ok(touched)
}

pub fn cancel(paths: &Paths, target: &Target) -> Result<usize> {
    ledger::update(paths, |book| {
        let mut count = 0;
        for record in book.sessions.values_mut() {
            if target.matches(record) && record.is_open() {
                record.status = Status::Cancelled;
                count += 1;
            }
        }
        count
    })
}

pub fn set_message(paths: &Paths, target: &Target, message: Option<String>) -> Result<usize> {
    ledger::update(paths, |book| {
        let mut count = 0;
        for record in book.sessions.values_mut() {
            if target.matches(record) && record.is_open() {
                record.resume_message = message.clone();
                count += 1;
            }
        }
        count
    })
}

pub enum Target {
    All,
    Id(String),
}

impl Target {
    fn matches(&self, record: &SessionRecord) -> bool {
        match self {
            Target::All => true,
            Target::Id(id) => {
                record.key.starts_with(id.as_str())
                    || record
                        .session_id
                        .as_deref()
                        .is_some_and(|session| session.starts_with(id.as_str()))
            }
        }
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Stopped => "WAITING",
        Status::Resuming => "RESUMING",
        Status::Resumed => "RESUMED",
        Status::Failed => "FAILED",
        Status::Cancelled => "CANCELLED",
    }
}

fn limit_label(record: &SessionRecord) -> &'static str {
    match record.stop {
        ledger::StopKind::UsageLimit => match record.limit {
            ledger::LimitType::FiveHour => "5h limit",
            ledger::LimitType::Weekly => "weekly limit",
            ledger::LimitType::Model => "model limit",
            ledger::LimitType::Unknown => "limit",
        },
        ledger::StopKind::Overload => "overload",
        ledger::StopKind::StreamCut => "cut off",
    }
}
