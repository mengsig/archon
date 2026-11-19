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

/// `window`'s pace as aetherd reported it at `read_at`; `None` when it names
/// no length or reset, or has already reset.
pub fn window_pace(window: &QuotaWindow, read_at: Timestamp, margin: u8) -> Option<WindowPace> {
    assert!(margin <= 100, "a margin is a percentage");
    let minutes = window.window_minutes.filter(|m| *m > 0)?;
    let resets_at = window.reset_at(read_at)?;
    let length = i64::try_from(minutes.checked_mul(60)?).ok()?;
    let until = (resets_at - read_at).num_seconds();
    if until <= 0 {
        return None;
    }
    let elapsed_seconds = (length - until).clamp(0, length);
    let elapsed = elapsed_seconds as f64 / length as f64;
    let remaining = window.remaining_percent.min(100) as f64 / 100.0;
    let used = 1.0 - remaining;
    let short = used * 100.0 > elapsed * 100.0 + f64::from(margin);
    let empty_at = empty_at(used, remaining, elapsed_seconds, read_at);
    let pace = WindowPace {
        window: window.name.clone(),
        window_minutes: minutes,
        used_percent: (used * 100.0).round() as u8,
        elapsed_percent: (elapsed * 100.0).round() as u8,
        resets_at,
        empty_at,
        short,
    };
    debug_assert!(pace.used_percent <= 100 && pace.elapsed_percent <= 100);
    debug_assert!(pace.empty_at.is_none_or(|at| at >= read_at));
    Some(pace)
}

/// When `remaining` runs out if it keeps going at the rate `used` took over
/// `elapsed_seconds`; `None` with no rate to go by.
fn empty_at(
    used: f64,
    remaining: f64,
    elapsed_seconds: i64,
    read_at: Timestamp,
) -> Option<Timestamp> {
    assert!((0.0..=1.0).contains(&used) && (0.0..=1.0).contains(&remaining));
    if used <= 0.0 || elapsed_seconds <= 0 {
        return None;
    }
    let seconds = remaining * elapsed_seconds as f64 / used;
    debug_assert!(seconds.is_finite() && seconds >= 0.0);
    let seconds = chrono::TimeDelta::try_seconds(seconds.min(i32::MAX as f64) as i64)?;
    read_at.checked_add_signed(seconds)
}

/// The pace of each of `reading`'s windows still ahead of `now`; none for a
/// stale reading, which says nothing about quota.
pub fn provider_pace(
    reading: &ProviderHeadroom,
    read_at: Timestamp,
    now: Timestamp,
    margin: u8,
) -> Vec<WindowPace> {
    assert!(now >= read_at, "a reading is never from the future");
    if reading.stale {
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
    /// `7d used 70% at 30% elapsed, runs out ~2026-11-21 04:00 UTC`, as
    /// `status` and `queue_status` show it.
    pub fn describe(&self) -> String {
        assert!(self.used_percent <= 100 && self.elapsed_percent <= 100);
        let mut line = format!(
            "{} used {}% at {}% elapsed",
            self.window, self.used_percent, self.elapsed_percent
        );
        if self.short {
            let empty = self.empty_at.map_or("soon".to_owned(), |at| {
                format!("~{}", at.format("%Y-%m-%d %H:%M UTC"))
            });
            line.push_str(&format!(
                ", runs out {empty}, before its reset {}",
                self.resets_at.format("%Y-%m-%d %H:%M UTC")
            ));
        }
        line
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
        let zero_margin = window_pace(&window(59, 60, read), read, 0).unwrap();
        assert!(
            zero_margin.short,
            "41% used at 40% elapsed passes a 0 margin"
        );
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
                {"name": "1d", "remaining_percent": 1, "resets_in_seconds": 60}
            ],
            "stale": false
        }))
        .unwrap();
        assert!(provider_pace(&partial, read, read, 10).is_empty());
        let bare: ProviderHeadroom = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(
            provider_pace(&bare, read, read, 10).is_empty(),
            "unknown is stale"
        );
    }
}
