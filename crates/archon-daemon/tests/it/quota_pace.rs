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

/// Each held `quota_pace`: provider, window, used %, and the providers it shows.
async fn pace_news(h: &Harness) -> Vec<(String, String, u8, Vec<String>)> {
    let held = triggers::held(&h.ctx).await.expect("held news");
    let news = held.into_iter().filter_map(|event| match event {
        TriggerEvent::QuotaPace {
            provider,
            pace,
            providers,
            ..
        } => {
            // A window just opened has no rate yet; one under way runs out before it resets.
            assert!(pace.short && pace.empty_at.is_none_or(|at| at < pace.resets_at));
            let shown = providers.into_keys().collect();
            Some((provider, pace.window, pace.used_percent, shown))
        }
        _ => None,
    });
    news.collect()
}

#[tokio::test]
async fn a_short_window_warns_once_per_window_and_shows_in_status() {
    let mut h = Harness::with_config(Config::default()).await;
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
        [(
            "codex".to_owned(),
            "7d".to_owned(),
            78,
            vec!["claude-code".to_owned(), "codex".to_owned()]
        )]
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
        line("codex").contains("; pace 7d used 78% at 29% elapsed, empty ~"),
        "{}",
        line("codex")
    );
    assert!(
        line("codex").contains(", before its reset "),
        "{}",
        line("codex")
    );
    let claude = line("claude-code");
    assert!(
        claude.contains("; pace 7d used 20% at 71% elapsed, empty ~"),
        "{claude}"
    );
    assert!(claude.contains(", resets "), "{claude}");

    // The receipt outlives a restart.
    h.restart();
    read(&h, TimeDelta::zero(), vec![("codex", codex())]);
    h.dispatch().await;
    assert_eq!(warnings(&h).await.len(), 1, "a restart does not warn again");

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
async fn the_configured_margin_decides_what_is_short() {
    let mut config = Config::default();
    config.models.pacing_margin_percent = Some(50);
    let h = Harness::with_config(config).await;
    // 78% used at 29% elapsed is 49 points ahead: within a 50-point margin.
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", week(22, TimeDelta::days(5), false))],
    );
    h.dispatch().await;
    assert!(warnings(&h).await.is_empty());
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", week(20, TimeDelta::days(5), false))],
    );
    h.dispatch().await;
    assert_eq!(warnings(&h).await.len(), 1, "80% used is 51 points ahead");
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
    // A reply received just now that carries an 11-minute-old observation.
    let mut old = week(5, TimeDelta::days(5), false);
    old.age_seconds = Some(660);
    read(&h, TimeDelta::zero(), vec![("codex", old)]);
    h.dispatch().await;
    // Aether 1.4.1 may report a window without its length or reset.
    let mut partial = week(5, TimeDelta::days(5), false);
    partial.windows[0].window_minutes = None;
    let mut no_reset = week(5, TimeDelta::days(5), false);
    no_reset.windows[0].resets_at = None;
    no_reset.windows[0].resets_in_seconds = Some(5 * 24 * 3600);
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", partial), ("claude-code", no_reset)],
    );
    h.dispatch().await;
    assert!(warnings(&h).await.is_empty());
    assert!(pace_news(&h).await.is_empty());
}

#[tokio::test]
async fn a_sample_that_ages_out_while_waiting_for_the_store_warns_of_nothing() {
    let h = Harness::with_config(Config::default()).await;
    read(
        &h,
        TimeDelta::zero(),
        vec![("codex", week(5, TimeDelta::days(5), false))],
    );
    let store = h.ctx.store.lock().await;
    let ctx = h.ctx.clone();
    let warn = tokio::spawn(async move { archon_daemon::quota::warn_short(&ctx).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!warn.is_finished(), "the warning waits for the store");
    read(
        &h,
        TimeDelta::minutes(11),
        vec![("codex", week(5, TimeDelta::days(5), false))],
    );
    drop(store);
    warn.await.expect("joins").expect("warns");
    assert!(warnings(&h).await.is_empty());
    assert!(pace_news(&h).await.is_empty());
}

/// A connected fake aetherd that never answers `providers`, under a 500 ms
/// request timeout.
async fn silent_providers(h: &Harness) -> archon_aether::fake::FakeAether {
    use archon_aether::client::{AetherClient, ClientOptions};
    use archon_aether::fake::{FakeAether, Scripted};
    let aether = FakeAether::start(&h.dir.path().join("aetherd.sock"))
        .await
        .expect("fake aetherd");
    aether.set_default("providers", Scripted::Silent);
    let options = ClientOptions {
        request_timeout: std::time::Duration::from_millis(500),
        ..ClientOptions::default()
    };
    let client = AetherClient::connect_with(&aether.endpoint(), options)
        .await
        .expect("connect");
    let linked = h.ctx.aether.replace(Some(std::sync::Arc::new(client)));
    linked.expect("link");
    aether
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callers_joining_a_failed_ask_take_its_failure_instead_of_asking_again() {
    use archon_daemon::quota::{Asker, refresh};
    use std::time::{Duration, Instant};

    let h = Harness::with_config(Config::default()).await;
    let aether = silent_providers(&h).await;
    let started = Instant::now();
    let ctx = h.ctx.clone();
    let poll = tokio::spawn(async move { refresh(&ctx, Asker::PacePoll).await });
    aether
        .wait_for("providers", 1, Duration::from_secs(5))
        .await;
    let joiners: Vec<_> = (0..8)
        .map(|_| {
            let ctx = h.ctx.clone();
            tokio::spawn(async move { refresh(&ctx, Asker::Routing).await })
        })
        .collect();
    assert!(poll.await.expect("joins").is_err());
    // A new ask starting the moment the first fails holds no joiner of the first.
    let ctx = h.ctx.clone();
    let next = tokio::spawn(async move { refresh(&ctx, Asker::Routing).await });
    for joiner in joiners {
        assert!(
            joiner.await.expect("joins").is_err(),
            "a joiner takes the failure"
        );
    }
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "one timeout, not two"
    );
    assert!(next.await.expect("joins").is_err());
    assert_eq!(
        aether.received_named("providers").len(),
        2,
        "the joiners asked nothing"
    );
}
