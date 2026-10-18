//! Cron expressions of backup profiles (`schedule`), and whether one is due.
//!
//! Five fields, minute hour day-of-month month day-of-week, each `*`, a
//! number, a range `a-b`, a list `a,b` or a step `*/n` / `a-b/n`; days of the
//! week are 0–7 (0 and 7 are Sunday) or `mon`…`sun`. Also `@hourly`,
//! `@daily`, `@weekly` and `@monthly`. Times are the server's local time.

use std::fmt;
use std::str::FromStr;

use chrono::{Datelike, Duration, NaiveDateTime, Timelike};

/// How far back a missed run is still caught up.
const CATCH_UP_DAYS: i64 = 31;

/// `schedule: off` in an app's profile: the profile is kept, but not run on a schedule.
pub const OFF: &str = "off";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    /// Day of month and day of week both restricted: either matches (as cron does).
    either_day: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Bits `min..=max` set by one field.
fn field(text: &str, name: &str, min: u32, max: u32, names: &[&str]) -> Result<u64, ParseError> {
    let err = |detail: &str| ParseError(format!("{name} `{text}`: {detail}"));
    let value = |s: &str| -> Result<u32, ParseError> {
        let lower = s.to_ascii_lowercase();
        if let Some(index) = names.iter().position(|n| *n == lower) {
            return Ok(index as u32 + min);
        }
        let n: u32 = s.parse().map_err(|_| err("not a number"))?;
        if n < min || n > max {
            return Err(err(&format!("must be {min}–{max}")));
        }
        Ok(n)
    };
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| err("bad step"))?;
                if step == 0 {
                    return Err(err("step must be at least 1"));
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (from, to) = match range {
            "*" => (min, max),
            _ => match range.split_once('-') {
                Some((a, b)) => (value(a)?, value(b)?),
                None if step > 1 => (value(range)?, max),
                None => {
                    let n = value(range)?;
                    (n, n)
                }
            },
        };
        if from > to {
            return Err(err("range goes backwards"));
        }
        for n in (from..=to).step_by(step as usize) {
            bits |= 1 << n;
        }
    }
    Ok(bits)
}

impl FromStr for Schedule {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        let expanded = match text.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(ParseError(format!(
                "`{text}`: expected 5 fields (minute hour day month weekday), e.g. `0 3 * * *`"
            )));
        };
        let mut weekdays = field(weekday, "day of week", 0, 7, &WEEKDAYS)?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & !(1 << 7);
        }
        Ok(Self {
            minutes: field(minute, "minute", 0, 59, &[])?,
            hours: field(hour, "hour", 0, 23, &[])?,
            days: field(day, "day of month", 1, 31, &[])?,
            months: field(month, "month", 1, 12, &MONTHS)?,
            weekdays,
            either_day: day != "*" && weekday != "*",
        })
    }
}

impl Schedule {
    pub fn matches(&self, time: NaiveDateTime) -> bool {
        let bit = |mask: u64, n: u32| mask & (1 << n) != 0;
        let day = bit(self.days, time.day());
        let weekday = bit(self.weekdays, time.weekday().num_days_from_sunday());
        let day_ok = if self.either_day {
            day || weekday
        } else {
            day && weekday
        };
        bit(self.minutes, time.minute())
            && bit(self.hours, time.hour())
            && bit(self.months, time.month())
            && day_ok
    }

    /// The latest scheduled minute after `last` and at or before `now`, if
    /// any; missed runs older than a month are not caught up.
    pub fn due(&self, last: NaiveDateTime, now: NaiveDateTime) -> Option<NaiveDateTime> {
        let now = now.with_second(0)?.with_nanosecond(0)?;
        let oldest = last.max(now - Duration::days(CATCH_UP_DAYS));
        let mut time = now;
        while time > oldest {
            if self.matches(time) {
                return Some(time);
            }
            time -= Duration::minutes(1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    fn schedule(text: &str) -> Schedule {
        text.parse().unwrap()
    }

    #[test]
    fn parses_and_matches() {
        let daily = schedule("0 3 * * *");
        assert!(daily.matches(at("2026-10-18 03:00")));
        assert!(!daily.matches(at("2026-10-18 03:01")));
        // 2026-10-18 is a Sunday.
        let sunday = schedule("30 4 * * sun");
        assert!(sunday.matches(at("2026-10-18 04:30")));
        assert_eq!(schedule("30 4 * * 7"), sunday);
        assert_eq!(schedule("30 4 * * 0"), sunday);
        assert!(!sunday.matches(at("2026-10-19 04:30")));
        let workdays = schedule("*/15 8-18 * * mon-fri");
        assert!(workdays.matches(at("2026-10-19 08:45")));
        assert!(!workdays.matches(at("2026-10-19 08:50")));
        assert!(!workdays.matches(at("2026-10-18 08:45")));
        // Day of month or day of week, as in cron.
        let either = schedule("0 0 1 * mon");
        assert!(either.matches(at("2026-10-01 00:00")));
        assert!(either.matches(at("2026-10-19 00:00")));
        assert!(!either.matches(at("2026-10-20 00:00")));
        assert_eq!(schedule("@daily"), schedule("0 0 * * *"));
        let quarter = schedule("0 0 1,15 jan-mar *");
        assert!(quarter.matches(at("2026-02-15 00:00")));
        assert!(!quarter.matches(at("2026-04-15 00:00")));
    }

    #[test]
    fn rejects_bad_expressions() {
        for (text, message) in [
            ("0 3 * *", "expected 5 fields"),
            ("60 3 * * *", "minute `60`: must be 0–59"),
            ("0 3 * * fun", "day of week `fun`: not a number"),
            ("*/0 * * * *", "step must be at least 1"),
            ("0 5-3 * * *", "range goes backwards"),
        ] {
            let err = text.parse::<Schedule>().unwrap_err();
            assert!(err.to_string().contains(message), "{text}: {err}");
        }
    }

    #[test]
    fn due_catches_up_once() {
        let daily = schedule("0 3 * * *");
        let last = at("2026-10-17 03:00");
        assert_eq!(daily.due(last, at("2026-10-18 02:59")), None);
        assert_eq!(
            daily.due(last, at("2026-10-18 03:00")),
            Some(at("2026-10-18 03:00"))
        );
        // The server was off for three days: one run, for the latest time.
        assert_eq!(
            daily.due(last, at("2026-10-20 09:12")),
            Some(at("2026-10-20 03:00"))
        );
        // Not again for the same time.
        assert_eq!(
            daily.due(at("2026-10-20 03:00"), at("2026-10-20 09:17")),
            None
        );
        // Runs missed long ago are not caught up.
        let yearly = schedule("0 0 1 1 *");
        assert_eq!(
            yearly.due(at("2025-01-01 00:00"), at("2026-10-18 12:00")),
            None
        );
    }
}
