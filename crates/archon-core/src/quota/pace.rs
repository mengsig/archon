//! Quota pace: how much of a usage window is spent against how much of it
//! has passed, and when it runs out at that rate (docs/quota-pacing.md).

use serde::{Deserialize, Serialize};

use super::{ProviderHeadroom, QuotaWindow};
use crate::Timestamp;

/// Points by which the used share may run ahead of the elapsed share before
/// a window counts as short (`[models] pacing_margin_percent`).
pub const DEFAULT_MARGIN_PERCENT: u8 = 10;

/// One window's pace, as of the reading it was computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WindowPace {
    pub window: String,
    pub window_minutes: u64,
    /// Percent of the window's quota spent, 0 to 100.
    pub used_percent: u8,
    /// Percent of the window's time passed, 0 to 100.
    pub elapsed_percent: u8,
    pub resets_at: Timestamp,
    /// When it runs out at its rate so far; `None` while nothing is spent.
    #[serde(default)]
    pub empty_at: Option<Timestamp>,
    /// Used runs ahead of elapsed by more than the margin, so at this rate
    /// the window runs out before it resets.
    pub short: bool,
}

/// A usage observation older than this says nothing about pace, however
/// recently Archon received it.
pub const MAX_OBSERVATION_AGE_SECONDS: i64 = 600;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// `window`'s pace as aetherd reported it at `read_at`; `None` when it names
/// no length or absolute reset, or has already reset. Time counts in
/// nanoseconds, so no rounding moves a window across the margin.
pub fn window_pace(window: &QuotaWindow, read_at: Timestamp, margin: u8) -> Option<WindowPace> {
    assert!(margin <= 100, "a margin is a percentage");
    let minutes = window.window_minutes.filter(|m| *m > 0)?;
    let resets_at = window.resets_at?;
    if resets_at <= read_at {
        return None;
    }
    let length = i128::from(minutes) * 60 * NANOS_PER_SECOND;
    let until = i128::from((resets_at - read_at).num_nanoseconds()?);
    debug_assert!(until > 0 && length > 0);
    let elapsed = (length - until).clamp(0, length);
    let used = 100 - window.remaining_percent.min(100);
    // Exact: (used − margin) / 100 > elapsed / length, cross-multiplied.
    let short = (i128::from(used) - i128::from(margin)) * length > 100 * elapsed;
    let elapsed_percent = (elapsed as f64 * 100.0 / length as f64).round() as u8;
    let pace = WindowPace {
        window: window.name.clone(),
        window_minutes: minutes,
        used_percent: u8::try_from(used).expect("used is at most 100"),
        elapsed_percent,
        resets_at,
        empty_at: empty_at(used, elapsed, read_at),
        short,
    };
    debug_assert!(pace.used_percent <= 100 && pace.elapsed_percent <= 100);
    debug_assert!(pace.empty_at.is_none_or(|at| at >= read_at));
    Some(pace)
}

/// When the rest runs out if it keeps going at the rate `used` percent took
/// over `elapsed` nanoseconds; `None` with no rate to go by.
fn empty_at(used: u64, elapsed: i128, read_at: Timestamp) -> Option<Timestamp> {
    assert!(used <= 100 && elapsed >= 0);
    if used == 0 || elapsed == 0 {
        return None;
    }
    let nanos = i128::from(100 - used) * elapsed / i128::from(used);
    debug_assert!(nanos >= 0);
    let nanos = nanos.min(i128::from(i32::MAX) * NANOS_PER_SECOND);
    read_at.checked_add_signed(chrono::TimeDelta::nanoseconds(i64::try_from(nanos).ok()?))
}

/// How old `reading`'s usage observation is at `now`: the older of its
/// exact timestamp and its age when aetherd answered at `read_at`.
fn observation_age(
    reading: &ProviderHeadroom,
    read_at: Timestamp,
    now: Timestamp,
) -> Option<chrono::TimeDelta> {
    assert!(now >= read_at, "a reading is never from the future");
    let by_timestamp = reading.observed_at.map(|at| now - at);
    let by_age = (reading.age_seconds)
        .and_then(|age| i64::try_from(age).ok())
        .map(chrono::TimeDelta::seconds)
        .map(|age| {
            age.checked_add(&(now - read_at))
                .unwrap_or(chrono::TimeDelta::MAX)
        });
    debug_assert!(by_age.is_none_or(|age| age >= chrono::TimeDelta::zero()));
    by_timestamp.max(by_age)
}

/// The pace of each of `reading`'s windows still ahead of `now`; none for a
/// stale reading or one observed over [`MAX_OBSERVATION_AGE_SECONDS`] ago.
pub fn provider_pace(
    reading: &ProviderHeadroom,
    read_at: Timestamp,
    now: Timestamp,
    margin: u8,
) -> Vec<WindowPace> {
    assert!(now >= read_at, "a reading is never from the future");
    let max_age = chrono::TimeDelta::seconds(MAX_OBSERVATION_AGE_SECONDS);
    let too_old = observation_age(reading, read_at, now).is_some_and(|age| age > max_age);
    if reading.stale || too_old {
        return Vec::new();
    }
    let paces: Vec<WindowPace> = (reading.windows.iter())
        .filter_map(|window| window_pace(window, read_at, margin))
        .filter(|pace| pace.resets_at > now)
        .collect();
    debug_assert!(paces.len() <= reading.windows.len());
    paces
}

impl WindowPace {
    /// `7d used 70% at 30% elapsed, empty ~2026-11-21 04:00 UTC before its
    /// reset 2026-11-24 12:00 UTC`, as `status` and `queue_status` show it.
    pub fn describe(&self) -> String {
        assert!(self.used_percent <= 100 && self.elapsed_percent <= 100);
        let minute = |at: Timestamp| at.format("%Y-%m-%d %H:%M UTC").to_string();
        let empty = self.empty_at.map_or("no rate yet".to_owned(), |at| {
            format!("empty ~{}", minute(at))
        });
        let reset = match self.short {
            true => "before its reset",
            false => "resets",
        };
        format!(
            "{} used {}% at {}% elapsed, {empty}, {reset} {}",
            self.window,
            self.used_percent,
            self.elapsed_percent,
            minute(self.resets_at)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        chrono::DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    /// A 100-minute window read at `read_at`, `remaining` left, resetting in
    /// `resets_in` minutes.
    fn window(remaining: u64, resets_in: i64, read_at: Timestamp) -> QuotaWindow {
        QuotaWindow {
            name: "w".into(),
            window_minutes: Some(100),
            remaining_percent: remaining,
            resets_at: Some(read_at + chrono::TimeDelta::minutes(resets_in)),
            resets_in_seconds: None,
        }
    }

    #[test]
    fn short_only_once_used_runs_past_elapsed_by_more_than_the_margin() {
        let read = at("2026-11-19T12:00:00Z");
        // 40% elapsed: used 50% is exactly at the margin, 51% past it.
        let at_margin = window_pace(&window(50, 60, read), read, 10).unwrap();
        assert_eq!(
            (at_margin.used_percent, at_margin.elapsed_percent),
            (50, 40)
        );
        assert!(!at_margin.short, "{at_margin:?}");
        let past = window_pace(&window(49, 60, read), read, 10).unwrap();
        assert!(past.short, "{past:?}");
        // 51 used over 40 minutes runs the other 49 out in ~38.4 minutes.
        let empty = past.empty_at.unwrap();
        assert!(empty < past.resets_at, "{past:?}");
        assert_eq!((empty - read).num_minutes(), 38);
        // Exact boundaries that binary floating point once rounded past.
        for (remaining, resets_in) in [(70, 80), (58, 68)] {
            let edge = window_pace(&window(remaining, resets_in, read), read, 10).unwrap();
            assert_eq!(edge.used_percent - edge.elapsed_percent, 10, "{edge:?}");
            assert!(!edge.short, "exactly at the margin is not short: {edge:?}");
        }
        let even = window_pace(&window(60, 60, read), read, 0).unwrap();
        assert!(!even.short, "40% used at 40% elapsed is on pace");
        let zero_margin = window_pace(&window(59, 60, read), read, 0).unwrap();
        assert!(
            zero_margin.short,
            "41% used at 40% elapsed passes a 0 margin"
        );
    }

    #[test]
    fn a_fraction_of_a_second_never_rounds_across_the_margin_or_the_reset() {
        let read = at("2026-11-19T12:00:00Z");
        let half = chrono::TimeDelta::milliseconds(500);
        // 30% used at 19m59.5s of 100 minutes is just past a 10-point margin.
        let mut just_past = window(70, 80, read);
        just_past.resets_at = Some(read + chrono::TimeDelta::minutes(80) + half);
        assert!(window_pace(&just_past, read, 10).unwrap().short);
        let at_margin = window_pace(&window(70, 80, read), read, 10).unwrap();
        assert!(!at_margin.short, "{at_margin:?}");
        // Half a second before its reset a window still has a pace.
        let mut ending = window(10, 0, read);
        ending.resets_at = Some(read + half);
        let pace = window_pace(&ending, read, 10).expect("not reset yet");
        assert_eq!(pace.elapsed_percent, 100);
    }

    #[test]
    fn a_fresh_or_full_window_has_no_rate_and_a_reset_one_no_pace() {
        let read = at("2026-11-19T12:00:00Z");
        let unused = window_pace(&window(100, 60, read), read, 10).unwrap();
        assert_eq!(
            (unused.used_percent, unused.empty_at, unused.short),
            (0, None, false)
        );
        // Just opened yet already 15% spent: no rate, but it is short.
        let opened = window_pace(&window(85, 100, read), read, 10).unwrap();
        assert_eq!((opened.elapsed_percent, opened.empty_at), (0, None));
        assert!(opened.short);
        let spent = window_pace(&window(0, 30, read), read, 10).unwrap();
        assert_eq!(spent.empty_at, Some(read));
        assert!(window_pace(&window(10, 0, read), read, 10).is_none());
        assert!(window_pace(&window(10, -5, read), read, 10).is_none());
    }

    #[test]
    fn stale_or_partial_readings_never_pace() {
        let read = at("2026-11-19T12:00:00Z");
        let mut headroom = ProviderHeadroom {
            windows: vec![window(5, 60, read)],
            observed_at: None,
            age_seconds: None,
            stale: true,
        };
        assert!(provider_pace(&headroom, read, read, 10).is_empty());
        headroom.stale = false;
        assert_eq!(provider_pace(&headroom, read, read, 10).len(), 1);
        let later = read + chrono::TimeDelta::minutes(61);
        assert!(
            provider_pace(&headroom, read, later, 10).is_empty(),
            "it reset since"
        );
        // Aether 1.4.1 may leave out a window's length or its reset.
        let partial: ProviderHeadroom = serde_json::from_value(serde_json::json!({
            "windows": [
                {"name": "5h", "remaining_percent": 1},
                {"name": "7d", "remaining_percent": 1, "window_minutes": 10080},
                {"name": "1d", "remaining_percent": 1, "resets_in_seconds": 60},
                {"name": "2d", "remaining_percent": 1, "window_minutes": 2880,
                 "resets_in_seconds": 60}
            ],
            "stale": false
        }))
        .unwrap();
        assert!(provider_pace(&partial, read, read, 10).is_empty());
        // A fresh reply may carry an old observation: its age counts, not the reply's.
        headroom.age_seconds = Some(540);
        let soon = read + chrono::TimeDelta::seconds(60);
        assert_eq!(
            provider_pace(&headroom, read, soon, 10).len(),
            1,
            "ten minutes old"
        );
        let past = soon + chrono::TimeDelta::milliseconds(1);
        assert!(provider_pace(&headroom, read, past, 10).is_empty());
        // An exact timestamp counts to the nanosecond beside a rounded age.
        headroom.observed_at = Some(read - chrono::TimeDelta::minutes(10));
        assert_eq!(provider_pace(&headroom, read, read, 10).len(), 1);
        let just_past = read + chrono::TimeDelta::milliseconds(250);
        assert!(provider_pace(&headroom, read, just_past, 10).is_empty());
        headroom.age_seconds = None;
        headroom.observed_at = Some(read - chrono::TimeDelta::minutes(11));
        assert!(provider_pace(&headroom, read, read, 10).is_empty());
        let bare: ProviderHeadroom = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(
            provider_pace(&bare, read, read, 10).is_empty(),
            "unknown is stale"
        );
    }
}
