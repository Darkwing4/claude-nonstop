use std::sync::LazyLock;

use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use regex::Regex;

use crate::error::{Error, Result};
use crate::reset_time::parse_duration;

static CLOCK_24: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,2}):(\d{2})$").expect("clock regex"));
static CLOCK_12: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(\d{1,2})(?::(\d{2}))?\s*(am|pm)$").expect("12h clock regex")
});
static EPOCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{10}$|^\d{13}$").expect("epoch regex"));

pub fn parse_at(text: &str, now: Timestamp, tz: &TimeZone) -> Result<u64> {
    let trimmed = text.trim();
    if let Some(duration) = trimmed.strip_prefix('+') {
        let span = parse_duration(duration)
            .ok_or_else(|| Error::Invalid(format!("cannot read a duration from {trimmed:?}")))?;
        return to_unix(now.as_second() + span.as_secs() as i64);
    }
    if EPOCH.is_match(trimmed) {
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
        let hour: i8 = captures[1]
            .parse()
            .map_err(|_| Error::Invalid(format!("bad hour in {trimmed:?}")))?;
        let minute: i8 = captures[2]
            .parse()
            .map_err(|_| Error::Invalid(format!("bad minute in {trimmed:?}")))?;
        return next_clock(hour, minute, now, tz);
    }
    if let Some(captures) = CLOCK_12.captures(trimmed) {
        let mut hour: i8 = captures[1]
            .parse()
            .map_err(|_| Error::Invalid(format!("bad hour in {trimmed:?}")))?;
        let minute: i8 = captures
            .get(2)
            .map(|m| m.as_str().parse().unwrap_or(0))
            .unwrap_or(0);
        if !(1..=12).contains(&hour) {
            return Err(Error::Invalid(format!(
                "{trimmed:?}: a 12-hour clock needs 1..12"
            )));
        }
        let pm = captures[3].eq_ignore_ascii_case("pm");
        if hour == 12 {
            hour = 0;
        }
        if pm {
            hour += 12;
        }
        return next_clock(hour, minute, now, tz);
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

fn next_clock(hour: i8, minute: i8, now: Timestamp, tz: &TimeZone) -> Result<u64> {
    let time = Time::new(hour, minute, 0, 0)
        .map_err(|error| Error::Invalid(format!("{hour:02}:{minute:02}: {error}")))?;
    let today: Date = now.to_zoned(tz.clone()).date();
    for offset in 0..2 {
        let day = today
            .checked_add(jiff::Span::new().days(offset))
            .map_err(|error| Error::Invalid(error.to_string()))?;
        let zoned = tz
            .to_ambiguous_zoned(day.to_datetime(time))
            .later()
            .map_err(|error| Error::Invalid(error.to_string()))?;
        if zoned.timestamp() > now {
            return to_unix(zoned.timestamp().as_second());
        }
    }
    Err(Error::Invalid(format!(
        "{hour:02}:{minute:02} did not resolve to a future instant"
    )))
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
        assert!(parse_at("soon", now, &tz).is_err());
    }
}
