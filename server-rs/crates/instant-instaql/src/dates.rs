use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DateValue {
    NegativeInfinity,
    Finite(InstantValue),
    PositiveInfinity,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct InstantValue {
    seconds: i64,
    nanoseconds: u32,
}

impl InstantValue {
    fn from_millis(milliseconds: i64) -> Self {
        Self {
            seconds: milliseconds.div_euclid(1_000),
            nanoseconds: u32::try_from(milliseconds.rem_euclid(1_000))
                .expect("millisecond remainder is in 0..1000")
                * 1_000_000,
        }
    }

    fn from_datetime(value: DateTime<Utc>) -> Self {
        Self {
            seconds: value.timestamp(),
            nanoseconds: value.timestamp_subsec_nanos(),
        }
    }

    fn to_millis(self) -> Option<i64> {
        let milliseconds =
            i128::from(self.seconds) * 1_000 + i128::from(self.nanoseconds / 1_000_000);
        i64::try_from(milliseconds).ok()
    }
}

pub(crate) fn parse_date_value(value: &Value, now: DateTime<Utc>) -> Option<DateValue> {
    if let Some(milliseconds) = value.as_i64() {
        return Some(DateValue::Finite(InstantValue::from_millis(milliseconds)));
    }
    // Clojure accepts every `number?` here and Java interop coerces the argument
    // to Instant/ofEpochMilli(long), truncating a floating-point value toward
    // zero. Keep this branch limited to JSON floats: large unsigned integers
    // must not be rounded through f64 and accidentally accepted as another
    // millisecond value.
    if value.as_number().is_some_and(serde_json::Number::is_f64) {
        #[allow(clippy::cast_possible_truncation)] // This is the Java long-coercion contract.
        let milliseconds = value.as_f64()?.trunc() as i64;
        return Some(DateValue::Finite(InstantValue::from_millis(milliseconds)));
    }
    let value = value.as_str()?;
    let decoded = serde_json::from_str::<String>(value).ok();
    parse_date_string(decoded.as_deref().unwrap_or(value).trim(), now)
}

/// Parses every checked-date family accepted by the JVM and returns the exact
/// epoch-millisecond value used for attribute-sketch hashing. PostgreSQL's
/// positive/negative infinity sentinels use the same pgjdbc constants.
#[must_use]
pub fn parse_date_millis(value: &Value, now_millis: i64) -> Option<i64> {
    let now = Utc.timestamp_millis_opt(now_millis).single()?;
    match parse_date_value(value, now)? {
        DateValue::NegativeInfinity => Some(-9_223_372_036_832_400_000),
        DateValue::Finite(value) => value.to_millis(),
        DateValue::PositiveInfinity => Some(9_223_372_036_825_200_000),
    }
}

fn parse_date_string(value: &str, now: DateTime<Utc>) -> Option<DateValue> {
    match value {
        "epoch" => return Some(DateValue::Finite(InstantValue::from_millis(0))),
        "infinity" => return Some(DateValue::PositiveInfinity),
        "-infinity" => return Some(DateValue::NegativeInfinity),
        "now" => return Some(DateValue::Finite(InstantValue::from_datetime(now))),
        "today" => {
            return start_of_day(now)
                .map(InstantValue::from_datetime)
                .map(DateValue::Finite);
        }
        "tomorrow" => {
            return start_of_day(now + Duration::days(1))
                .map(InstantValue::from_datetime)
                .map(DateValue::Finite);
        }
        "yesterday" => {
            return start_of_day(now - Duration::days(1))
                .map(InstantValue::from_datetime)
                .map(DateValue::Finite);
        }
        _ => {}
    }

    let value = strip_valid_zone_region(value)?;

    DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_rfc2822(value))
        .map(|date| DateValue::Finite(InstantValue::from_datetime(date.with_timezone(&Utc))))
        .ok()
        .or_else(|| {
            parse_offset_date(value)
                .map(InstantValue::from_datetime)
                .map(DateValue::Finite)
        })
        .or_else(|| {
            parse_local_date_time(value)
                .map(InstantValue::from_datetime)
                .map(DateValue::Finite)
        })
        .or_else(|| {
            parse_local_date(value)
                .map(InstantValue::from_datetime)
                .map(DateValue::Finite)
        })
        .or_else(|| parse_extended_iso_instant(value).map(DateValue::Finite))
}

fn strip_valid_zone_region(value: &str) -> Option<&str> {
    if !value.contains(['[', ']']) {
        return Some(value);
    }
    let region_end = value.strip_suffix(']')?;
    let region_start = region_end.rfind('[')?;
    let (date_time, region) = region_end.split_at(region_start);
    let region = region.strip_prefix('[')?;
    if date_time.contains(['[', ']']) || region.is_empty() || region.contains(['[', ']']) {
        return None;
    }
    let (_, time) = date_time.split_once('T')?;
    if !time.ends_with('Z') && !time.contains(['+', '-']) {
        return None;
    }
    let fixed_region = ["UTC", "GMT", "UT"].iter().any(|prefix| {
        region
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.is_empty() || parse_iso_offset(suffix).is_some())
    });
    let valid = fixed_region || region.parse::<chrono_tz::Tz>().is_ok();
    valid.then_some(date_time)
}

fn parse_extended_iso_instant(value: &str) -> Option<InstantValue> {
    let (date, time_and_offset) = value.split_once('T')?;
    let (year_month, day) = date.rsplit_once('-')?;
    let (year, month) = year_month.rsplit_once('-')?;
    let year = year.parse::<i64>().ok()?;
    if !(-999_999_999..=999_999_999).contains(&year) {
        return None;
    }
    let month = month.parse::<i64>().ok()?;
    let day = day.parse::<i64>().ok()?;
    let days = unix_days_from_civil(year, month, day)?;

    let (time, offset_seconds) = if let Some(time) = time_and_offset.strip_suffix('Z') {
        (time, 0_i64)
    } else {
        let offset_start = time_and_offset.rfind(['+', '-'])?;
        let (time, offset) = time_and_offset.split_at(offset_start);
        (time, parse_iso_offset(offset)?)
    };
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let second_and_fraction = time_parts.next()?;
    if time_parts.next().is_some() || !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    let (second, nanoseconds) = match second_and_fraction.split_once('.') {
        Some((second, fraction)) => {
            if fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            let mut nanos = fraction.parse::<u32>().ok()?;
            for _ in fraction.len()..9 {
                nanos *= 10;
            }
            (second.parse::<i64>().ok()?, nanos)
        }
        None => (second_and_fraction.parse::<i64>().ok()?, 0),
    };
    if !(0..=59).contains(&second) {
        return None;
    }
    let seconds = i128::from(days) * 86_400 + i128::from(hour * 3_600 + minute * 60 + second)
        - i128::from(offset_seconds);
    Some(InstantValue {
        seconds: i64::try_from(seconds).ok()?,
        nanoseconds,
    })
}

fn parse_iso_offset(value: &str) -> Option<i64> {
    let sign = match value.as_bytes().first()? {
        b'+' => 1_i64,
        b'-' => -1_i64,
        _ => return None,
    };
    let body = &value[1..];
    if !body.is_ascii() {
        return None;
    }
    let (hour, minute, second) = if body.contains(':') {
        let mut parts = body.split(':');
        let hour = parts.next()?.parse::<i64>().ok()?;
        let minute = parts.next()?.parse::<i64>().ok()?;
        let second = parts
            .next()
            .map(str::parse::<i64>)
            .transpose()
            .ok()?
            .unwrap_or(0);
        if parts.next().is_some() {
            return None;
        }
        (hour, minute, second)
    } else {
        let (hour, minute, second) = match body.len() {
            2 => (&body[0..2], "00", "00"),
            4 => (&body[0..2], &body[2..4], "00"),
            6 => (&body[0..2], &body[2..4], &body[4..6]),
            _ => return None,
        };
        (
            hour.parse::<i64>().ok()?,
            minute.parse::<i64>().ok()?,
            second.parse::<i64>().ok()?,
        )
    };
    if !(0..=18).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }
    if hour == 18 && (minute != 0 || second != 0) {
        return None;
    }
    Some(sign * (hour * 3_600 + minute * 60 + second))
}

fn unix_days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    (civil_from_unix_days(days) == (year, month, day)).then_some(days)
}

/// Renders the finite instant accepted by Java's date parser.
/// Chrono preserves the existing query parameter spelling for ordinary dates;
/// the civil-date fallback covers the remainder of Java's full `long` input.
pub(crate) fn render_instant(value: InstantValue) -> String {
    if let Some(value) = DateTime::from_timestamp(value.seconds, value.nanoseconds) {
        return value.to_rfc3339();
    }

    let days = value.seconds.div_euclid(86_400);
    let day_seconds = value.seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_unix_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    let year = if (0..=9_999).contains(&year) {
        format!("{year:04}")
    } else if year > 9_999 {
        format!("+{year}")
    } else {
        format!("-{year:04}", year = year.unsigned_abs())
    };
    if value.nanoseconds == 0 {
        format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        let fraction = if value.nanoseconds.is_multiple_of(1_000_000) {
            format!("{:03}", value.nanoseconds / 1_000_000)
        } else if value.nanoseconds.is_multiple_of(1_000) {
            format!("{:06}", value.nanoseconds / 1_000)
        } else {
            format!("{:09}", value.nanoseconds)
        };
        format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction}Z")
    }
}

fn civil_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn start_of_day(value: DateTime<Utc>) -> Option<DateTime<Utc>> {
    value
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|value| value.and_utc())
}

fn parse_offset_date(value: &str) -> Option<DateTime<Utc>> {
    let expanded = expand_year(normalize_iso_components(expand_timezone(value)));
    let formats = [
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%-m-%-dT%-H:%-M:%-S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%-m-%-d %-H:%-M:%-S%.f%z",
        "%a %b %d %Y %H:%M:%S GMT%z",
    ];
    formats.iter().find_map(|format| {
        DateTime::parse_from_str(&expanded, format)
            .ok()
            .map(|date| date.with_timezone(&Utc))
    })
}

fn normalize_iso_components(value: String) -> String {
    let Some(separator) = value.find(['T', ' ']) else {
        return value;
    };
    let (date, remainder) = value.split_at(separator);
    let date_parts = date.split('-').collect::<Vec<_>>();
    if date_parts.len() != 3 {
        return value;
    }
    let time_end = remainder[1..]
        .find(['Z', '+', '-'])
        .map_or(remainder.len(), |position| position + 1);
    let (time, zone) = remainder[1..].split_at(time_end.saturating_sub(1));
    let time_parts = time.split(':').collect::<Vec<_>>();
    if time_parts.len() != 3 {
        return value;
    }
    let (seconds, fraction) = time_parts[2]
        .split_once('.')
        .map_or((time_parts[2], None), |(seconds, fraction)| {
            (seconds, Some(fraction))
        });
    format!(
        "{}-{:0>2}-{:0>2}{}{:0>2}:{:0>2}:{:0>2}{}{}",
        date_parts[0],
        date_parts[1],
        date_parts[2],
        &remainder[..1],
        time_parts[0],
        time_parts[1],
        seconds,
        fraction.map_or_else(String::new, |fraction| format!(".{fraction}")),
        zone
    )
}

fn expand_year(value: String) -> String {
    let year_length = value.bytes().take_while(u8::is_ascii_digit).count();
    if year_length > 4 {
        format!("+{value}")
    } else {
        value
    }
}

fn expand_timezone(value: &str) -> String {
    let mut value = value.to_owned();
    if value.ends_with('Z') {
        value.pop();
        value.push_str("+0000");
        return value;
    }
    for (abbreviation, offset) in [
        ("CETDST", "+0200"),
        ("EST", "-0500"),
        ("PDT", "-0700"),
        ("CET", "+0100"),
    ] {
        if value.ends_with(abbreviation) {
            value.truncate(value.len() - abbreviation.len());
            value.push_str(offset);
            return value;
        }
    }
    let Some(sign) = value.rfind(['+', '-']) else {
        return value;
    };
    let suffix = &value[sign..];
    if suffix.len() == 3 && suffix[1..].bytes().all(|byte| byte.is_ascii_digit()) {
        value.push_str("00");
    }
    value
}

fn parse_local_date_time(value: &str) -> Option<DateTime<Utc>> {
    let value = expand_year(normalize_iso_components(value.to_owned()));
    let formats = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%-m-%-d %-H:%-M:%-S%.f",
        "%Y-%m-%d %H:%M",
        "%m/%d/%Y, %I:%M:%S %p",
    ];
    formats.iter().find_map(|format| {
        NaiveDateTime::parse_from_str(&value, format)
            .ok()
            .map(|date| date.and_utc())
    })
}

fn parse_local_date(value: &str) -> Option<DateTime<Utc>> {
    let value = expand_year(value.to_owned());
    let formats = [
        "%Y-%m-%d",
        "%Y-%-m-%-d",
        "%a %b %d %Y",
        "%m-%d-%Y",
        "%-m/%-d/%Y",
    ];
    formats.iter().find_map(|format| {
        NaiveDate::parse_from_str(&value, format)
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|date| date.and_utc())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_reference_date_families() {
        let now = DateTime::parse_from_rfc3339("2025-08-11T18:42:30.157666Z")
            .unwrap()
            .to_utc();
        for value in [
            json!("Sat, 05 Apr 2025 18:00:31 GMT"),
            json!("2025-01-01T00:00:00Z"),
            json!("2025-01-02T00:00:00-08"),
            json!("\"2025-01-02T00:00:00-08\""),
            json!("2025-01-15 20:53:08.200"),
            json!("2025-01-15 20:53:08.892865"),
            json!("\"2025-01-15 20:53:08\""),
            json!("Wed Jul 09 2025"),
            json!("8/4/2025, 11:02:31 PM"),
            json!("2024-12-30 20:19:41.892865+00"),
            json!("epoch"),
            json!("infinity"),
            json!("-infinity"),
            json!("Mon Feb 24 2025 22:37:27 GMT+0000"),
            json!("\t2025-03-02T16:08:53Z"),
            json!("2024-05-29 01:51:06.11848+00"),
            json!("2025-03-01T16:08:53+0000"),
            json!("2025-12-31 21:11"),
            json!("04-17-2025"),
            json!("2025-06-12T10:56:31.924+0530"),
            json!("2025-08-7T00:00:00.000Z"),
            json!("72026-07-01"),
            json!("2025-06-05T17:00:00EST"),
            json!("2025-06-05T17:00:00PDT"),
            json!("2025-06-05T17:00:00CETDST"),
            json!("2025-06-05T17:00:00CET"),
            json!("3/12/4444"),
            json!("2025-9-29T23:59:59.999Z"),
            json!("2026-04-28T04:7:00.000Z"),
            json!("2026-04-28T4:07:00.000Z"),
            json!("2026-04-28T04:07:7.000Z"),
            json!(0),
        ] {
            assert!(parse_date_value(&value, now).is_some(), "{value}");
        }
    }

    #[test]
    fn sketch_millis_preserve_special_values_and_created_at_relative_dates() {
        let now = 1_755_023_550_157_i64;
        assert_eq!(parse_date_millis(&json!("now"), now), Some(now));
        assert_eq!(
            parse_date_millis(&json!("infinity"), now),
            Some(9_223_372_036_825_200_000)
        );
        assert_eq!(
            parse_date_millis(&json!("-infinity"), now),
            Some(-9_223_372_036_832_400_000)
        );
    }

    #[test]
    fn fractional_epoch_milliseconds_follow_java_long_coercion() {
        let now = DateTime::UNIX_EPOCH;
        assert_eq!(parse_date_millis(&json!(1.9), 0), Some(1));
        assert_eq!(parse_date_millis(&json!(-1.9), 0), Some(-1));
        assert_eq!(parse_date_millis(&json!(0.9), 0), Some(0));
        assert_eq!(
            parse_date_value(&json!(-1.9), now),
            Some(DateValue::Finite(InstantValue::from_millis(-1)))
        );
    }

    #[test]
    fn numeric_epoch_milliseconds_cover_the_full_java_instant_long_range() {
        assert_eq!(parse_date_millis(&json!(i64::MAX), 0), Some(i64::MAX));
        assert_eq!(parse_date_millis(&json!(i64::MIN), 0), Some(i64::MIN));
        assert_eq!(
            render_instant(InstantValue::from_millis(i64::MAX)),
            "+292278994-08-17T07:12:55.807Z"
        );
        assert_eq!(
            render_instant(InstantValue::from_millis(i64::MIN)),
            "-292275055-05-16T16:47:04.192Z"
        );
    }

    #[test]
    fn parsed_instants_retain_sub_millisecond_ordering_precision() {
        let now = DateTime::UNIX_EPOCH;
        let earlier = parse_date_value(&json!("2025-01-01T00:00:00.000001Z"), now).unwrap();
        let later = parse_date_value(&json!("2025-01-01T00:00:00.000002Z"), now).unwrap();
        assert!(earlier < later);
        assert_eq!(
            parse_date_millis(&json!("2025-01-01T00:00:00.000001Z"), 0),
            parse_date_millis(&json!("2025-01-01T00:00:00.000002Z"), 0)
        );
    }

    #[test]
    fn extended_iso_instants_cover_java_local_date_text_range() {
        let now = DateTime::UNIX_EPOCH;
        for (input, expected) in [
            (
                "+999999999-12-31T23:59:59.999999999Z",
                "+999999999-12-31T23:59:59.999999999Z",
            ),
            ("-999999999-01-01T00:00:00Z", "-999999999-01-01T00:00:00Z"),
            (
                "+292278994-08-17T08:12:55.807+01:00",
                "+292278994-08-17T07:12:55.807Z",
            ),
            (
                "+292278994-08-17T08:12:55.807+0100",
                "+292278994-08-17T07:12:55.807Z",
            ),
        ] {
            let DateValue::Finite(value) = parse_date_value(&json!(input), now).unwrap() else {
                panic!("{input}: expected a finite instant");
            };
            assert_eq!(render_instant(value), expected, "{input}");
        }
        for invalid in [
            "+1000000000-01-01T00:00:00Z",
            "+999999999-02-29T00:00:00Z",
            "+999999999-01-01T24:00:00Z",
            "+999999999-01-01T00:00:00+18:01",
        ] {
            assert!(
                parse_date_value(&json!(invalid), now).is_none(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn zoned_region_suffixes_keep_javas_explicit_offset_precedence() {
        let now = DateTime::UNIX_EPOCH;
        for (input, expected) in [
            (
                "2024-11-03T01:30:00-04:00[America/New_York]",
                "2024-11-03T05:30:00+00:00",
            ),
            (
                "2024-11-03T01:30:00-05:00[America/New_York]",
                "2024-11-03T06:30:00+00:00",
            ),
            (
                "2024-03-10T02:30:00-05:00[America/New_York]",
                "2024-03-10T07:30:00+00:00",
            ),
            (
                "+292278994-08-17T08:12:55.807+01:00[Europe/Paris]",
                "+292278994-08-17T07:12:55.807Z",
            ),
            (
                "2024-01-01T00:00:00+00:00[UTC+01:30]",
                "2024-01-01T00:00:00+00:00",
            ),
        ] {
            let DateValue::Finite(value) = parse_date_value(&json!(input), now).unwrap() else {
                panic!("{input}: expected a finite instant");
            };
            assert_eq!(render_instant(value), expected, "{input}");
        }
        for invalid in [
            "2024-01-01T00:00:00Z[Not/A_Zone]",
            "2024-01-01T00:00:00[Europe/Paris]",
            "2024-01-01T00:00:00Z[Europe/Paris",
            "2024-01-01T00:00:00Z[]",
            "2024-01-01T00:00:00Z[Z]",
            "2024-01-01T00:00:00Z[+01:00]",
        ] {
            assert!(
                parse_date_value(&json!(invalid), now).is_none(),
                "{invalid}"
            );
        }
    }
}
