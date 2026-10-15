use std::sync::LazyLock;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use regex::Regex;

use crate::error::{Error, Result};
use crate::reset_time::{Clock, civil_instant, clock_from_parts, parse_duration};

static CLOCK_24: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,2}):(\d{2})$").expect("clock regex"));
static CLOCK_12: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(\d{1,2})(?::(\d{2}))?\s*(am|pm)$").expect("12h clock regex")
});

pub fn parse_at(text: &str, now: Timestamp, tz: &TimeZone) -> Result<u64> {
    let trimmed = text.trim();
    if let Some(duration) = trimmed.strip_prefix('+') {
        let span = parse_duration(duration)
            .ok_or_else(|| Error::Invalid(format!("cannot read a duration from {trimmed:?}")))?;
        return to_unix(now.as_second() + span.as_secs() as i64);
    }
    if is_epoch(trimmed) {
        let value: i64 = trimmed
            .parse()
            .map_err(|_| Error::Invalid(format!("{trimmed:?} is not a number")))?;
        let seconds = if trimmed.len() == 13 {
            value / 1000
        } else {
            value
        };
        return future_only(seconds, now);
    }
    if let Some(captures) = CLOCK_24.captures(trimmed) {
        let clock = clock_from_parts(&captures[1], Some(&captures[2]), None);
        return next_clock(clock, trimmed, now, tz);
    }
    if let Some(captures) = CLOCK_12.captures(trimmed) {
        let minute = captures.get(2).map(|m| m.as_str());
        let clock = clock_from_parts(&captures[1], minute, Some(&captures[3]));
        return next_clock(clock, trimmed, now, tz);
    }
    if let Ok(timestamp) = trimmed.parse::<Timestamp>() {
        return future_only(timestamp.as_second(), now);
    }
    if let Ok(civil) = trimmed.parse::<jiff::civil::DateTime>() {
        let zoned = tz
            .to_ambiguous_zoned(civil)
            .later()
            .map_err(|error| Error::Invalid(format!("{trimmed:?}: {error}")))?;
        return future_only(zoned.timestamp().as_second(), now);
    }
    Err(Error::Invalid(format!(
        "cannot read a time from {trimmed:?}: use +2h30m, 14:30, 7pm, an ISO date, or an epoch"
    )))
}

fn is_epoch(text: &str) -> bool {
    matches!(text.len(), 10 | 13) && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn next_clock(clock: Option<Clock>, text: &str, now: Timestamp, tz: &TimeZone) -> Result<u64> {
    let Some(clock) = clock else {
        return Err(Error::Invalid(format!(
            "{text:?} is not a valid clock time"
        )));
    };

    let today = now.to_zoned(tz.clone()).date();
    let tomorrow = today
        .tomorrow()
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let next = [today, tomorrow]
        .into_iter()
        .filter_map(|date| civil_instant(tz, date, clock.hour, clock.minute))
        .find(|at| *at > now);

    match next {
        Some(at) => to_unix(at.as_second()),
        None => Err(Error::Invalid(format!(
            "{text:?} did not resolve to a future instant"
        ))),
    }
}

fn future_only(seconds: i64, now: Timestamp) -> Result<u64> {
    if seconds <= now.as_second() {
        return Err(Error::Invalid(
            "that time is already in the past".to_owned(),
        ));
    }
    to_unix(seconds)
}

fn to_unix(seconds: i64) -> Result<u64> {
    u64::try_from(seconds).map_err(|_| Error::Invalid("time before 1970".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn every_form_resolves_to_a_future_instant() {
        let tz = TimeZone::get("Europe/Moscow").unwrap();
        let now = at("2026-09-09T08:00:00Z");
        assert_eq!(
            parse_at("+2h30m", now, &tz).unwrap(),
            at("2026-09-09T10:30:00Z").as_second() as u64
        );
        assert_eq!(
            parse_at("14:30", now, &tz).unwrap(),
            at("2026-09-09T11:30:00Z").as_second() as u64
        );
        assert_eq!(
            parse_at("10:00", now, &tz).unwrap(),
            at("2026-09-10T07:00:00Z").as_second() as u64
        );
        assert_eq!(
            parse_at("7pm", now, &tz).unwrap(),
            at("2026-09-09T16:00:00Z").as_second() as u64
        );
        assert_eq!(
            parse_at("12:05 AM", now, &tz).unwrap(),
            at("2026-09-09T21:05:00Z").as_second() as u64
        );
        assert_eq!(
            parse_at("2026-09-09T20:00", now, &tz).unwrap(),
            at("2026-09-09T17:00:00Z").as_second() as u64
        );
        assert_eq!(
            parse_at("2026-09-09T20:00:00+03:00", now, &tz).unwrap(),
            at("2026-09-09T17:00:00Z").as_second() as u64
        );
        assert_eq!(parse_at("1789000000", now, &tz).unwrap(), 1789000000);
        assert_eq!(parse_at("1789000000000", now, &tz).unwrap(), 1789000000);
        assert!(parse_at("2026-01-01T00:00", now, &tz).is_err());
        assert!(parse_at("13pm", now, &tz).is_err());
        assert!(parse_at("24:00", now, &tz).is_err());
        assert!(parse_at("7:60pm", now, &tz).is_err());
        assert!(parse_at("soon", now, &tz).is_err());
    }
}
