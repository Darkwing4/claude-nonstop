use std::sync::LazyLock;
use std::time::Duration;

use jiff::civil::{Date, DateTime, Weekday};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use regex::{Captures, Regex};

use crate::ledger::ResetSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallDate {
    Today,
    Weekday(Weekday),
    MonthDay {
        month: i8,
        day: i8,
        year: Option<i16>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResetSpec {
    Wall {
        date: WallDate,
        hour: u8,
        minute: u8,
        ambiguous: bool,
        tz: Option<TimeZone>,
    },
    Relative(Duration),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub at: Timestamp,
    pub source: ResetSource,
}

const MONTH: &str = concat!(
    r"(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?",
    r"|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\b\.?",
);
const DOW: &str = concat!(
    r"(mon(?:day)?|tue(?:s(?:day)?)?|wed(?:nesday)?|thu(?:rs?(?:day)?)?|fri(?:day)?",
    r"|sat(?:urday)?|sun(?:day)?)\b",
);
const DAY: &str = r"(\d{1,2})(?:st|nd|rd|th)?";
const CLOCK: &str = r"(\d{1,2})(?::(\d{2}))?\s*(am|pm)?\b";
const ZONE: &str = r"(?:\s*\(([A-Za-z][A-Za-z0-9_+\-/]*)\))?";
const DURATION_SPAN: &str = r"((?:[~\s,]*(?:and\s+)?\d+(?:\.\d+)?\s*[a-z]+)+)";
const DURATION_UNIT: &str =
    r"ms|milliseconds?|s|secs?|seconds?|m|mins?|minutes?|h|hrs?|hours?|d|days?|w|weeks?";

static RETRY_BANNER: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\bretrying in\s+{DURATION_SPAN}\s*attempt\s*#\d+");
    Regex::new(&pattern).expect("retry banner regex")
});

static TRY_AGAIN_AT_DATE: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(
        r"(?i)\btry again at\s+{MONTH}\s+{DAY},?\s+(\d{{4}})\s+(\d{{1,2}}):(\d{{2}})\s*(am|pm)\b"
    );
    Regex::new(&pattern).expect("try again at date regex")
});

static TRY_AGAIN_AT_CLOCK: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = r"(?i)\btry again at\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm)\b";
    Regex::new(pattern).expect("try again at clock regex")
});

static RESETS_MONTH_DAY: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(
        r"(?i)\bresets?\s+(?:on\s+)?{MONTH}\s+{DAY},?\s+(?:(\d{{4}}),?\s+)?(?:at\s+)?{CLOCK}{ZONE}"
    );
    Regex::new(&pattern).expect("resets month day regex")
});

static RESETS_CLOCK: LazyLock<Regex> = LazyLock::new(|| {
    let pattern =
        format!(r"(?i)\bresets?\s+(?:at\s+)?(?:on\s+)?(?:{DOW},?\s+)?(?:at\s+)?{CLOCK}{ZONE}");
    Regex::new(&pattern).expect("resets clock regex")
});

static RESETS_WEEKDAY: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\bresets?\s+(?:on\s+)?{DOW}");
    Regex::new(&pattern).expect("resets weekday regex")
});

static RESETS_IN: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(
        r"(?i)\b(?:try again|resets?|refreshes|retry|wait(?:\s+for)?)\s+in[:\s]\s*{DURATION_SPAN}"
    );
    Regex::new(&pattern).expect("resets in regex")
});

static WAIT_FOR: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\bwait(?:\s+for)?\s+{DURATION_SPAN}");
    Regex::new(&pattern).expect("wait for regex")
});

static DURATION_UNIT_AHEAD: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)^\s*(?:{DURATION_UNIT})\b");
    Regex::new(&pattern).expect("duration unit ahead regex")
});

struct Clock {
    hour: u8,
    minute: u8,
    ambiguous: bool,
}

pub fn parse_reset(text: &str) -> Option<ResetSpec> {
    parse_retry_banner(text)
        .or_else(|| parse_try_again_at_date(text))
        .or_else(|| parse_try_again_at_clock(text))
        .or_else(|| parse_resets_month_day(text))
        .or_else(|| parse_resets_clock(text))
        .or_else(|| parse_resets_weekday(text))
        .or_else(|| parse_resets_in(text))
        .or_else(|| parse_wait_for(text))
}

fn parse_retry_banner(text: &str) -> Option<ResetSpec> {
    let captures = RETRY_BANNER.captures(text)?;
    relative_from_capture(&captures, 1)
}

fn parse_try_again_at_date(text: &str) -> Option<ResetSpec> {
    let captures = TRY_AGAIN_AT_DATE.captures(text)?;
    let month = month_number(capture(&captures, 1)?)?;
    let day = day_number(capture(&captures, 2)?)?;
    let year: i16 = capture(&captures, 3)?.parse().ok()?;
    let clock = clock_from_parts(
        capture(&captures, 4)?,
        capture(&captures, 5),
        capture(&captures, 6),
    )?;
    Some(wall(
        WallDate::MonthDay {
            month,
            day,
            year: Some(year),
        },
        clock,
        None,
    ))
}

fn parse_try_again_at_clock(text: &str) -> Option<ResetSpec> {
    let captures = TRY_AGAIN_AT_CLOCK.captures(text)?;
    let clock = clock_from_parts(
        capture(&captures, 1)?,
        capture(&captures, 2),
        capture(&captures, 3),
    )?;
    Some(wall(WallDate::Today, clock, None))
}

fn parse_resets_month_day(text: &str) -> Option<ResetSpec> {
    let captures = RESETS_MONTH_DAY.captures(text)?;
    let month = month_number(capture(&captures, 1)?)?;
    let day = day_number(capture(&captures, 2)?)?;
    let year = match capture(&captures, 3) {
        Some(raw_year) => Some(raw_year.parse::<i16>().ok()?),
        None => None,
    };
    let clock = clock_from_parts(
        capture(&captures, 4)?,
        capture(&captures, 5),
        capture(&captures, 6),
    )?;
    let tz = zone_from_capture(capture(&captures, 7))?;
    Some(wall(WallDate::MonthDay { month, day, year }, clock, tz))
}

fn parse_resets_clock(text: &str) -> Option<ResetSpec> {
    let captures = RESETS_CLOCK.captures(text)?;
    let whole_match = captures.get(0)?;
    let text_after_match = &text[whole_match.end()..];
    if DURATION_UNIT_AHEAD.is_match(text_after_match) {
        return None;
    }
    let date = match capture(&captures, 1) {
        Some(raw_weekday) => WallDate::Weekday(weekday_from_name(raw_weekday)?),
        None => WallDate::Today,
    };
    let clock = clock_from_parts(
        capture(&captures, 2)?,
        capture(&captures, 3),
        capture(&captures, 4),
    )?;
    let tz = zone_from_capture(capture(&captures, 5))?;
    Some(wall(date, clock, tz))
}

fn parse_resets_weekday(text: &str) -> Option<ResetSpec> {
    let captures = RESETS_WEEKDAY.captures(text)?;
    let weekday = weekday_from_name(capture(&captures, 1)?)?;
    let midnight = Clock {
        hour: 0,
        minute: 0,
        ambiguous: false,
    };
    Some(wall(WallDate::Weekday(weekday), midnight, None))
}

fn parse_resets_in(text: &str) -> Option<ResetSpec> {
    let captures = RESETS_IN.captures(text)?;
    relative_from_capture(&captures, 1)
}

fn parse_wait_for(text: &str) -> Option<ResetSpec> {
    let captures = WAIT_FOR.captures(text)?;
    relative_from_capture(&captures, 1)
}

fn capture<'t>(captures: &Captures<'t>, index: usize) -> Option<&'t str> {
    captures.get(index).map(|m| m.as_str())
}

fn relative_from_capture(captures: &Captures<'_>, index: usize) -> Option<ResetSpec> {
    let duration = parse_duration(capture(captures, index)?)?;
    Some(ResetSpec::Relative(duration))
}

fn wall(date: WallDate, clock: Clock, tz: Option<TimeZone>) -> ResetSpec {
    ResetSpec::Wall {
        date,
        hour: clock.hour,
        minute: clock.minute,
        ambiguous: clock.ambiguous,
        tz,
    }
}

fn zone_from_capture(raw_zone: Option<&str>) -> Option<Option<TimeZone>> {
    match raw_zone {
        Some(name) => Some(Some(TimeZone::get(name).ok()?)),
        None => Some(None),
    }
}

fn clock_from_parts(
    raw_hour: &str,
    raw_minute: Option<&str>,
    meridiem: Option<&str>,
) -> Option<Clock> {
    let hour: u8 = raw_hour.parse().ok()?;
    let minute: u8 = match raw_minute {
        Some(raw) => raw.parse().ok()?,
        None => 0,
    };
    if minute > 59 {
        return None;
    }
    let Some(meridiem) = meridiem else {
        if hour > 23 {
            return None;
        }
        let ambiguous = (1..=12).contains(&hour);
        return Some(Clock {
            hour,
            minute,
            ambiguous,
        });
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    let is_pm = meridiem.eq_ignore_ascii_case("pm");
    let hour_24 = match (hour, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (_, false) => hour,
        (_, true) => hour + 12,
    };
    Some(Clock {
        hour: hour_24,
        minute,
        ambiguous: false,
    })
}

fn month_number(name: &str) -> Option<i8> {
    let prefix = name.get(..3)?.to_ascii_lowercase();
    let month = match prefix.as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    Some(month)
}

fn day_number(raw_day: &str) -> Option<i8> {
    let day: i8 = raw_day.parse().ok()?;
    if (1..=31).contains(&day) {
        Some(day)
    } else {
        None
    }
}

fn weekday_from_name(name: &str) -> Option<Weekday> {
    let prefix = name.get(..3)?.to_ascii_lowercase();
    let weekday = match prefix.as_str() {
        "mon" => Weekday::Monday,
        "tue" => Weekday::Tuesday,
        "wed" => Weekday::Wednesday,
        "thu" => Weekday::Thursday,
        "fri" => Weekday::Friday,
        "sat" => Weekday::Saturday,
        "sun" => Weekday::Sunday,
        _ => return None,
    };
    Some(weekday)
}

pub fn parse_duration(text: &str) -> Option<Duration> {
    let mut rest = text;
    let mut total = Duration::ZERO;
    let mut found_any_part = false;
    loop {
        rest = skip_duration_separators(rest);
        if rest.is_empty() {
            break;
        }
        let (number, after_number) = take_number(rest)?;
        let (unit, after_unit) = take_unit(after_number.trim_start())?;
        let unit_seconds = unit_seconds(unit)?;
        let part = Duration::try_from_secs_f64(number * unit_seconds).ok()?;
        total = total.checked_add(part)?;
        found_any_part = true;
        rest = after_unit;
    }
    if found_any_part { Some(total) } else { None }
}

fn skip_duration_separators(text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '~' || c == ',');
        let starts_with_and = rest
            .get(..3)
            .is_some_and(|word| word.eq_ignore_ascii_case("and"));
        let and_is_whole_word = rest
            .get(3..)
            .and_then(|after| after.chars().next())
            .is_none_or(|c| !c.is_alphanumeric());
        if starts_with_and && and_is_whole_word {
            rest = &rest[3..];
            continue;
        }
        return rest;
    }
}

fn take_number(text: &str) -> Option<(f64, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let raw = &text[..end];
    if !raw.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let number: f64 = raw.parse().ok()?;
    Some((number, &text[end..]))
}

fn take_unit(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    Some((&text[..end], &text[end..]))
}

fn unit_seconds(unit: &str) -> Option<f64> {
    let seconds = match unit.to_ascii_lowercase().as_str() {
        "ms" | "millisecond" | "milliseconds" => 0.001,
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "week" | "weeks" => 604_800.0,
        _ => return None,
    };
    Some(seconds)
}

const MONTH_DAY_HORIZON: SignedDuration = SignedDuration::from_hours(10 * 24);
const PASSED_CLOCK_ROLLS_TO_TOMORROW_AFTER: SignedDuration = SignedDuration::from_hours(6);
const AMBIGUOUS_PAST_GRACE: SignedDuration = SignedDuration::from_hours(1);
const STALE_FIVE_HOUR_WINDOW: SignedDuration = SignedDuration::from_mins(5 * 60 + 30);

pub fn resolve(
    spec: Option<&ResetSpec>,
    now: Timestamp,
    banner_at: Option<Timestamp>,
    host_tz: &TimeZone,
    margin: Duration,
    probe: Duration,
) -> Resolved {
    let fallback = Resolved {
        at: now.checked_add(probe).unwrap_or(now),
        source: ResetSource::Fallback,
    };
    let Some(spec) = spec else {
        return fallback;
    };
    match spec {
        ResetSpec::Relative(delay) => {
            let anchor = banner_at.unwrap_or(now);
            let after_delay = anchor
                .checked_add(*delay)
                .and_then(|t| t.checked_add(margin));
            match after_delay {
                Ok(at) => Resolved {
                    at,
                    source: ResetSource::Relative,
                },
                Err(_) => fallback,
            }
        }
        ResetSpec::Wall {
            date,
            hour,
            minute,
            ambiguous,
            tz,
        } => {
            let tz = tz.as_ref().unwrap_or(host_tz);
            let anchor = banner_at.unwrap_or(now);
            let clock = Clock {
                hour: *hour,
                minute: *minute,
                ambiguous: *ambiguous,
            };
            let wall_at = match date {
                WallDate::Today => resolve_today(tz, &clock, anchor, banner_at.is_some(), now),
                WallDate::Weekday(weekday) => resolve_weekday(tz, &clock, *weekday, anchor),
                WallDate::MonthDay { month, day, year } => {
                    resolve_month_day(tz, &clock, *month, *day, *year, anchor, now)
                }
            };
            let Some(wall_at) = wall_at else {
                return fallback;
            };
            let base = if wall_at <= now { now } else { wall_at };
            match base.checked_add(margin) {
                Ok(at) => Resolved {
                    at,
                    source: ResetSource::Absolute,
                },
                Err(_) => fallback,
            }
        }
    }
}

pub fn stale_five_hour_clamp(at: Timestamp, detected_at: Timestamp, margin: Duration) -> Timestamp {
    let Ok(at_without_margin) = at.checked_sub(margin) else {
        return at;
    };
    let Ok(stale_boundary) = detected_at.checked_add(STALE_FIVE_HOUR_WINDOW) else {
        return at;
    };
    if at_without_margin > stale_boundary {
        detected_at.checked_add(margin).unwrap_or(at)
    } else {
        at
    }
}

fn civil_instant(tz: &TimeZone, date: Date, hour: u8, minute: u8) -> Option<Timestamp> {
    let datetime = DateTime::new(
        date.year(),
        date.month(),
        date.day(),
        hour as i8,
        minute as i8,
        0,
        0,
    )
    .ok()?;
    tz.to_ambiguous_timestamp(datetime).later().ok()
}

fn hour_candidates(clock: &Clock) -> Vec<u8> {
    if clock.ambiguous {
        vec![clock.hour, (clock.hour + 12) % 24]
    } else {
        vec![clock.hour]
    }
}

fn instants_on(tz: &TimeZone, clock: &Clock, date: Date) -> Vec<Timestamp> {
    hour_candidates(clock)
        .into_iter()
        .filter_map(|hour| civil_instant(tz, date, hour, clock.minute))
        .collect()
}

fn resolve_today(
    tz: &TimeZone,
    clock: &Clock,
    anchor: Timestamp,
    banner_known: bool,
    now: Timestamp,
) -> Option<Timestamp> {
    let today = anchor.to_zoned(tz.clone()).date();
    let tomorrow = today.tomorrow().ok()?;
    if clock.ambiguous {
        return resolve_today_ambiguous(tz, clock, today, tomorrow, anchor, now);
    }
    let today_at = civil_instant(tz, today, clock.hour, clock.minute)?;
    if today_at > anchor {
        return Some(today_at);
    }
    let tomorrow_at = civil_instant(tz, tomorrow, clock.hour, clock.minute)?;
    if banner_known {
        return Some(tomorrow_at);
    }
    let passed_by = anchor.duration_since(today_at);
    if passed_by > PASSED_CLOCK_ROLLS_TO_TOMORROW_AFTER {
        return Some(tomorrow_at);
    }
    Some(today_at)
}

fn resolve_today_ambiguous(
    tz: &TimeZone,
    clock: &Clock,
    today: Date,
    tomorrow: Date,
    anchor: Timestamp,
    now: Timestamp,
) -> Option<Timestamp> {
    let today_candidates = instants_on(tz, clock, today);
    let earliest_future = today_candidates
        .iter()
        .copied()
        .filter(|t| *t > anchor)
        .min();
    if earliest_future.is_some() {
        return earliest_future;
    }
    let most_recent_past = today_candidates.iter().copied().max()?;
    let passed_by = now.duration_since(most_recent_past);
    if passed_by < AMBIGUOUS_PAST_GRACE {
        return Some(most_recent_past);
    }
    instants_on(tz, clock, tomorrow).into_iter().min()
}

fn resolve_weekday(
    tz: &TimeZone,
    clock: &Clock,
    weekday: Weekday,
    anchor: Timestamp,
) -> Option<Timestamp> {
    let mut date = anchor.to_zoned(tz.clone()).date();
    for _ in 0..9 {
        if date.weekday() == weekday {
            let earliest_after_anchor = instants_on(tz, clock, date)
                .into_iter()
                .filter(|t| *t > anchor)
                .min();
            if earliest_after_anchor.is_some() {
                return earliest_after_anchor;
            }
        }
        date = date.tomorrow().ok()?;
    }
    None
}

fn resolve_month_day(
    tz: &TimeZone,
    clock: &Clock,
    month: i8,
    day: i8,
    year: Option<i16>,
    anchor: Timestamp,
    now: Timestamp,
) -> Option<Timestamp> {
    let date = match year {
        Some(year) => Date::new(year, month, day).ok()?,
        None => {
            let base = anchor.to_zoned(tz.clone()).date().yesterday().ok()?;
            nearest_month_day_at_or_after(base, month, day)?
        }
    };
    let horizon = now.checked_add(MONTH_DAY_HORIZON).ok()?;
    instants_on(tz, clock, date)
        .into_iter()
        .filter(|t| *t > now && *t <= horizon)
        .min()
}

fn nearest_month_day_at_or_after(base: Date, month: i8, day: i8) -> Option<Date> {
    let next_year = base.year().checked_add(1)?;
    for year in [base.year(), next_year] {
        if let Ok(date) = Date::new(year, month, day) {
            if date >= base {
                return Some(date);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARGIN: Duration = Duration::from_secs(60);
    const PROBE: Duration = Duration::from_secs(15 * 60);

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    fn resolve_text(text: &str, now: &str, banner_at: Option<&str>, host: &str) -> Resolved {
        let spec = parse_reset(text);
        resolve(
            spec.as_ref(),
            ts(now),
            banner_at.map(ts),
            &zone(host),
            MARGIN,
            PROBE,
        )
    }

    fn resolve_msk(text: &str, now: &str) -> Resolved {
        resolve_text(text, now, None, "Europe/Moscow")
    }

    fn assert_absolute(resolved: &Resolved, expected: &str) {
        assert_eq!(resolved.source, ResetSource::Absolute);
        assert_eq!(resolved.at.to_string(), expected);
    }

    fn assert_relative(resolved: &Resolved, expected: &str) {
        assert_eq!(resolved.source, ResetSource::Relative);
        assert_eq!(resolved.at.to_string(), expected);
    }

    fn assert_fallback(resolved: &Resolved, now: &str) {
        assert_eq!(resolved.source, ResetSource::Fallback);
        assert_eq!(resolved.at, ts(now).checked_add(PROBE).unwrap());
    }

    fn assert_wake_now(resolved: &Resolved, now: &str) {
        assert_eq!(resolved.source, ResetSource::Absolute);
        assert_eq!(resolved.at, ts(now).checked_add(MARGIN).unwrap());
    }

    #[test]
    fn t01_resets_clock_with_zone() {
        let resolved = resolve_msk("resets 3pm (Europe/Moscow)", "2026-09-09T08:00:00Z");
        assert_absolute(&resolved, "2026-09-09T12:01:00Z");
    }

    #[test]
    fn t02_resets_clock_passed_long_ago_rolls_to_tomorrow() {
        let resolved = resolve_msk("resets 2:30am (Asia/Bangkok)", "2026-09-09T08:00:00Z");
        assert_absolute(&resolved, "2026-09-09T19:31:00Z");
    }

    #[test]
    fn t03_resets_clock_just_passed_wakes_now() {
        let now = "2026-09-08T23:03:00Z";
        let resolved = resolve_msk("resets 2am (Europe/Moscow)", now);
        assert_wake_now(&resolved, now);
    }

    #[test]
    fn t04_resets_after_midnight_rolls_to_tomorrow() {
        let resolved = resolve_msk("resets 12:04am", "2026-09-09T18:00:00Z");
        assert_absolute(&resolved, "2026-09-09T21:05:00Z");
    }

    #[test]
    fn t05_resets_clock_passed_within_grace_wakes_now() {
        let now = "2026-09-09T10:00:00Z";
        let resolved = resolve_msk("resets 9am", now);
        assert_wake_now(&resolved, now);
    }

    #[test]
    fn t06_resets_month_day_with_zone() {
        let resolved = resolve_msk("resets Aug 31, 12pm (Asia/Bangkok)", "2026-08-29T00:00:00Z");
        assert_absolute(&resolved, "2026-08-31T05:01:00Z");
    }

    #[test]
    fn t07_resets_month_day_in_the_past_falls_back() {
        let now = "2026-08-01T00:00:00Z";
        let resolved = resolve_msk("resets Jul 27, 8am (Europe/Moscow)", now);
        assert_fallback(&resolved, now);
    }

    #[test]
    fn t08_resets_month_day_at_midnight_host_zone() {
        let resolved = resolve_msk("resets Sep 1 at 12:00am", "2026-08-31T12:00:00Z");
        assert_absolute(&resolved, "2026-08-31T21:01:00Z");
    }

    #[test]
    fn t09_resets_month_day_rolls_into_next_year() {
        let resolved = resolve_msk("resets Jan 2 at 9am (UTC)", "2025-12-30T12:00:00Z");
        assert_absolute(&resolved, "2026-01-02T09:01:00Z");
    }

    #[test]
    fn t10_resets_weekday_midnight_host_zone() {
        let resolved = resolve_msk("resets Mon 12:00am", "2026-09-09T08:00:00Z");
        assert_absolute(&resolved, "2026-09-13T21:01:00Z");
    }

    #[test]
    fn t11_resets_same_weekday_already_passed_goes_next_week() {
        let resolved = resolve_msk("resets Tuesday 9am (UTC)", "2026-09-08T10:00:00Z");
        assert_absolute(&resolved, "2026-09-15T09:01:00Z");
    }

    #[test]
    fn t12_resets_weekday_across_dst_start() {
        let resolved = resolve_msk("resets Wed 9am (Europe/London)", "2026-03-28T12:00:00Z");
        assert_absolute(&resolved, "2026-04-01T08:01:00Z");
    }

    #[test]
    fn t13_resets_clock_in_dst_fold_takes_later() {
        let resolved = resolve_msk("resets 1:30am (Europe/London)", "2026-10-24T22:00:00Z");
        assert_absolute(&resolved, "2026-10-25T01:31:00Z");
    }

    #[test]
    fn t14_resets_clock_in_dst_gap_takes_later() {
        let resolved = resolve_msk("resets 1:30am (Europe/London)", "2026-03-28T22:00:00Z");
        assert_absolute(&resolved, "2026-03-29T01:31:00Z");
        let resolved = resolve_msk("resets 2:30am (Europe/London)", "2026-03-28T22:00:00Z");
        assert_absolute(&resolved, "2026-03-29T01:31:00Z");
    }

    #[test]
    fn t15_ambiguous_hour_takes_earliest_future() {
        let resolved = resolve_msk("resets 3", "2026-09-09T11:00:00Z");
        assert_absolute(&resolved, "2026-09-09T12:01:00Z");
    }

    #[test]
    fn t16_ambiguous_hour_both_past_goes_tomorrow() {
        let resolved = resolve_msk("resets 3", "2026-09-09T13:00:00Z");
        assert_absolute(&resolved, "2026-09-10T00:01:00Z");
    }

    #[test]
    fn t17_try_again_in_minutes() {
        let resolved = resolve_msk("try again in 45 minutes", "2026-09-09T08:00:00Z");
        assert_relative(&resolved, "2026-09-09T08:46:00Z");
    }

    #[test]
    fn t18_relative_anchors_on_banner_time() {
        let resolved = resolve_text(
            "try again in 45 minutes",
            "2026-09-09T08:30:00Z",
            Some("2026-09-09T08:00:00Z"),
            "Europe/Moscow",
        );
        assert_relative(&resolved, "2026-09-09T08:46:00Z");
    }

    #[test]
    fn t19_try_again_in_days_hours_minutes() {
        let resolved = resolve_msk(
            "Try again in 4 days 20 hours 9 minutes.",
            "2026-09-09T08:00:00Z",
        );
        assert_relative(&resolved, "2026-09-14T04:10:00Z");
    }

    #[test]
    fn t20_retry_banner_packed_duration() {
        let resolved = resolve_msk("[retrying in 2h5m attempt #4]", "2026-09-09T08:00:00Z");
        assert_relative(&resolved, "2026-09-09T10:06:00Z");
    }

    #[test]
    fn t21_resets_in_with_colon() {
        let resolved = resolve_msk("resets in: 3 hours", "2026-09-09T08:00:00Z");
        assert_relative(&resolved, "2026-09-09T11:01:00Z");
    }

    #[test]
    fn t22_try_again_in_zero_minutes() {
        let resolved = resolve_msk("try again in 0 minutes", "2026-09-09T08:00:00Z");
        assert_relative(&resolved, "2026-09-09T08:01:00Z");
    }

    #[test]
    fn t23_try_again_at_clock_host_bangkok() {
        let resolved = resolve_text(
            "try again at 3:51 PM.",
            "2026-09-09T08:00:00Z",
            None,
            "Asia/Bangkok",
        );
        assert_absolute(&resolved, "2026-09-09T08:52:00Z");
    }

    #[test]
    fn t24_try_again_at_full_date() {
        let resolved = resolve_text(
            "try again at Feb 23rd, 2026 9:01 PM.",
            "2026-02-23T10:00:00Z",
            None,
            "Asia/Bangkok",
        );
        assert_absolute(&resolved, "2026-02-23T14:02:00Z");
    }

    #[test]
    fn t25_try_again_at_full_date_in_the_past_falls_back() {
        let now = "2026-08-01T00:00:00Z";
        let resolved = resolve_msk("try again at Jul 28th, 2026 8:03 PM.", now);
        assert_fallback(&resolved, now);
    }

    #[test]
    fn t26_try_again_later_falls_back() {
        let now = "2026-09-09T08:00:00Z";
        let resolved = resolve_msk("Please try again later.", now);
        assert_fallback(&resolved, now);
    }

    #[test]
    fn t27_invalid_clock_falls_back() {
        let now = "2026-09-09T08:00:00Z";
        assert_eq!(parse_reset("resets 45:99"), None);
        let resolved = resolve_msk("resets 45:99", now);
        assert_fallback(&resolved, now);
    }

    #[test]
    fn t28_unknown_zone_falls_back() {
        let now = "2026-09-09T08:00:00Z";
        assert_eq!(parse_reset("resets 3pm (Mars/Olympus)"), None);
        let resolved = resolve_msk("resets 3pm (Mars/Olympus)", now);
        assert_fallback(&resolved, now);
    }

    #[test]
    fn t30_stale_five_hour_clamp() {
        let detected_at = ts("2026-09-09T08:00:00Z");
        let far = detected_at
            .checked_add(SignedDuration::from_mins(18 * 60 + 30))
            .unwrap();
        let clamped = stale_five_hour_clamp(far, detected_at, MARGIN);
        assert_eq!(clamped, detected_at.checked_add(MARGIN).unwrap());
        let near = detected_at
            .checked_add(SignedDuration::from_hours(3))
            .unwrap();
        assert_eq!(stale_five_hour_clamp(near, detected_at, MARGIN), near);
    }

    #[test]
    fn parse_reset_yields_expected_specs() {
        let spec = parse_reset("resets Mon 12:00am");
        assert_eq!(
            spec,
            Some(ResetSpec::Wall {
                date: WallDate::Weekday(Weekday::Monday),
                hour: 0,
                minute: 0,
                ambiguous: false,
                tz: None,
            })
        );
        let spec = parse_reset("resets Sunday");
        assert_eq!(
            spec,
            Some(ResetSpec::Wall {
                date: WallDate::Weekday(Weekday::Sunday),
                hour: 0,
                minute: 0,
                ambiguous: false,
                tz: None,
            })
        );
        let spec = parse_reset("resets 3");
        assert_eq!(
            spec,
            Some(ResetSpec::Wall {
                date: WallDate::Today,
                hour: 3,
                minute: 0,
                ambiguous: true,
                tz: None,
            })
        );
        let spec = parse_reset("resets Sep 1, 2026 at 12:00am (UTC)");
        assert_eq!(
            spec,
            Some(ResetSpec::Wall {
                date: WallDate::MonthDay {
                    month: 9,
                    day: 1,
                    year: Some(2026)
                },
                hour: 0,
                minute: 0,
                ambiguous: false,
                tz: Some(zone("UTC")),
            })
        );
        assert_eq!(parse_reset("resets 3 hours"), None);
        assert_eq!(
            parse_reset("wait ~3 min"),
            Some(ResetSpec::Relative(Duration::from_secs(180)))
        );
        assert_eq!(parse_reset("resets 13pm"), None);
    }

    #[test]
    fn parse_duration_table() {
        assert_eq!(
            parse_duration("2h5m"),
            Some(Duration::from_secs(2 * 3600 + 5 * 60))
        );
        assert_eq!(
            parse_duration("45 minutes"),
            Some(Duration::from_secs(45 * 60))
        );
        assert_eq!(
            parse_duration("4 days 20 hours 9 minutes"),
            Some(Duration::from_secs(4 * 86_400 + 20 * 3600 + 9 * 60))
        );
        assert_eq!(
            parse_duration("1h and 30m"),
            Some(Duration::from_secs(90 * 60))
        );
        assert_eq!(parse_duration("~3 min"), Some(Duration::from_secs(180)));
        assert_eq!(parse_duration("0 minutes"), Some(Duration::ZERO));
        assert_eq!(parse_duration("five minutes"), None);
        assert_eq!(parse_duration("3 parsecs"), None);
        assert_eq!(parse_duration(""), None);
    }

    struct XorShift64(u64);

    impl XorShift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    #[test]
    fn undated_clock_always_resolves_within_a_day() {
        let zones = [
            "Europe/Moscow",
            "Asia/Bangkok",
            "Europe/London",
            "America/New_York",
            "America/Los_Angeles",
            "Asia/Tokyo",
            "Australia/Sydney",
            "UTC",
            "America/Sao_Paulo",
            "Asia/Kolkata",
        ];
        let year_start = ts("2026-01-01T00:00:00Z");
        let year_seconds = 365 * 86_400;
        let host = zone("Europe/Moscow");
        let mut rng = XorShift64(0x9E37_79B9_7F4A_7C15);
        for _ in 0..2000 {
            let with_meridiem = rng.below(2) == 0;
            let (hour, ambiguous) = if with_meridiem {
                let hour_12 = rng.below(12) as u8 + 1;
                let is_pm = rng.below(2) == 0;
                let hour = match (hour_12, is_pm) {
                    (12, false) => 0,
                    (12, true) => 12,
                    (_, false) => hour_12,
                    (_, true) => hour_12 + 12,
                };
                (hour, false)
            } else {
                let hour = rng.below(24) as u8;
                (hour, (1..=12).contains(&hour))
            };
            let minute = rng.below(60) as u8;
            let tz = zone(zones[rng.below(zones.len() as u64) as usize]);
            let now = year_start
                .checked_add(SignedDuration::from_secs(rng.below(year_seconds) as i64))
                .unwrap();
            let spec = ResetSpec::Wall {
                date: WallDate::Today,
                hour,
                minute,
                ambiguous,
                tz: Some(tz),
            };
            let resolved = resolve(Some(&spec), now, None, &host, MARGIN, PROBE);
            let lower = now.checked_sub(SignedDuration::from_hours(1)).unwrap();
            let upper = now.checked_add(SignedDuration::from_hours(25)).unwrap();
            let upper = upper.checked_add(MARGIN).unwrap();
            assert_eq!(resolved.source, ResetSource::Absolute, "{spec:?} at {now}");
            assert!(resolved.at > lower, "{spec:?} at {now} -> {}", resolved.at);
            assert!(resolved.at <= upper, "{spec:?} at {now} -> {}", resolved.at);
        }
    }
}
