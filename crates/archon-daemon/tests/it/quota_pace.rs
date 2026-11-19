//! Quota pace: a provider whose window is projected to run out before it
//! resets is told to the operator and the orchestrator once per window, with
//! no provider weights set; stale or partial readings never warn.

use crate::common;

use archon_core::config::Config;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{Reply, Request};
use archon_core::quota::{ProviderHeadroom, QuotaWindow};
use archon_daemon::quota::Reading;
use archon_daemon::triggers::{self, TriggerEvent};
use chrono::TimeDelta;
use common::Harness;

/// A seven-day window with `remaining` percent left, resetting `in_` from now.
fn week(remaining: u64, in_: TimeDelta, stale: bool) -> ProviderHeadroom {
    ProviderHeadroom {
        windows: vec![QuotaWindow {
            name: "7d".into(),
            window_minutes: Some(7 * 24 * 60),
            remaining_percent: remaining,
            resets_at: Some(archon_core::now() + in_),
            resets_in_seconds: None,
        }],
        observed_at: Some(archon_core::now()),
        age_seconds: Some(0),
        stale,
    }
}

fn read(h: &Harness, ago: TimeDelta, providers: Vec<(&str, ProviderHeadroom)>) {
    let providers = (providers.into_iter())
        .map(|(p, r)| (p.to_owned(), r))
        .collect();
    h.ctx.quota.set_reading(Reading {
        read_at: archon_core::now() - ago,
        providers,
    });
}

async fn warnings(h: &Harness) -> Vec<String> {
    let notices = h.notices(NotificationKind::Decision).await;
    notices
        .into_iter()
        .filter(|t| t.starts_with("Quota pace:"))
        .collect()
}

async fn pace_news(h: &Harness) -> Vec<(String, String, u8)> {
    let held = triggers::held(&h.ctx).await.expect("held news");
    let news = held.into_iter().filter_map(|event| match event {
        TriggerEvent::QuotaPace { provider, pace, .. } => {
            Some((provider, pace.window, pace.used_percent))
        }
        _ => None,
    });
    news.collect()
}

#[tokio::test]
async fn a_short_window_warns_once_per_window_and_shows_in_status() {
    let h = Harness::with_config(Config::default()).await;
    // Codex: 78% used two days into seven; Claude: 20% used five days in.
    let codex = || week(22, TimeDelta::days(5), false);
    let claude = week(80, TimeDelta::days(2), false);
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", codex()), ("claude-code", claude)],
    );
    h.dispatch().await;
    let told = warnings(&h).await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains("codex's 7d window is 78% used with 29% of it elapsed"),
        "{told:?}"
    );
    assert!(told[0].contains("before it resets at"), "{told:?}");
    assert_eq!(
        pace_news(&h).await,
        [("codex".to_owned(), "7d".to_owned(), 78)]
    );

    let status = match h.cli(Request::Status(Default::default())).await {
        Ok(Reply::Status(status)) => *status,
        other => panic!("status replied {other:?}"),
    };
    let quota = status.quota.expect("status shows the reading");
    let line = |name: &str| {
        let provider = quota.providers.iter().find(|p| p.provider == name).unwrap();
        provider.describe()
    };
    assert!(
        line("codex").contains("; pace 7d used 78% at 29% elapsed, runs out ~"),
        "{}",
        line("codex")
    );
    assert!(line("claude-code").ends_with("; pace 7d used 20% at 71% elapsed"));

    // A later reading of the same window, its reset a few seconds off, says nothing new.
    read(
        &h,
        TimeDelta::zero(),
        vec![(
            "codex",
            week(15, TimeDelta::days(5) + TimeDelta::seconds(30), false),
        )],
    );
    h.dispatch().await;
    assert_eq!(warnings(&h).await.len(), 1, "once per window");
    // The next window, short again, is told again.
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", week(40, TimeDelta::days(6), false))],
    );
    h.dispatch().await;
    assert_eq!(
        warnings(&h).await.len(),
        1,
        "6 days ahead is still the same window"
    );
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", week(40, TimeDelta::days(12), false))],
    );
    h.dispatch().await;
    assert_eq!(warnings(&h).await.len(), 2, "a new window warns again");
    assert_eq!(pace_news(&h).await.len(), 2);
}

#[tokio::test]
async fn stale_old_or_partial_readings_never_warn() {
    let h = Harness::with_config(Config::default()).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", week(5, TimeDelta::days(5), true))],
    );
    h.dispatch().await;
    // Ten minutes is as long as a reading is trusted.
    read(
        &h,
        TimeDelta::minutes(11),
        vec![("codex", week(5, TimeDelta::days(5), false))],
    );
    h.dispatch().await;
    // Aether 1.4.1 may report a window without its length or reset.
    let mut partial = week(5, TimeDelta::days(5), false);
    partial.windows[0].window_minutes = None;
    let mut no_reset = week(5, TimeDelta::days(5), false);
    no_reset.windows[0].resets_at = None;
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", partial), ("claude-code", no_reset)],
    );
    h.dispatch().await;
    assert!(warnings(&h).await.is_empty());
    assert!(pace_news(&h).await.is_empty());
}
