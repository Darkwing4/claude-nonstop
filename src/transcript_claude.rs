use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

const SCAN_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserRecord {
    OurPrompt,
    ToolResult,
    UserPrompt,
}

pub fn classify_user_record(record: &Value, prompts: &[&str]) -> Option<UserRecord> {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = record
        .get("message")
        .and_then(|message| message.get("content"))?;
    if let Some(text) = content.as_str() {
        return Some(if is_prompt(text, prompts) {
            UserRecord::OurPrompt
        } else {
            UserRecord::UserPrompt
        });
    }
    let blocks = content.as_array()?;
    if blocks.is_empty() {
        return None;
    }
    if blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return Some(UserRecord::ToolResult);
    }
    let texts: Vec<&str> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if texts.is_empty() {
        return None;
    }
    let joined = texts.join("\n");
    Some(if is_prompt(&joined, prompts) {
        UserRecord::OurPrompt
    } else {
        UserRecord::UserPrompt
    })
}

fn is_prompt(text: &str, prompts: &[&str]) -> bool {
    let trimmed = text.trim();
    prompts.iter().any(|prompt| prompt.trim() == trimmed)
}

pub fn progressed_since(path: &Path, offset: u64, prompts: &[&str]) -> io::Result<bool> {
    let size = std::fs::metadata(path)?.len();
    if offset > size {
        return Ok(true);
    }
    let start = offset.max(size.saturating_sub(SCAN_BYTES));
    let kinds = kinds_after(path, start, start > offset, prompts)?;
    if let Some(last_prompt) = kinds
        .iter()
        .rposition(|kind| *kind == Some(UserRecord::OurPrompt))
    {
        return Ok(kinds[last_prompt + 1..].iter().any(|kind| {
            matches!(
                kind,
                Some(UserRecord::UserPrompt) | Some(UserRecord::ToolResult)
            )
        }));
    }
    if start > offset {
        return Ok(kinds.iter().any(|kind| {
            matches!(
                kind,
                Some(UserRecord::UserPrompt) | Some(UserRecord::ToolResult)
            )
        }));
    }
    Ok(false)
}

fn kinds_after(
    path: &Path,
    start: u64,
    drop_partial_first: bool,
    prompts: &[&str],
) -> io::Result<Vec<Option<UserRecord>>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let mut lines: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
    if drop_partial_first && !lines.is_empty() {
        lines.remove(0);
    }
    let mut kinds = Vec::new();
    for raw in lines {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(raw) {
            Ok(record) => kinds.push(classify_user_record(&record, prompts)),
            Err(_) => continue,
        }
    }
    Ok(kinds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    const RETRY: &str =
        "Your response above was cut off mid-stream. Resume directly from where it stops.";

    fn user(text: &str) -> String {
        serde_json::json!({"type":"user","message":{"role":"user","content":text}}).to_string()
    }

    fn user_blocks(text: &str) -> String {
        serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":text}]}}).to_string()
    }

    fn sidechain(text: &str) -> String {
        serde_json::json!({"type":"user","isSidechain":true,"message":{"role":"user","content":text}}).to_string()
    }

    fn tool_result() -> String {
        serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}).to_string()
    }

    fn assistant(text: &str) -> String {
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text}]}}).to_string()
    }

    fn lost() -> String {
        serde_json::json!({"type":"assistant","isApiErrorMessage":true,"error":"server_error","message":{"role":"assistant","content":[{"type":"text","text":"API Error: Connection lost mid-response."}]}}).to_string()
    }

    fn transcript(name: &str, lines: &[String]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("claude-nonstop-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn append(path: &Path, lines: &[String]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all((lines.join("\n") + "\n").as_bytes())
            .unwrap();
    }

    #[test]
    fn retry_prompt_partial_output_and_errors_are_not_progress() {
        let path = transcript("progress.jsonl", &[user("do X"), assistant("part"), lost()]);
        let offset = std::fs::metadata(&path).unwrap().len();
        append(&path, &[user(RETRY), assistant("more"), lost()]);
        assert!(!progressed_since(&path, offset, &[RETRY]).unwrap());
        append(
            &path,
            &[user(RETRY), assistant("calling"), tool_result(), lost()],
        );
        assert!(progressed_since(&path, offset, &[RETRY]).unwrap());
    }

    #[test]
    fn fresh_user_prompt_after_retry_is_progress() {
        let path = transcript("prompt.jsonl", &[user("do X"), lost()]);
        let offset = std::fs::metadata(&path).unwrap().len();
        append(&path, &[user(RETRY), lost(), user("go on"), lost()]);
        assert!(progressed_since(&path, offset, &[RETRY]).unwrap());
    }

    #[test]
    fn noise_records_are_ignored() {
        let path = transcript("noise.jsonl", &[user("do X"), lost()]);
        let offset = std::fs::metadata(&path).unwrap().len();
        append(
            &path,
            &[
                user_blocks(RETRY),
                sidechain(RETRY),
                "{\"type\":\"assistant\"".to_owned(),
                serde_json::json!({"type":"system"}).to_string(),
                String::new(),
                lost(),
            ],
        );
        assert!(!progressed_since(&path, offset, &[RETRY]).unwrap());
        assert!(
            progressed_since(
                &path,
                std::fs::metadata(&path).unwrap().len() + 10,
                &[RETRY]
            )
            .unwrap()
        );
    }

    #[test]
    fn late_flushed_records_and_missing_retry_are_not_progress() {
        let path = transcript("late.jsonl", &[]);
        let offset = std::fs::metadata(&path).unwrap().len();
        append(&path, &[user("do X"), lost()]);
        assert!(!progressed_since(&path, offset, &[RETRY]).unwrap());
        append(&path, &[user(RETRY), lost()]);
        assert!(!progressed_since(&path, offset, &[RETRY]).unwrap());
    }

    #[test]
    fn records_classify_by_content() {
        let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        assert_eq!(
            classify_user_record(&parse(&user_blocks("plain")), &[RETRY]),
            Some(UserRecord::UserPrompt)
        );
        assert_eq!(
            classify_user_record(&parse(&user_blocks(RETRY)), &[RETRY]),
            Some(UserRecord::OurPrompt)
        );
        assert_eq!(
            classify_user_record(&parse(&user(&format!("  {RETRY}\n"))), &[RETRY]),
            Some(UserRecord::OurPrompt)
        );
        assert_eq!(
            classify_user_record(&parse(&tool_result()), &[RETRY]),
            Some(UserRecord::ToolResult)
        );
        assert_eq!(
            classify_user_record(&parse(&assistant("x")), &[RETRY]),
            None
        );
        assert_eq!(
            classify_user_record(&parse(&sidechain("x")), &[RETRY]),
            None
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitRecord {
    pub at: u64,
    pub text: String,
    pub kind: crate::ledger::LimitType,
}

pub fn latest_limit_record(
    path: &Path,
    overload: &crate::config::Overload,
) -> io::Result<Option<LimitRecord>> {
    let size = std::fs::metadata(path)?.len();
    let start = size.saturating_sub(SCAN_BYTES);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let mut lines: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    for raw in lines.iter().rev() {
        if !raw.windows(12).any(|window| window == b"\"rate_limit\"") {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(raw) else {
            continue;
        };
        if let Some(found) = classify_limit_record(&record, overload) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub fn classify_limit_record(
    record: &Value,
    overload: &crate::config::Overload,
) -> Option<LimitRecord> {
    if record.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    if record.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if record.get("error").and_then(Value::as_str) != Some("rate_limit") {
        return None;
    }
    let text = record_text(record);
    if crate::hook::classify("rate_limit", Some(&text), overload)
        != crate::hook::HookKind::RateLimit
    {
        return None;
    }
    let at = record_timestamp(record).unwrap_or(0);
    Some(LimitRecord {
        at,
        kind: limit_kind(&text),
        text,
    })
}

pub fn limit_kind(text: &str) -> crate::ledger::LimitType {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("/usage-credits") || lowered.contains("switch models") {
        return crate::ledger::LimitType::Model;
    }
    if lowered.contains("week") {
        return crate::ledger::LimitType::Weekly;
    }
    if lowered.contains("session limit") || lowered.contains("-hour limit") {
        return crate::ledger::LimitType::FiveHour;
    }
    crate::ledger::LimitType::Unknown
}

pub fn progressed_after(path: &Path, since: u64) -> io::Result<bool> {
    let size = std::fs::metadata(path)?.len();
    let start = size.saturating_sub(SCAN_BYTES);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let mut lines: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    for raw in lines.iter().rev() {
        let Ok(record) = serde_json::from_slice::<Value>(raw) else {
            continue;
        };
        let Some(at) = record_timestamp(&record) else {
            continue;
        };
        if at <= since {
            break;
        }
        if is_progress_record(&record) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_progress_record(record: &Value) -> bool {
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match record.get("type").and_then(Value::as_str) {
        Some("user") => record
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            }),
        Some("assistant") => {
            if record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
                return false;
            }
            let usage = record.pointer("/message/usage");
            let total: u64 = [
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ]
            .iter()
            .filter_map(|key| {
                usage
                    .and_then(|usage| usage.get(key))
                    .and_then(Value::as_u64)
            })
            .sum();
            total > 0
        }
        _ => false,
    }
}

fn record_text(record: &Value) -> String {
    match record.pointer("/message/content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn record_timestamp(record: &Value) -> Option<u64> {
    let stamp = record.get("timestamp").and_then(Value::as_str)?;
    let parsed: jiff::Timestamp = stamp.parse().ok()?;
    u64::try_from(parsed.as_second()).ok()
}

#[cfg(test)]
mod limit_tests {
    use super::*;
    use crate::ledger::LimitType;

    fn latest_limit_record(path: &Path) -> io::Result<Option<LimitRecord>> {
        super::latest_limit_record(path, &crate::config::Overload::default())
    }

    #[test]
    fn capacity_errors_do_not_become_usage_limits() {
        let mut overload = crate::config::Overload::default();
        overload
            .message_contains
            .push("upstream pool exhausted".to_owned());
        for text in [
            "API Error: Server is temporarily limiting requests (not your usage limit) · only reserved subscriptions are left and their shared part is used up",
            "UPSTREAM POOL EXHAUSTED",
        ] {
            let raw = limit(text, "2026-09-03T06:43:54.740Z");
            let record = serde_json::from_str(&raw).unwrap();
            assert!(classify_limit_record(&record, &overload).is_none());
            let file = path("capacity.jsonl", &[raw]);
            assert!(
                super::latest_limit_record(&file, &overload)
                    .unwrap()
                    .is_none()
            );
        }
    }

    fn limit(text: &str, stamp: &str) -> String {
        serde_json::json!({"type":"assistant","timestamp":stamp,"isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"message":{"role":"assistant","content":[{"type":"text","text":text}],"usage":{"input_tokens":0,"output_tokens":0}}}).to_string()
    }

    fn worked(stamp: &str) -> String {
        serde_json::json!({"type":"assistant","timestamp":stamp,"message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":12,"output_tokens":3}}}).to_string()
    }

    fn path(name: &str, lines: &[String]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("claude-nonstop-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, lines.join("\n") + "\n").unwrap();
        file
    }

    #[test]
    fn newest_rate_limit_record_wins_and_is_classified() {
        let file = path(
            "limits.jsonl",
            &[
                limit(
                    "You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.",
                    "2026-09-03T05:54:29.819Z",
                ),
                worked("2026-09-03T06:00:00.000Z"),
                limit(
                    "You've hit your session limit · resets 5:30pm (Asia/Bangkok)",
                    "2026-09-03T06:43:54.740Z",
                ),
            ],
        );
        let found = latest_limit_record(&file).unwrap().unwrap();
        assert_eq!(found.kind, LimitType::FiveHour);
        assert!(found.text.contains("resets 5:30pm"));
        assert_eq!(found.at, 1788417834);
        assert_eq!(
            limit_kind("You've hit your weekly limit · resets Aug 31, 12pm"),
            LimitType::Weekly
        );
        assert_eq!(
            limit_kind("You've hit your limit · resets 2am"),
            LimitType::Unknown
        );
        assert_eq!(
            limit_kind("You've reached your Fable limit. Run /usage-credits"),
            LimitType::Model
        );
        assert!(
            latest_limit_record(&path("none.jsonl", &[worked("2026-09-03T06:00:00.000Z")]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn progress_after_an_instant_needs_real_work() {
        let file = path(
            "progress.jsonl",
            &[
                worked("2026-09-03T06:00:00.000Z"),
                limit(
                    "You've hit your session limit · resets 5:30pm (Asia/Bangkok)",
                    "2026-09-03T06:43:54.740Z",
                ),
            ],
        );
        assert!(!progressed_after(&file, 1788417834).unwrap());
        assert!(progressed_after(&file, 1788415000).unwrap());
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(&worked("2026-09-03T07:00:00.000Z"));
        text.push('\n');
        std::fs::write(&file, text).unwrap();
        assert!(progressed_after(&file, 1788417834).unwrap());
    }
}
