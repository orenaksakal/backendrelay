//! UTC cron expressions for per-app backups, independent of application execution.
use std::collections::HashSet;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use instant_http::{ApiError, ApiErrorType};

#[derive(Clone)]
pub(crate) struct Cron {
    minute: HashSet<u32>,
    hour: HashSet<u32>,
    day: HashSet<u32>,
    month: HashSet<u32>,
    weekday: HashSet<u32>,
    day_wildcard: bool,
    weekday_wildcard: bool,
}

impl Cron {
    pub(crate) fn parse(value: &str) -> Result<Self, ApiError> {
        let fields: Vec<_> = value.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(validation("cron must have five fields"));
        }
        let (day, day_wildcard) = cron_field(fields[2], 1, 31)?;
        let (weekday, weekday_wildcard) = cron_field(fields[4], 0, 6)?;
        Ok(Self {
            minute: cron_field(fields[0], 0, 59)?.0,
            hour: cron_field(fields[1], 0, 23)?.0,
            day,
            month: cron_field(fields[3], 1, 12)?.0,
            weekday,
            day_wildcard,
            weekday_wildcard,
        })
    }

    fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, ApiError> {
        let outside_range = || validation("cron occurrence is outside the supported date range");
        let mut candidate = after
            .checked_add_signed(Duration::minutes(1))
            .and_then(|value| value.with_second(0))
            .and_then(|value| value.with_nanosecond(0))
            .ok_or_else(outside_range)?;
        for _ in 0..(400 * 24 * 60) {
            let day_matches = self.day.contains(&candidate.day());
            let weekday_matches = self
                .weekday
                .contains(&candidate.weekday().num_days_from_sunday());
            let calendar_matches = if self.day_wildcard || self.weekday_wildcard {
                day_matches && weekday_matches
            } else {
                day_matches || weekday_matches
            };
            if self.minute.contains(&candidate.minute())
                && self.hour.contains(&candidate.hour())
                && self.month.contains(&candidate.month())
                && calendar_matches
            {
                return Ok(candidate);
            }
            candidate = candidate
                .checked_add_signed(Duration::minutes(1))
                .ok_or_else(outside_range)?;
        }
        Err(validation("cron has no occurrence within 400 days"))
    }
}

pub(crate) async fn next_cron(
    schedule: Cron,
    after: DateTime<Utc>,
) -> Result<DateTime<Utc>, ApiError> {
    tokio::task::spawn_blocking(move || schedule.next_after(after))
        .await
        .map_err(|_| ApiError::new(ApiErrorType::Unknown, "backup schedule calculation failed"))?
}

fn cron_field(field: &str, min: u32, max: u32) -> Result<(HashSet<u32>, bool), ApiError> {
    let mut values = HashSet::new();
    for part in field.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((base, step)) => (base, step.parse::<u32>().map_err(|_| malformed())?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(malformed());
        }
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            (
                a.parse().map_err(|_| malformed())?,
                b.parse().map_err(|_| malformed())?,
            )
        } else {
            let value = base.parse().map_err(|_| malformed())?;
            (value, value)
        };
        if start < min || end > max || start > end {
            return Err(malformed());
        }
        for value in (start..=end).step_by(usize::try_from(step).map_err(|_| malformed())?) {
            values.insert(value);
        }
    }
    if values.is_empty() {
        Err(malformed())
    } else {
        Ok((values, field == "*"))
    }
}

fn validation(message: &str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn malformed() -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        "invalid backup cron expression",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    #[test]
    fn daily_backups_use_utc_and_strictly_advance() {
        let schedule = Cron::parse("0 9 * * *").unwrap();
        assert_eq!(
            schedule.next_after(utc("2026-01-01T08:30:42Z")).unwrap(),
            utc("2026-01-01T09:00:00Z")
        );
        assert_eq!(
            schedule.next_after(utc("2026-01-01T09:00:00Z")).unwrap(),
            utc("2026-01-02T09:00:00Z")
        );
    }

    #[test]
    fn ranges_lists_and_steps_are_preserved() {
        let schedule = Cron::parse("*/15 1-3 * 1,2 1-5").unwrap();
        assert_eq!(
            schedule.next_after(utc("2026-01-02T01:01:00Z")).unwrap(),
            utc("2026-01-02T01:15:00Z")
        );
    }

    #[test]
    fn restricted_day_and_weekday_use_cron_or_semantics() {
        let schedule = Cron::parse("0 0 15 * 1").unwrap();
        assert_eq!(
            schedule.next_after(utc("2026-01-11T00:00:00Z")).unwrap(),
            utc("2026-01-12T00:00:00Z")
        );
    }

    #[test]
    fn invalid_expressions_are_rejected() {
        for value in [
            "",
            "0 0 * *",
            "60 * * * *",
            "*/0 * * * *",
            "0 0 0 * *",
            "0 0 * 13 *",
            "0 0 * * 7",
            "0 0 * * 4-1",
        ] {
            assert!(Cron::parse(value).is_err(), "{value}");
        }
        assert!(
            Cron::parse("0 0 31 2 *")
                .unwrap()
                .next_after(utc("2026-01-01T00:00:00Z"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn calculation_runs_off_the_async_worker() {
        assert_eq!(
            next_cron(
                Cron::parse("0 0 * * *").unwrap(),
                utc("2026-01-01T00:00:00Z")
            )
            .await
            .unwrap(),
            utc("2026-01-02T00:00:00Z")
        );
    }
}
