//! Live quota headroom for tier-only routing: the paced headroom of each
//! provider's tightest usage window scales its routing weight.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::Timestamp;

/// The most a provider's weight is multiplied by: credit about to reset is
/// plentiful, but never worth more than twice its share.
pub const MAX_HEADROOM: f64 = 2.0;

/// One usage window as aetherd reports it (its protocol's `WindowHeadroom`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub name: String,
    #[serde(default)]
    pub window_minutes: Option<u64>,
    pub remaining_percent: u64,
    #[serde(default)]
    pub resets_at: Option<Timestamp>,
    /// Counted from when aetherd answered, which `resets_at` is not.
    #[serde(default)]
    pub resets_in_seconds: Option<u64>,
}

impl QuotaWindow {
    /// When it resets, by its reset time, else by the countdown aetherd gave
    /// at `read_at`; `None` when it named neither.
    pub fn reset_at(&self, read_at: Timestamp) -> Option<Timestamp> {
        if self.resets_at.is_some() {
            return self.resets_at;
        }
        let countdown = i64::try_from(self.resets_in_seconds?).ok()?;
        read_at.checked_add_signed(chrono::TimeDelta::try_seconds(countdown)?)
    }
}

/// A provider's latest usage report (aetherd's `ProviderHeadroom`). A stale
/// or empty one says nothing, so routing treats the provider as unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHeadroom {
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    #[serde(default)]
    pub observed_at: Option<Timestamp>,
    #[serde(default)]
    pub age_seconds: Option<u64>,
    #[serde(default = "unknown_is_stale")]
    pub stale: bool,
}

fn unknown_is_stale() -> bool {
    true
}

/// Percent of every window that stage routing leaves to the orchestrator:
/// on its own provider, and on providers the operator marks interactive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserves {
    pub orchestrator_provider: String,
    pub orchestrator: u8,
    pub interactive_providers: BTreeSet<String>,
    pub interactive: u8,
}

impl Reserves {
    /// The percent held back on `provider`: the larger reserve that applies.
    pub fn percent(&self, provider: &str) -> u8 {
        let orchestrator = if provider == self.orchestrator_provider {
            self.orchestrator
        } else {
            0
        };
        let interactive = if self.interactive_providers.contains(provider) {
            self.interactive
        } else {
            0
        };
        let percent = orchestrator.max(interactive);
        debug_assert!(percent <= 100, "reserves are validated as percentages");
        percent
    }
}

/// Seconds from `now` until `window`, read at `read_at`, resets.
fn seconds_to_reset(window: &QuotaWindow, read_at: Timestamp, now: Timestamp) -> Option<i64> {
    assert!(now >= read_at, "a reading is never from the future");
    Some((window.reset_at(read_at)? - now).num_seconds())
}

/// `((remaining − reserve) / time to reset) ÷ (100 / window length)`, in
/// `[0, MAX_HEADROOM]`; `None` once the reset passed (docs/configuration.md).
pub fn paced_headroom(window: &QuotaWindow, reserve: u8, until_reset: Option<i64>) -> Option<f64> {
    assert!(reserve <= 100, "a reserve is a percentage");
    if until_reset.is_some_and(|seconds| seconds <= 0) {
        return None;
    }
    let left = window.remaining_percent.min(100) as f64 - f64::from(reserve);
    if left <= 0.0 {
        return Some(0.0);
    }
    let paced = match (until_reset, window.window_minutes) {
        (Some(seconds), Some(minutes)) if minutes > 0 => {
            (left / seconds as f64) / (100.0 / (minutes as f64 * 60.0))
        }
        _ => left / 100.0,
    };
    let clamped = paced.clamp(0.0, MAX_HEADROOM);
    debug_assert!(clamped.is_finite(), "{paced} from {window:?}");
    Some(clamped)
}

/// What routing knows of one provider: the paced headroom of its tightest
/// window, and when it has none, when the windows that hold it at 0 reset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Headroom {
    pub factor: f64,
    pub reserve: u8,
    /// Set only at a factor of 0, and only when every window at 0 names its reset.
    pub resets_at: Option<Timestamp>,
}

impl Headroom {
    pub fn exhausted(&self) -> bool {
        self.factor <= 0.0
    }
}

/// `reading`'s headroom at `now`; `None` when it is stale, empty, or every
/// window it reports has reset since, so the provider counts as unknown.
pub fn provider_headroom(
    reading: &ProviderHeadroom,
    reserve: u8,
    read_at: Timestamp,
    now: Timestamp,
) -> Option<Headroom> {
    if reading.stale {
        return None;
    }
    let mut factor: Option<f64> = None;
    let mut exhausted_resets = Vec::new();
    for window in &reading.windows {
        let until = seconds_to_reset(window, read_at, now);
        let Some(paced) = paced_headroom(window, reserve, until) else {
            continue;
        };
        if paced <= 0.0 {
            exhausted_resets.push(until.map(|s| now + chrono::TimeDelta::seconds(s)));
        }
        factor = Some(factor.map_or(paced, |f| f.min(paced)));
    }
    let factor = factor?;
    // The provider frees up only once every window at 0 has reset.
    let resets_at = match exhausted_resets.is_empty() {
        true => None,
        false => exhausted_resets
            .into_iter()
            .try_fold(now, |latest, at| at.map(|at| latest.max(at))),
    };
    debug_assert!((0.0..=MAX_HEADROOM).contains(&factor));
    debug_assert!(resets_at.is_none() || factor <= 0.0);
    Some(Headroom {
        factor,
        reserve,
        resets_at,
    })
}

/// Each provider's headroom for stage routing at one moment. A provider it
/// does not know keeps its plain weight (a factor of 1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaView {
    known: BTreeMap<String, Headroom>,
}

impl QuotaView {
    /// The view of `readings`, taken by aetherd at `read_at`, at `now`.
    pub fn new<'a>(
        readings: impl IntoIterator<Item = (&'a str, &'a ProviderHeadroom)>,
        reserves: &Reserves,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Self {
        assert!(now >= read_at, "a reading is never from the future");
        let known: BTreeMap<String, Headroom> = readings
            .into_iter()
            .filter_map(|(provider, reading)| {
                let reserve = reserves.percent(provider);
                let headroom = provider_headroom(reading, reserve, read_at, now)?;
                Some((provider.to_owned(), headroom))
            })
            .collect();
        debug_assert!(known.keys().all(|p| !p.is_empty()));
        Self { known }
    }

    pub fn get(&self, provider: &str) -> Option<&Headroom> {
        self.known.get(provider)
    }

    /// The multiplier of `provider`'s weight: 1 while its usage is unknown.
    pub fn factor(&self, provider: &str) -> f64 {
        self.known.get(provider).map_or(1.0, |h| h.factor)
    }

    pub fn any_exhausted(&self) -> bool {
        self.known.values().any(Headroom::exhausted)
    }

    /// The earliest of `providers`' known resets, when the provider is at 0.
    pub fn earliest_reset<'a>(
        &self,
        providers: impl IntoIterator<Item = &'a str>,
    ) -> Option<Timestamp> {
        providers
            .into_iter()
            .filter_map(|p| self.known.get(p).and_then(|h| h.resets_at))
            .min()
    }
}

/// Why a queued item waits: every provider that could serve one of its
/// tier-only stages at its floor is out of quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaHold {
    /// The plan key whose stage has nowhere to run.
    pub stage: String,
    /// The earliest reset among those providers; `None` when none named one.
    pub until: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn window(remaining: u64, minutes: u64, resets_in: i64, now: Timestamp) -> QuotaWindow {
        QuotaWindow {
            name: "w".into(),
            window_minutes: Some(minutes),
            remaining_percent: remaining,
            resets_at: Some(now + chrono::TimeDelta::seconds(resets_in)),
            resets_in_seconds: None,
        }
    }

    fn reserves(orchestrator: u8) -> Reserves {
        Reserves {
            orchestrator_provider: "claude-code".into(),
            orchestrator,
            interactive_providers: BTreeSet::from(["gemini".to_owned()]),
            interactive: 10,
        }
    }

    const WEEK: u64 = 7 * 24 * 60;

    #[test]
    fn credit_about_to_reset_scores_higher_than_credit_that_must_last_days() {
        let now = at("2026-10-16T12:00:00Z");
        let soon = window(30, WEEK, 10 * 60, now);
        let later = window(30, WEEK, 5 * 24 * 3600, now);
        let soon = paced_headroom(&soon, 0, seconds_to_reset(&soon, now, now)).unwrap();
        let later = paced_headroom(&later, 0, seconds_to_reset(&later, now, now)).unwrap();
        assert!(soon > later, "{soon} <= {later}");
        assert_eq!(soon, MAX_HEADROOM, "clamped");
        assert!((later - 0.42).abs() < 0.01, "30% over 5 of 7 days: {later}");
    }

    #[test]
    fn the_tightest_window_and_the_reserve_bound_a_provider() {
        let now = at("2026-10-16T12:00:00Z");
        let reading = ProviderHeadroom {
            windows: vec![window(80, 300, 150 * 60, now), window(14, WEEK, 3600, now)],
            observed_at: Some(now),
            age_seconds: Some(0),
            stale: false,
        };
        let open = provider_headroom(&reading, 0, now, now).unwrap();
        assert!(
            (open.factor - 1.6).abs() < 1e-9,
            "the 5-hour window binds: {open:?}"
        );
        let held = provider_headroom(&reading, 15, now, now).unwrap();
        assert!(held.exhausted(), "14% left is under a 15% reserve");
        assert_eq!(held.resets_at, Some(now + chrono::TimeDelta::hours(1)));
        let stale = ProviderHeadroom {
            stale: true,
            ..reading.clone()
        };
        assert_eq!(provider_headroom(&stale, 0, now, now), None);
        let later = now + chrono::TimeDelta::hours(2);
        let reset = provider_headroom(&reading, 15, now, later).unwrap();
        assert!(
            !reset.exhausted(),
            "a window past its reset no longer binds"
        );
    }

    #[test]
    fn reserves_apply_to_the_orchestrator_and_interactive_providers_only() {
        let reserves = reserves(15);
        assert_eq!(reserves.percent("claude-code"), 15);
        assert_eq!(reserves.percent("gemini"), 10);
        assert_eq!(reserves.percent("codex"), 0);
        let now = at("2026-10-16T12:00:00Z");
        let low = ProviderHeadroom {
            windows: vec![window(12, 300, 3600, now)],
            observed_at: Some(now),
            age_seconds: Some(0),
            stale: false,
        };
        let view = QuotaView::new(
            [("claude-code", &low), ("codex", &low), ("gemini", &low)],
            &reserves,
            now,
            now,
        );
        assert!(view.get("claude-code").unwrap().exhausted());
        assert!(!view.get("codex").unwrap().exhausted());
        assert!(!view.get("gemini").unwrap().exhausted(), "12% is over 10%");
        assert_eq!(view.factor("unknown"), 1.0);
        let countdown = QuotaWindow {
            resets_at: None,
            resets_in_seconds: Some(600),
            ..window(50, 300, 0, now)
        };
        let later = now + chrono::TimeDelta::seconds(100);
        assert_eq!(seconds_to_reset(&countdown, now, later), Some(500));
    }

    #[test]
    fn headroom_reads_aethers_wire_shape_and_defaults_to_unknown() {
        let wire = serde_json::json!({
            "windows": [{"name":"primary", "window_minutes":300, "remaining_percent":62,
                "resets_at":"2030-01-02T15:30:00Z", "resets_in_seconds":900}],
            "observed_at":"2030-01-02T15:14:30Z", "age_seconds":30, "stale":false
        });
        let read: ProviderHeadroom = serde_json::from_value(wire).unwrap();
        assert_eq!(read.windows[0].remaining_percent, 62);
        assert!(!read.stale);
        let bare: ProviderHeadroom = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(
            bare.stale && bare.windows.is_empty(),
            "missing means unknown"
        );
    }
}
