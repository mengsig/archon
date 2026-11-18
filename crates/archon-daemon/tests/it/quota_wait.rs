//! A reviewer Aether holds on a spent provider quota is stopped after a few
//! minutes, and its lead is told to re-plan the review, instead of waiting
//! until the stage deadline.

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::ports::TreePulse;
use archon_core::quota::{ProviderHeadroom, QuotaWindow};
use archon_core::run::StageRun;
use archon_daemon::quota::Reading;
use chrono::TimeDelta;
use common::Harness;

/// The provider of [`common::REVIEW_MODEL`].
const REVIEW_PROVIDER: &str = "codex";

/// Aether's reading of the review provider with `remaining` percent left.
fn read_review_quota(h: &Harness, remaining: u64) {
    let headroom = ProviderHeadroom {
        windows: vec![QuotaWindow {
            name: "5h".into(),
            window_minutes: Some(300),
            remaining_percent: remaining,
            resets_at: Some(archon_core::now() + TimeDelta::hours(2)),
            resets_in_seconds: None,
        }],
        observed_at: Some(archon_core::now()),
        age_seconds: Some(0),
        stale: false,
    };
    h.ctx.quota.set_reading(Reading {
        read_at: archon_core::now(),
        providers: vec![(REVIEW_PROVIDER.to_owned(), headroom)],
    });
}

/// A running stage whose lead delegated a reviewer Aether holds for quota.
async fn held_reviewer(h: &Harness) -> (StageRun, AetherAgentId, AetherAgentId) {
    assert!(common::REVIEW_MODEL.starts_with(REVIEW_PROVIDER));
    h.enqueue("review on a spent quota").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let lead = stage.aether_agent.clone().expect("a lead");
    let reviewer = h.backend.delegate(&lead, common::REVIEW_MODEL);
    let held = TreePulse {
        held: true,
        held_for_cause: true,
        ..TreePulse::default()
    };
    h.backend.set_pulse(&lead, held);
    let blocked = TreePulse {
        quota_blocked: true,
        ..held
    };
    h.backend.set_pulse(&reviewer, blocked);
    (stage, lead, reviewer)
}

/// Move the recorded start of `reviewer`'s wait back by `ago`.
async fn backdate_wait(h: &Harness, stage: &StageRun, reviewer: &AetherAgentId, ago: TimeDelta) {
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let mut store = h.ctx.store.lock().await;
    let since = store.kv_get(&key).unwrap().expect("the wait is recorded");
    let since: archon_core::Timestamp = since.parse().unwrap();
    store.kv_set(&key, &(since - ago).to_rfc3339()).unwrap();
}

fn told(h: &Harness, lead: &AetherAgentId) -> Vec<String> {
    let messages = h.backend.messages().into_iter();
    messages
        .filter(|(to, text)| to == lead && text.contains("quota is spent"))
        .map(|(_, text)| text)
        .collect()
}

#[tokio::test]
async fn a_reviewer_waiting_on_a_spent_quota_fails_within_five_minutes() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    read_review_quota(&h, 0);
    h.dispatch().await;
    assert!(
        h.backend.cancelled().is_empty(),
        "a fresh wait is not stopped"
    );
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    read_review_quota(&h, 0);
    h.dispatch().await;
    assert_eq!(h.backend.cancelled(), vec![reviewer.clone()]);
    let told = told(&h, &lead);
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains(reviewer.as_str()), "{}", told[0]);
    assert!(told[0].contains("Re-plan the review"), "{}", told[0]);
    assert!(told[0].contains("fail_task"), "{}", told[0]);
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "a stopped reviewer's wait is forgotten");
}

#[tokio::test]
async fn a_quota_restored_before_the_limit_stops_nothing() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    read_review_quota(&h, 0);
    h.dispatch().await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(2)).await;
    read_review_quota(&h, 40);
    h.dispatch().await;
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "a restored quota clears the wait");
    // Spent again later: the wait starts over, so nothing is stopped yet.
    read_review_quota(&h, 0);
    h.dispatch().await;
    assert!(h.backend.cancelled().is_empty());
    assert!(told(&h, &lead).is_empty());
}

#[tokio::test]
async fn a_reviewer_held_for_another_cause_is_not_stopped() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    // Held for an approval or credentials, or working while its tree is held.
    let other_holds = [
        TreePulse {
            held: true,
            held_for_cause: true,
            ..TreePulse::default()
        },
        TreePulse {
            running: true,
            ..TreePulse::default()
        },
    ];
    for pulse in other_holds {
        h.backend.set_pulse(&reviewer, pulse);
        read_review_quota(&h, 0);
        h.dispatch().await;
        let key = format!("quota_wait:{}:{reviewer}", stage.id);
        let mark = h.ctx.store.lock().await.kv_get(&key).unwrap();
        assert_eq!(mark, None, "{pulse:?} does not wait on the quota");
    }
    assert!(h.backend.cancelled().is_empty());
    assert!(told(&h, &lead).is_empty());
}
