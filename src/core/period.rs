//! Evaluation windows for the report views: `--since` / `--until` / `--last`
//! resolved into UTC epoch bounds. A pure transform — the clock and the
//! timezone are injected so tests stay deterministic (`.claude/rules/tdd.md`),
//! dates are interpreted in the presentation zone (JST by convention, like
//! `bucket.rs`) while the bounds they produce are UTC epochs
//! (`docs/specs/cli.md`).

use chrono::{FixedOffset, NaiveDate, TimeZone};

/// A resolved evaluation window in UTC epoch seconds. Both ends are
/// inclusive; `None` is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeRange {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl TimeRange {
    /// Every event — the flags-absent default, which must not change a
    /// query's text or a view's output by a byte.
    pub const ALL: Self = Self {
        start: None,
        end: None,
    };

    /// Whether the flags drew no window at all.
    pub fn is_all(self) -> bool {
        self.start.is_none() && self.end.is_none()
    }
}

/// Resolve the period flags into a UTC epoch window.
///
/// Dates (`--since` / `--until`, `YYYY-MM-DD`) are interpreted in
/// `tz_offset_secs` — the presentation zone, JST by convention — and `--until`
/// counts the whole day it names, so it resolves to that day's last second.
/// `--last <N>d | <N>h | <N>w` is a rolling window ending at the injected
/// `now`. `--since` after `--until` is an error rather than an empty report,
/// which would read as "nothing was used".
pub fn resolve(
    since: Option<&str>,
    until: Option<&str>,
    last: Option<&str>,
    now: i64,
    tz_offset_secs: i32,
) -> Result<TimeRange, String> {
    if last.is_some() && (since.is_some() || until.is_some()) {
        return Err(
            "--last is a rolling window on its own and cannot be combined with --since or --until"
                .to_string(),
        );
    }
    if let Some(value) = last {
        let secs = span_secs(value)?;
        return Ok(TimeRange {
            start: Some(now - secs),
            end: Some(now),
        });
    }
    let start = since
        .map(|value| day_start_epoch(value, "--since", tz_offset_secs))
        .transpose()?;
    let end = until
        .map(|value| day_end_epoch(value, "--until", tz_offset_secs))
        .transpose()?;
    if let (Some(start), Some(end)) = (start, end)
        && start > end
    {
        let since = since.unwrap_or_default();
        let until = until.unwrap_or_default();
        return Err(format!(
            "empty window: --since {since} is after --until {until}"
        ));
    }
    Ok(TimeRange { start, end })
}

/// The whole day `--until` names, inclusive: the day's last local second.
fn day_end_epoch(value: &str, flag: &str, tz_offset_secs: i32) -> Result<i64, String> {
    Ok(day_start_epoch(value, flag, tz_offset_secs)? + 86_400 - 1)
}

/// Local midnight of the `YYYY-MM-DD` date, in `tz_offset_secs`.
fn day_start_epoch(value: &str, flag: &str, tz_offset_secs: i32) -> Result<i64, String> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        format!("invalid {flag} '{value}' — expected a YYYY-MM-DD date (e.g. 2026-10-01)")
    })?;
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight always exists");
    let tz = FixedOffset::east_opt(tz_offset_secs).expect("valid tz offset");
    let local = tz
        .from_local_datetime(&midnight)
        .single()
        .ok_or_else(|| format!("invalid {flag} '{value}' — not a local time in this zone"))?;
    Ok(local.timestamp())
}

/// The seconds in a `<N>d | <N>h | <N>w` rolling window.
fn span_secs(value: &str) -> Result<i64, String> {
    let invalid = || {
        format!(
            "invalid --last '{value}' — expected <N>d | <N>h | <N>w with a positive N (e.g. 7d, 24h, 2w)"
        )
    };
    let mut chars = value.chars();
    let unit = chars.next_back().ok_or_else(invalid)?;
    let n: i64 = chars.as_str().parse().map_err(|_| invalid())?;
    if n <= 0 {
        return Err(invalid());
    }
    let secs = match unit {
        'd' => 86_400,
        'h' => 3_600,
        'w' => 7 * 86_400,
        _ => return Err(invalid()),
    };
    n.checked_mul(secs).ok_or_else(invalid)
}

/// The window's resolved instants as RFC 3339 strings in the given zone:
/// `(since, until)` — the bounds that exist only.
pub fn bounds(range: TimeRange, tz_offset_secs: i32) -> (Option<String>, Option<String>) {
    let tz = FixedOffset::east_opt(tz_offset_secs).expect("valid tz offset");
    let instant = |epoch: i64| {
        tz.timestamp_opt(epoch, 0)
            .single()
            .map_or_else(String::new, |t| {
                t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
            })
    };
    (range.start.map(instant), range.end.map(instant))
}

/// The window as one human-readable line, instants rendered in
/// `tz_offset_secs` as RFC 3339 with their offset — e.g.
/// `since 2026-10-01T00:00:00+09:00, until 2026-10-07T23:59:59+09:00`.
pub fn display(range: TimeRange, tz_offset_secs: i32) -> String {
    let (since, until) = bounds(range, tz_offset_secs);
    let mut parts = Vec::new();
    if let Some(since) = since {
        parts.push(format!("since {since}"));
    }
    if let Some(until) = until {
        parts.push(format!("until {until}"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bucket::JST_OFFSET_SECS;

    const NOW: i64 = 1_000_000;

    fn resolve(
        since: Option<&str>,
        until: Option<&str>,
        last: Option<&str>,
    ) -> Result<TimeRange, String> {
        super::resolve(since, until, last, NOW, JST_OFFSET_SECS)
    }

    // 2026-10-01T00:00:00Z — JST 2026-10-01 09:00.
    const OCT_1_UTC: i64 = 1_790_812_800;
    // 2026-10-08T00:00:00+09:00 == 2026-10-07T15:00:00Z.
    const OCT_8_JST_MIDNIGHT: i64 = OCT_1_UTC + 6 * 86_400 + 15 * 3_600;

    #[test]
    fn since_resolves_to_the_jst_day_start() {
        // 2026-10-01 00:00 JST is 2026-09-30T15:00:00Z.
        let range = resolve(Some("2026-10-01"), None, None).unwrap();
        assert_eq!(range.start, Some(OCT_1_UTC - 9 * 3600));
        assert_eq!(range.end, None);
    }

    #[test]
    fn until_counts_the_whole_day_it_names() {
        // Inclusive to the last second of 2026-10-07 JST.
        let range = resolve(None, Some("2026-10-07"), None).unwrap();
        assert_eq!(range.start, None);
        assert_eq!(range.end, Some(OCT_8_JST_MIDNIGHT - 1));
    }

    #[test]
    fn an_event_at_utc_21h_belongs_to_the_next_jst_day() {
        // 2026-10-07T21:00:00Z is JST 2026-10-08 06:00 — outside --until
        // 2026-10-07 even though the UTC date is the 7th.
        let range = resolve(None, Some("2026-10-07"), None).unwrap();
        let event = OCT_1_UTC + 6 * 86_400 + 21 * 3600;
        assert!(
            event > range.end.unwrap(),
            "the edge event must be excluded"
        );
    }

    #[test]
    fn since_and_until_bound_a_week() {
        let range = resolve(Some("2026-10-01"), Some("2026-10-07"), None).unwrap();
        assert_eq!(range.start, Some(OCT_1_UTC - 9 * 3600));
        assert_eq!(range.end, Some(OCT_8_JST_MIDNIGHT - 1));
    }

    #[test]
    fn last_rolls_a_window_back_from_the_injected_now() {
        assert_eq!(
            resolve(None, None, Some("7d")).unwrap(),
            TimeRange {
                start: Some(NOW - 7 * 86_400),
                end: Some(NOW)
            }
        );
        assert_eq!(
            resolve(None, None, Some("24h")).unwrap(),
            TimeRange {
                start: Some(NOW - 86_400),
                end: Some(NOW)
            }
        );
        assert_eq!(
            resolve(None, None, Some("2w")).unwrap(),
            TimeRange {
                start: Some(NOW - 14 * 86_400),
                end: Some(NOW)
            }
        );
    }

    #[test]
    fn the_month_end_resolves_without_rolling_over() {
        let range = resolve(Some("2026-10-31"), None, None).unwrap();
        assert_eq!(range.start, Some(OCT_1_UTC + 30 * 86_400 - 9 * 3600));
        let range = resolve(None, Some("2026-10-31"), None).unwrap();
        // 2026-11-01T00:00+09:00 minus one second.
        assert_eq!(range.end, Some(OCT_1_UTC + 31 * 86_400 - 9 * 3600 - 1));
    }

    #[test]
    fn no_flags_mean_every_event() {
        assert_eq!(resolve(None, None, None).unwrap(), TimeRange::ALL);
        assert!(TimeRange::ALL.is_all());
        assert!(!resolve(None, None, Some("1d")).unwrap().is_all());
    }

    #[test]
    fn a_date_spelled_with_slashes_is_an_error() {
        let err = resolve(Some("2026/10/01"), None, None).unwrap_err();
        assert!(err.contains("--since"), "{err}");
        assert!(err.contains("2026/10/01"), "{err}");
        let err = resolve(None, Some("2026.10.01"), None).unwrap_err();
        assert!(err.contains("--until"), "{err}");
    }

    #[test]
    fn a_span_without_a_unit_is_an_error() {
        assert!(resolve(None, None, Some("7")).is_err());
        assert!(resolve(None, None, Some("d")).is_err());
        assert!(resolve(None, None, Some("")).is_err());
        assert!(resolve(None, None, Some("1y")).is_err());
    }

    #[test]
    fn a_non_positive_span_is_an_error() {
        assert!(resolve(None, None, Some("0d")).is_err());
        assert!(resolve(None, None, Some("-3d")).is_err());
    }

    #[test]
    fn last_cannot_be_combined_with_the_dates() {
        let err = resolve(Some("2026-10-01"), None, Some("7d")).unwrap_err();
        assert!(err.contains("--last"), "{err}");
        let err = resolve(None, Some("2026-10-01"), Some("7d")).unwrap_err();
        assert!(err.contains("--last"), "{err}");
    }

    #[test]
    fn since_after_until_is_an_empty_window_error() {
        let err = resolve(Some("2026-10-07"), Some("2026-10-01"), None).unwrap_err();
        assert!(err.contains("2026-10-07"), "{err}");
        assert!(err.contains("2026-10-01"), "{err}");
    }

    #[test]
    fn display_renders_the_resolved_instants_with_their_offset() {
        let range = resolve(Some("2026-10-01"), Some("2026-10-07"), None).unwrap();
        let text = display(range, JST_OFFSET_SECS);
        assert!(
            text.contains("since 2026-10-01T00:00:00+09:00"),
            "the JST presentation of the day start: {text}"
        );
        assert!(text.contains("until 2026-10-07T23:59:59+09:00"), "{text}");
    }
}
