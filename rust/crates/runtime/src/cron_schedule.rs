//! Self-contained 5-field cron expression matching for the cron scheduler.
//!
//! Supports the standard `minute hour day-of-month month day-of-week` fields
//! with `*`, single values, `*/step`, ranges `a-b`, `a-b/step`, and
//! comma-separated lists. Day-of-week is 0-6 (Sunday=0); 7 is also accepted as
//! Sunday. Time is evaluated in UTC, decomposed from a Unix timestamp without
//! pulling in a date library so the runtime stays dependency-light and the
//! logic stays deterministic and unit-testable.

/// A parsed 5-field cron schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: FieldMatch,
    hour: FieldMatch,
    day_of_month: FieldMatch,
    month: FieldMatch,
    day_of_week: FieldMatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldMatch {
    /// `*` — matches any value in the field's range.
    Any,
    /// An explicit set of allowed values.
    Set(Vec<u32>),
}

impl FieldMatch {
    fn matches(&self, value: u32) -> bool {
        match self {
            FieldMatch::Any => true,
            FieldMatch::Set(values) => values.contains(&value),
        }
    }
}

/// Calendar fields decomposed from a Unix timestamp (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilTime {
    pub minute: u32,
    pub hour: u32,
    pub day_of_month: u32,
    pub month: u32,
    pub day_of_week: u32,
}

impl CronSchedule {
    /// Parse a standard 5-field cron expression. Returns `None` if the
    /// expression does not have exactly five fields or any field is invalid.
    #[must_use]
    pub fn parse(expression: &str) -> Option<Self> {
        let fields = expression.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 {
            return None;
        }
        Some(Self {
            minute: parse_field(fields[0], 0, 59)?,
            hour: parse_field(fields[1], 0, 23)?,
            day_of_month: parse_field(fields[2], 1, 31)?,
            month: parse_field(fields[3], 1, 12)?,
            day_of_week: parse_dow_field(fields[4])?,
        })
    }

    /// Returns `true` when the schedule fires at the given civil time. Matches
    /// standard cron semantics: when both day-of-month and day-of-week are
    /// restricted (neither is `*`), the schedule fires if EITHER matches.
    #[must_use]
    pub fn matches(&self, time: CivilTime) -> bool {
        let dom_restricted = !matches!(self.day_of_month, FieldMatch::Any);
        let dow_restricted = !matches!(self.day_of_week, FieldMatch::Any);
        let day_matches = if dom_restricted && dow_restricted {
            self.day_of_month.matches(time.day_of_month)
                || self.day_of_week.matches(time.day_of_week)
        } else {
            self.day_of_month.matches(time.day_of_month)
                && self.day_of_week.matches(time.day_of_week)
        };
        self.minute.matches(time.minute)
            && self.hour.matches(time.hour)
            && self.month.matches(time.month)
            && day_matches
    }
}

fn parse_field(spec: &str, min: u32, max: u32) -> Option<FieldMatch> {
    if spec == "*" {
        return Some(FieldMatch::Any);
    }
    let mut values = Vec::new();
    for part in spec.split(',') {
        collect_part(part, min, max, &mut values)?;
    }
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.dedup();
    Some(FieldMatch::Set(values))
}

/// Day-of-week field: accept 0-7 with both 0 and 7 meaning Sunday, normalizing
/// 7 to 0 so it matches the civil-time convention (Sunday = 0).
fn parse_dow_field(spec: &str) -> Option<FieldMatch> {
    match parse_field(spec, 0, 7)? {
        FieldMatch::Any => Some(FieldMatch::Any),
        FieldMatch::Set(values) => {
            let mut normalized = values
                .into_iter()
                .map(|value| if value == 7 { 0 } else { value })
                .collect::<Vec<_>>();
            normalized.sort_unstable();
            normalized.dedup();
            Some(FieldMatch::Set(normalized))
        }
    }
}

fn collect_part(part: &str, min: u32, max: u32, out: &mut Vec<u32>) -> Option<()> {
    // Split an optional `/step` suffix.
    let (range_spec, step) = match part.split_once('/') {
        Some((range, step_str)) => (range, step_str.parse::<u32>().ok().filter(|s| *s > 0)?),
        None => (part, 1),
    };

    let (start, end) = if range_spec == "*" {
        (min, max)
    } else if let Some((lo, hi)) = range_spec.split_once('-') {
        (lo.parse::<u32>().ok()?, hi.parse::<u32>().ok()?)
    } else {
        let value = range_spec.parse::<u32>().ok()?;
        // A bare number with a step (e.g. `5/10`) means "from 5 to max step 10".
        if step > 1 {
            (value, max)
        } else {
            (value, value)
        }
    };

    if start < min || end > max || start > end {
        return None;
    }
    let mut value = start;
    while value <= end {
        out.push(value);
        value += step;
    }
    Some(())
}

/// Decompose a Unix timestamp (seconds, UTC) into calendar fields using the
/// civil-from-days algorithm (Howard Hinnant), avoiding a date dependency.
#[must_use]
pub fn civil_time_from_unix_secs(secs: u64) -> CivilTime {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let minute = ((secs_of_day % 3_600) / 60) as u32;
    let hour = (secs_of_day / 3_600) as u32;
    // 1970-01-01 was a Thursday (day_of_week = 4, Sunday = 0).
    let day_of_week = (((days % 7) + 4 + 7) % 7) as u32;

    // Civil-from-days: convert days-since-epoch to (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let _year = y + i64::from(month <= 2);

    CivilTime {
        minute,
        hour,
        day_of_month: day,
        month,
        day_of_week,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_field_count() {
        assert!(CronSchedule::parse("* * * *").is_none());
        assert!(CronSchedule::parse("* * * * * *").is_none());
        assert!(CronSchedule::parse("").is_none());
    }

    #[test]
    fn every_minute_matches_any_time() {
        let schedule = CronSchedule::parse("* * * * *").expect("valid");
        let time = CivilTime {
            minute: 37,
            hour: 9,
            day_of_month: 15,
            month: 6,
            day_of_week: 3,
        };
        assert!(schedule.matches(time));
    }

    #[test]
    fn specific_minute_and_hour() {
        let schedule = CronSchedule::parse("30 14 * * *").expect("valid");
        let base = CivilTime {
            minute: 30,
            hour: 14,
            day_of_month: 1,
            month: 1,
            day_of_week: 0,
        };
        assert!(schedule.matches(base));
        assert!(!schedule.matches(CivilTime { minute: 31, ..base }));
        assert!(!schedule.matches(CivilTime { hour: 13, ..base }));
    }

    #[test]
    fn step_and_range_and_list() {
        let schedule = CronSchedule::parse("*/15 9-17 * * 1,3,5").expect("valid");
        let weekday_9 = CivilTime {
            minute: 0,
            hour: 9,
            day_of_month: 10,
            month: 4,
            day_of_week: 1,
        };
        assert!(schedule.matches(weekday_9));
        assert!(schedule.matches(CivilTime {
            minute: 45,
            ..weekday_9
        }));
        assert!(!schedule.matches(CivilTime {
            minute: 7,
            ..weekday_9
        }));
        assert!(!schedule.matches(CivilTime {
            hour: 18,
            ..weekday_9
        }));
        // day_of_week = 2 (Tuesday) is not in {1,3,5}
        assert!(!schedule.matches(CivilTime {
            day_of_week: 2,
            ..weekday_9
        }));
    }

    #[test]
    fn dom_or_dow_when_both_restricted() {
        // Fires on the 1st OR on Mondays.
        let schedule = CronSchedule::parse("0 0 1 * 1").expect("valid");
        let on_first = CivilTime {
            minute: 0,
            hour: 0,
            day_of_month: 1,
            month: 5,
            day_of_week: 4,
        };
        let on_monday = CivilTime {
            minute: 0,
            hour: 0,
            day_of_month: 9,
            month: 5,
            day_of_week: 1,
        };
        let neither = CivilTime {
            minute: 0,
            hour: 0,
            day_of_month: 9,
            month: 5,
            day_of_week: 4,
        };
        assert!(schedule.matches(on_first));
        assert!(schedule.matches(on_monday));
        assert!(!schedule.matches(neither));
    }

    #[test]
    fn sunday_accepts_zero_and_seven() {
        let zero = CronSchedule::parse("0 0 * * 0").expect("valid");
        let seven = CronSchedule::parse("0 0 * * 7").expect("valid");
        let sunday = CivilTime {
            minute: 0,
            hour: 0,
            day_of_month: 4,
            month: 1,
            day_of_week: 0,
        };
        assert!(zero.matches(sunday));
        assert!(seven.matches(sunday));
    }

    #[test]
    fn civil_time_decodes_known_timestamps() {
        // 2021-01-01 00:00:00 UTC = 1609459200, a Friday (day_of_week = 5).
        let t = civil_time_from_unix_secs(1_609_459_200);
        assert_eq!(t.year_fields(), (1, 1));
        assert_eq!(t.hour, 0);
        assert_eq!(t.minute, 0);
        assert_eq!(t.day_of_week, 5);

        // 2026-06-03 14:30:00 UTC = 1780497000, a Wednesday (day_of_week = 3).
        let t2 = civil_time_from_unix_secs(1_780_497_000);
        assert_eq!(t2.hour, 14);
        assert_eq!(t2.minute, 30);
        assert_eq!(t2.month, 6);
        assert_eq!(t2.day_of_month, 3);
        assert_eq!(t2.day_of_week, 3);
    }

    impl CivilTime {
        fn year_fields(self) -> (u32, u32) {
            (self.month, self.day_of_month)
        }
    }
}
