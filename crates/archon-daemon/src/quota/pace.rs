//! Warn the orchestrator and the operator once per window when a provider's
//! quota is projected to run out before it resets. Nothing is throttled: the
//! orchestrator chooses what to run (docs/quota-pacing.md).

use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::quota::pace::WindowPace;
use chrono::TimeDelta;

use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

/// `kv`: each `provider/window` warned of and the reset it was warned for.
const WARNED_KEY: &str = "quota_pace_warned";

/// Tell both, once per provider window, of each short window the last
/// trusted reading shows.
pub(crate) async fn warn_short(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let short: Vec<(String, WindowPace)> = (super::paces(ctx).into_iter())
        .flat_map(|(provider, paces)| {
            (paces.into_iter().filter(|pace| pace.short)).map(move |pace| (provider.clone(), pace))
        })
        .collect();
    if short.is_empty() {
        return Ok(());
    }
    for (provider, pace) in claim(ctx, short).await? {
        let text = warning_text(&provider, &pace);
        crate::executor::notify(ctx, NotificationKind::Decision, &text, None, None).await?;
        let note = format!(
            "At this rate {provider} runs out before its reset. Archon throttles nothing: \
             choose what runs there (fewer items at once, lower efforts, another provider) \
             and tell the operator."
        );
        let event = TriggerEvent::QuotaPace {
            provider,
            pace,
            note,
        };
        triggers::push(ctx, event).await?;
    }
    Ok(())
}

/// The windows of `short` not yet warned of in their current window, now
/// recorded as warned.
async fn claim(
    ctx: &DaemonContext,
    short: Vec<(String, WindowPace)>,
) -> Result<Vec<(String, WindowPace)>, ArchonError> {
    assert!(
        short
            .iter()
            .all(|(provider, pace)| !provider.is_empty() && pace.short)
    );
    let mut store = ctx.store.lock().await;
    let raw = store.kv_get(WARNED_KEY)?.unwrap_or_default();
    // An unreadable record only costs a repeated warning, never dispatch.
    let mut warned: BTreeMap<String, Timestamp> = match serde_json::from_str(&raw) {
        Ok(warned) => warned,
        Err(_) if raw.is_empty() => BTreeMap::new(),
        Err(error) => {
            tracing::warn!(%error, "kv {WARNED_KEY} is unreadable; warning afresh");
            BTreeMap::new()
        }
    };
    let fresh: Vec<(String, WindowPace)> = (short.into_iter())
        .filter(|(provider, pace)| is_new_window(warned.get(&key(provider, pace)), pace))
        .collect();
    if fresh.is_empty() {
        return Ok(fresh);
    }
    for (provider, pace) in &fresh {
        warned.insert(key(provider, pace), pace.resets_at);
    }
    store.kv_set(WARNED_KEY, &serde_json::to_string(&warned)?)?;
    debug_assert!(
        fresh
            .iter()
            .all(|(p, pace)| warned.get(&key(p, pace)) == Some(&pace.resets_at))
    );
    Ok(fresh)
}

fn key(provider: &str, pace: &WindowPace) -> String {
    format!("{provider}/{}", pace.window)
}

/// Whether `pace` is a later window than the one warned for at `warned`.
/// A countdown reading moves the reset by seconds, so only a jump of half
/// a window counts as a new one.
fn is_new_window(warned: Option<&Timestamp>, pace: &WindowPace) -> bool {
    let Some(warned) = warned else {
        return true;
    };
    let half = i64::try_from(pace.window_minutes / 2).unwrap_or(i64::MAX);
    let half = TimeDelta::try_minutes(half).unwrap_or(TimeDelta::MAX);
    warned
        .checked_add_signed(half)
        .is_some_and(|next| pace.resets_at > next)
}

/// What the operator and orchestrator are told.
fn warning_text(provider: &str, pace: &WindowPace) -> String {
    assert!(pace.short, "only a short window is warned of");
    let empty = pace.empty_at.map_or("soon".to_owned(), |at| {
        format!("around {}", at.format("%Y-%m-%d %H:%M UTC"))
    });
    format!(
        "Quota pace: {provider}'s {} window is {}% used with {}% of it elapsed. At this rate \
         it runs out {empty}, before it resets at {}. Archon throttles nothing; the \
         orchestrator chooses what runs on {provider} until then.",
        pace.window,
        pace.used_percent,
        pace.elapsed_percent,
        pace.resets_at.format("%Y-%m-%d %H:%M UTC")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pace(resets_at: Timestamp) -> WindowPace {
        WindowPace {
            window: "7d".into(),
            window_minutes: 10080,
            used_percent: 70,
            elapsed_percent: 30,
            resets_at,
            empty_at: None,
            short: true,
        }
    }

    #[test]
    fn a_window_is_new_only_once_its_reset_moves_by_half_a_window() {
        let reset = chrono::DateTime::parse_from_rfc3339("2026-11-26T00:00:00Z")
            .unwrap()
            .to_utc();
        assert!(is_new_window(None, &pace(reset)));
        let jitter = reset + TimeDelta::seconds(40);
        assert!(
            !is_new_window(Some(&reset), &pace(jitter)),
            "the same window"
        );
        assert!(!is_new_window(
            Some(&reset),
            &pace(reset - TimeDelta::hours(1))
        ));
        let next = reset + TimeDelta::days(7);
        assert!(is_new_window(Some(&reset), &pace(next)), "the next window");
    }
}
