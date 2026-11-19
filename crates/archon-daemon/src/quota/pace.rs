//! Warn the orchestrator and the operator once per window when a provider's
//! quota is projected to run out before it resets. Nothing is throttled: the
//! orchestrator chooses what to run (docs/quota-pacing.md).

use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::NotificationId;
use archon_core::notify::{Notification, NotificationKind};
use archon_core::quota::pace::WindowPace;
use archon_queue::Store;
use chrono::TimeDelta;

use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

/// `kv`: each `provider/window` warned of and the reset it was warned for.
const WARNED_KEY: &str = "quota_pace_warned";

/// Tell both, once per provider window, of each short window the last
/// trusted reading shows. Each warning, its feed notice and its receipt
/// commit together, so a crash loses none and repeats none.
pub async fn warn_short(ctx: &DaemonContext) -> Result<(), ArchonError> {
    if !any_short(&super::paces(ctx)) {
        return Ok(());
    }
    let mut told = Vec::new();
    let mut failed = None;
    {
        let mut store = ctx.store.lock().await;
        // Judged again under the writer lock: a sample that aged out or a
        // window that reset while this waited warns of nothing.
        let paces = super::paces(ctx);
        let short: Vec<(&String, &WindowPace)> = (paces.iter())
            .flat_map(|(provider, windows)| windows.iter().map(move |pace| (provider, pace)))
            .filter(|(_, pace)| pace.short)
            .collect();
        let mut warned = read_warned(&store)?;
        for (provider, pace) in short {
            if !is_new_window(warned.get(&key(provider, pace)), pace) {
                continue;
            }
            warned.insert(key(provider, pace), pace.resets_at);
            match record(&mut store, provider, pace, &paces, &warned) {
                Ok(item) => told.push(item),
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
    }
    // Those already committed are told now, whatever a later write did.
    if !told.is_empty() {
        crate::feed::publish(ctx, told);
        triggers::wake(ctx);
    }
    failed.map_or(Ok(()), Err)
}

fn any_short(paces: &BTreeMap<String, Vec<WindowPace>>) -> bool {
    paces.values().flatten().any(|pace| pace.short)
}

/// The warned record; an unreadable one only costs a repeated warning.
fn read_warned(store: &Store) -> Result<BTreeMap<String, Timestamp>, ArchonError> {
    let raw = store.kv_get(WARNED_KEY)?.unwrap_or_default();
    match serde_json::from_str(&raw) {
        Ok(warned) => Ok(warned),
        Err(_) if raw.is_empty() => Ok(BTreeMap::new()),
        Err(error) => {
            tracing::warn!(%error, "kv {WARNED_KEY} is unreadable; warning afresh");
            Ok(BTreeMap::new())
        }
    }
}

/// Hold one window's warning for both audiences with `warned` as its receipt.
fn record(
    store: &mut Store,
    provider: &str,
    pace: &WindowPace,
    paces: &BTreeMap<String, Vec<WindowPace>>,
    warned: &BTreeMap<String, Timestamp>,
) -> Result<archon_core::notify::FeedItem, ArchonError> {
    assert!(pace.short, "only a short window is warned of");
    assert_eq!(
        warned.get(&key(provider, pace)),
        Some(&pace.resets_at),
        "receipt first"
    );
    let notice = Notification {
        id: NotificationId::new(),
        kind: NotificationKind::Decision,
        text: warning_text(provider, pace),
        item: None,
        run: None,
        at: archon_core::now(),
    };
    let note = format!(
        "At this rate {provider} runs out before its reset. Archon throttles nothing: \
         choose what runs there (fewer items at once, lower efforts, another provider) \
         and tell the operator."
    );
    let event = TriggerEvent::QuotaPace {
        provider: provider.to_owned(),
        pace: pace.clone(),
        note,
        providers: paces.clone(),
    };
    let receipt = serde_json::to_string(warned)?;
    triggers::record_quota_pace(store, event, notice, (WARNED_KEY, &receipt))
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
