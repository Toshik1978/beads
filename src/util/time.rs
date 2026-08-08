//! Time and date parsing utilities.

use crate::error::{BeadsError, Result};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use std::num::IntErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelativeTimeError {
    InvalidUnit,
    OutOfRange,
}

/// Parse a flexible time specification into a `DateTime<Utc>`.
///
/// Supports:
/// - RFC3339: `2025-01-15T12:00:00Z`, `2025-01-15T12:00:00+00:00`
/// - Simple date: `2025-01-15` (defaults to 9:00 AM local time)
/// - Relative duration: `+1h`, `+2d`, `+1w`, `+30m`
/// - Keywords: `tomorrow`, `next-week`
///
/// # Errors
///
/// Returns an error if:
/// - The time format is invalid or unrecognized
/// - A relative duration has an invalid unit (only m, h, d, w supported)
/// - The local time is ambiguous (e.g., during DST transitions)
///
/// # Panics
///
/// This function does not panic. The internal `unwrap()` calls on `from_hms_opt(9, 0, 0)`
/// are safe because 9:00:00 is always a valid time.
pub fn parse_flexible_timestamp(s: &str, field_name: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();

    // Try RFC3339 first
    if let Some(dt) = parse_rfc3339_timestamp(s) {
        return Ok(dt);
    }

    // Try simple date (YYYY-MM-DD) - default to 9:00 AM local time
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let naive_dt = date.and_time(time);
        return local_to_utc(&naive_dt, field_name);
    }

    match parse_relative_timestamp(s) {
        Ok(Some(dt)) => return Ok(dt),
        Ok(None) => {}
        Err(RelativeTimeError::InvalidUnit) => {
            return Err(BeadsError::validation(
                field_name,
                "invalid unit (use m, h, d, w)",
            ));
        }
        Err(RelativeTimeError::OutOfRange) => {
            return Err(BeadsError::validation(
                field_name,
                "relative duration is out of supported range",
            ));
        }
    }

    // Try keywords
    let now = Local::now();
    match s.to_lowercase().as_str() {
        "today" => {
            let time = NaiveTime::from_hms_opt(17, 0, 0).unwrap();
            let naive_dt = now.date_naive().and_time(time);
            Ok(local_to_utc(&naive_dt, field_name)?)
        }
        "yesterday" => {
            let yesterday = now.date_naive() - Duration::days(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = yesterday.and_time(time);
            Ok(local_to_utc(&naive_dt, field_name)?)
        }
        "tomorrow" => {
            let tomorrow = now.date_naive() + Duration::days(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = tomorrow.and_time(time);
            Ok(local_to_utc(&naive_dt, field_name)?)
        }
        "next-week" | "nextweek" => {
            let next_week = now.date_naive() + Duration::weeks(1);
            let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            let naive_dt = next_week.and_time(time);
            Ok(local_to_utc(&naive_dt, field_name)?)
        }
        _ => Err(BeadsError::validation(
            field_name,
            "invalid time format (try: +1h, -7d, tomorrow, next-week, or 2025-01-15)",
        )),
    }
}

/// Which end of a date range a bound is, for [`parse_range_bound`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeBound {
    /// A `--*-after` bound: the earliest instant that still matches.
    Lower,
    /// A `--*-before` bound: the latest instant that still matches.
    Upper,
}

/// Parse a date-range bound for `br list` / `br search` (bds-lf1).
///
/// Everything [`parse_flexible_timestamp`] accepts is accepted here and means
/// the same instant — RFC 3339, relative offsets like `-7d`, and the
/// `today`/`yesterday`/`tomorrow`/`next-week` keywords. The one deliberate
/// difference is the bare `YYYY-MM-DD` form.
///
/// `parse_flexible_timestamp` reads a bare date as **09:00 local**, which is the
/// right guess for `--due 2026-03-01` (a deadline is a moment in a working day)
/// and the wrong one for a range. `--created-after 2026-03-01 --created-before
/// 2026-03-01` would be a range of zero width at 09:00 and would match almost
/// nothing, when the only thing a reader could mean by it is "created on the
/// 1st". So a bare date widens to the day it names: the start of it for a lower
/// bound, the last instant of it for an upper one.
///
/// Both ends are **inclusive**, which is what the pre-existing
/// `ListFilters::updated_before` already did (`updated_at <= ?`) and what
/// `br stale` depends on. A timestamp given explicitly is used exactly as
/// given — the widening applies only to the form that names a day rather than
/// an instant.
///
/// # Errors
///
/// Same as [`parse_flexible_timestamp`]: an unrecognised format, a bad relative
/// unit, or a local time that cannot be resolved.
pub fn parse_range_bound(s: &str, field_name: &str, bound: RangeBound) -> Result<DateTime<Utc>> {
    let trimmed = s.trim();
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let time = match bound {
            RangeBound::Lower => NaiveTime::MIN,
            // 23:59:59.999999999 rather than the next midnight, so that an
            // inclusive `<=` cannot pick up the first instant of the next day.
            RangeBound::Upper => {
                NaiveTime::from_hms_nano_opt(23, 59, 59, 999_999_999).unwrap_or(NaiveTime::MIN)
            }
        };
        return local_to_utc(&date.and_time(time), field_name);
    }
    parse_flexible_timestamp(trimmed, field_name)
}

fn parse_rfc3339_timestamp(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }

    let normalized = strip_zero_offset_seconds(s)?;
    DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn strip_zero_offset_seconds(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let sign_pos = bytes.len().checked_sub(9)?;
    if !matches!(bytes.get(sign_pos), Some(b'+' | b'-'))
        || bytes.get(sign_pos + 3) != Some(&b':')
        || bytes.get(sign_pos + 6) != Some(&b':')
    {
        return None;
    }

    let offset_digits = [
        bytes.get(sign_pos + 1)?,
        bytes.get(sign_pos + 2)?,
        bytes.get(sign_pos + 4)?,
        bytes.get(sign_pos + 5)?,
        bytes.get(sign_pos + 7)?,
        bytes.get(sign_pos + 8)?,
    ];
    if !offset_digits.iter().all(|byte| byte.is_ascii_digit())
        || bytes.get(sign_pos + 7..sign_pos + 9) != Some(b"00")
    {
        return None;
    }

    Some(s[..bytes.len() - 3].to_string())
}

fn parse_relative_timestamp(
    s: &str,
) -> std::result::Result<Option<DateTime<Utc>>, RelativeTimeError> {
    let Some(rest) = s.strip_prefix(['+', '-'].as_ref()) else {
        return Ok(None);
    };
    let Some(unit_char) = rest.chars().last() else {
        return Ok(None);
    };

    let amount_end = s.len() - unit_char.len_utf8();
    let amount_str = &s[..amount_end];
    let amount = match amount_str.parse::<i64>() {
        Ok(amount) => amount,
        Err(err)
            if matches!(
                err.kind(),
                IntErrorKind::PosOverflow | IntErrorKind::NegOverflow
            ) =>
        {
            return Err(RelativeTimeError::OutOfRange);
        }
        Err(_) => return Ok(None),
    };

    let duration = match unit_char {
        'm' => Duration::try_minutes(amount),
        'h' => Duration::try_hours(amount),
        'd' => Duration::try_days(amount),
        'w' => Duration::try_weeks(amount),
        _ => return Err(RelativeTimeError::InvalidUnit),
    }
    .ok_or(RelativeTimeError::OutOfRange)?;

    Utc::now()
        .checked_add_signed(duration)
        .ok_or(RelativeTimeError::OutOfRange)
        .map(Some)
}

/// Format a duration as a human-readable relative time string (e.g., "2 days ago").
#[must_use]
pub fn format_relative_time(dt: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let duration = if dt > now {
        dt.signed_duration_since(now)
    } else {
        now.signed_duration_since(dt)
    };

    let suffix = if dt > now { "from now" } else { "ago" };

    let seconds = duration.num_seconds();
    if seconds < 60 {
        return "just now".to_string();
    }

    let minutes = duration.num_minutes();
    if minutes < 60 {
        return format!(
            "{} minute{} {}",
            minutes,
            if minutes == 1 { "" } else { "s" },
            suffix
        );
    }

    let hours = duration.num_hours();
    if hours < 24 {
        return format!(
            "{} hour{} {}",
            hours,
            if hours == 1 { "" } else { "s" },
            suffix
        );
    }

    let days = duration.num_days();
    if days < 30 {
        return format!(
            "{} day{} {}",
            days,
            if days == 1 { "" } else { "s" },
            suffix
        );
    }

    if days < 365 {
        #[allow(clippy::cast_possible_truncation)]
        let months = (days as f64 / 30.44).round() as i64;
        let months = months.max(1);
        if months >= 12 {
            return format!("1 year {suffix}");
        }
        return format!(
            "{} month{} {}",
            months,
            if months == 1 { "" } else { "s" },
            suffix
        );
    }

    let years = days / 365;
    let years = years.max(1);
    format!(
        "{} year{} {}",
        years,
        if years == 1 { "" } else { "s" },
        suffix
    )
}

/// Convert a stored UTC timestamp into the caller's local timezone for display.
///
/// Timestamps are always stored and serialized as UTC; this is the single
/// conversion point for human-facing output, mirroring the local-time
/// interpretation that [`parse_flexible_timestamp`] applies to user input.
#[must_use]
pub fn to_local(dt: DateTime<Utc>) -> DateTime<Local> {
    dt.with_timezone(&Local)
}

fn local_to_utc(naive_dt: &chrono::NaiveDateTime, field_name: &str) -> Result<DateTime<Utc>> {
    use chrono::LocalResult;
    match Local.from_local_datetime(naive_dt) {
        LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => Ok(dt.with_timezone(&Utc)),
        LocalResult::None => {
            // Time doesn't exist (DST gap), push forward by 1 hour
            let shifted = *naive_dt + Duration::hours(1);
            match Local.from_local_datetime(&shifted) {
                LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => {
                    Ok(dt.with_timezone(&Utc))
                }
                LocalResult::None => Err(BeadsError::validation(
                    field_name,
                    "invalid local time around DST transition",
                )),
            }
        }
    }
}
#[cfg(test)]
mod tests {

    /// `parse_relative_timestamp` is the live relative-duration parser, reached
    /// from `parse_flexible_timestamp`. These tests used to drive a
    /// `parse_relative_time` wrapper that flattened its two failure modes into
    /// `None` and additionally accepted `today`/`tomorrow`/`next-week`, which no
    /// command ever offered.
    fn parse_relative_time(s: &str) -> Option<DateTime<Utc>> {
        parse_relative_timestamp(s).ok().flatten()
    }
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn test_parse_flexible_rfc3339() {
        let result = parse_flexible_timestamp("2025-01-15T12:00:00Z", "test").unwrap();
        assert_eq!(result.year(), 2025);
    }

    /// bds-lf1: a bare date names a *day*, and a range bound has to widen to it.
    ///
    /// The failure this prevents is quiet: with `parse_flexible_timestamp`'s
    /// 09:00 reading, `--created-after 2026-03-01 --created-before 2026-03-01`
    /// is a zero-width range at one instant and matches nothing, which looks
    /// like "no issues were created that day" rather than like a bug.
    #[test]
    fn a_bare_date_range_bound_widens_to_the_whole_day() {
        let lower = parse_range_bound("2026-03-01", "created_after", RangeBound::Lower).unwrap();
        let upper = parse_range_bound("2026-03-01", "created_before", RangeBound::Upper).unwrap();

        assert!(lower < upper, "lower={lower}, upper={upper}");
        assert_eq!(
            (upper - lower).num_seconds(),
            24 * 60 * 60 - 1,
            "the pair has to span the named day: lower={lower}, upper={upper}"
        );

        // Interpreted in the local zone, as every other bare date in this module
        // is, and the endpoints land on the day named rather than beside it.
        let local_lower = to_local(lower).naive_local();
        let local_upper = to_local(upper).naive_local();
        assert_eq!(local_lower.date(), local_upper.date());
        assert_eq!(local_lower.time(), NaiveTime::MIN);
        assert_eq!(local_upper.time().second(), 59);
    }

    /// Anything that names an instant is taken at that instant, both ends alike.
    /// The widening is a property of the bare-date *form*, not of the bound.
    #[test]
    fn an_explicit_timestamp_range_bound_is_not_widened() {
        let lower =
            parse_range_bound("2026-03-01T12:00:00Z", "created_after", RangeBound::Lower).unwrap();
        let upper =
            parse_range_bound("2026-03-01T12:00:00Z", "created_before", RangeBound::Upper).unwrap();
        assert_eq!(lower, upper);
        assert_eq!(
            lower,
            parse_flexible_timestamp("2026-03-01T12:00:00Z", "test").unwrap()
        );
    }

    /// The relative and keyword forms are shared with `--due` / `--defer`
    /// wholesale; a range bound must not quietly grow its own dialect.
    #[test]
    fn relative_and_keyword_range_bounds_match_the_flexible_parser() {
        for spelling in ["-7d", "+2w", "yesterday", "tomorrow", "next-week"] {
            let bound = parse_range_bound(spelling, "updated_after", RangeBound::Lower)
                .unwrap_or_else(|error| panic!("{spelling} should parse: {error}"));
            let flexible = parse_flexible_timestamp(spelling, "test").unwrap();
            // Relative forms are anchored on `Utc::now()` at call time, so allow
            // the microseconds between the two calls.
            assert!(
                (bound - flexible).num_seconds().abs() <= 1,
                "{spelling}: bound={bound}, flexible={flexible}"
            );
        }

        assert!(
            parse_range_bound("last-fortnight", "updated_after", RangeBound::Lower).is_err(),
            "an unrecognised spelling has to be rejected, not guessed at"
        );
    }

    #[test]
    fn test_parse_flexible_rfc3339_zero_offset_spellings() {
        let z = parse_flexible_timestamp("2025-01-15T12:00:00Z", "test").unwrap();
        let short_offset = parse_flexible_timestamp("2025-01-15T12:00:00+00:00", "test").unwrap();
        let long_offset = parse_flexible_timestamp("2025-01-15T12:00:00+00:00:00", "test").unwrap();

        assert_eq!(short_offset, z);
        assert_eq!(long_offset, z);
    }

    #[test]
    fn test_parse_flexible_rfc3339_preserves_pre_epoch_nanoseconds() {
        let result = parse_flexible_timestamp("1969-12-31T23:59:59.123456789Z", "test").unwrap();

        assert_eq!(result.timestamp(), -1);
        assert_eq!(result.timestamp_subsec_nanos(), 123_456_789);
    }

    #[test]
    fn test_parse_flexible_rfc3339_rejects_nonzero_offset_seconds() {
        let err = parse_flexible_timestamp("2025-01-15T12:00:00+00:00:01", "test")
            .expect_err("nonzero offset seconds are not supported");

        assert!(err.to_string().contains("invalid time format"));
    }

    #[test]
    fn test_parse_flexible_simple_date() {
        let result = parse_flexible_timestamp("2025-06-20", "test").unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 20);
    }

    #[test]
    fn test_parse_flexible_relative() {
        let result = parse_flexible_timestamp("+1h", "test").unwrap();
        assert!(result > Utc::now());
    }

    #[test]
    fn test_parse_flexible_relative_negative() {
        let result = parse_flexible_timestamp("-1d", "test").unwrap();
        assert!(result < Utc::now());
    }

    #[test]
    fn test_parse_flexible_relative_does_not_silently_clamp_large_valid_offsets() {
        let before = Utc::now();
        let result = parse_flexible_timestamp("+600000h", "test").unwrap();
        let after = Utc::now();

        assert!(result >= before + Duration::hours(600_000));
        assert!(result <= after + Duration::hours(600_000));
    }

    #[test]
    fn test_parse_flexible_relative_rejects_out_of_range_offsets() {
        let err = parse_flexible_timestamp("+9999999999999999999d", "test")
            .expect_err("overflowing relative duration should be rejected");

        assert!(
            err.to_string()
                .contains("relative duration is out of supported range"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_flexible_keywords() {
        let result = parse_flexible_timestamp("tomorrow", "test").unwrap();
        assert!(result > Utc::now());
    }

    #[test]
    fn test_parse_relative_time_positive() {
        let result = parse_relative_time("+1h").unwrap();
        assert!(result > Utc::now());
    }

    #[test]
    fn test_parse_relative_time_negative() {
        let result = parse_relative_time("-7d").unwrap();
        assert!(result < Utc::now());
    }

    #[test]
    fn test_parse_relative_time_does_not_silently_clamp_large_valid_offsets() {
        let before = Utc::now();
        let result = parse_relative_time("+600000m").unwrap();
        let after = Utc::now();

        assert!(result >= before + Duration::minutes(600_000));
        assert!(result <= after + Duration::minutes(600_000));
    }

    #[test]
    fn test_parse_relative_time_rejects_out_of_range_offsets() {
        assert!(parse_relative_time("+9999999999999999999d").is_none());
    }

    #[test]
    fn test_parse_relative_time_invalid() {
        assert!(parse_relative_time("invalid").is_none());
        assert!(parse_relative_time("2025-01-15").is_none());
    }

    #[test]
    fn test_format_relative_time_normalizes_twelve_months_to_year() {
        let now = Utc::now();
        let dt = now - Duration::days(364);
        assert_eq!(format_relative_time(dt, now), "1 year ago");
    }

    #[test]
    fn test_format_relative_time_keeps_midrange_months() {
        let now = Utc::now();
        let dt = now - Duration::days(330);
        assert_eq!(format_relative_time(dt, now), "11 months ago");
    }
}
