//! A reviewer Aether holds on a spent provider quota is stopped after a few
//! minutes, and its lead is told to re-plan the review, instead of waiting
//! until the stage deadline.

use std::time::Duration;

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::ports::TreePulse;
use archon_core::quota::{ProviderHeadroom, QuotaWindow};
use archon_core::run::StageRun;
use archon_daemon::executor::quota_wait;
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

/// A running stage whose lead delegated a reviewer Aether holds for quota:
/// the stage started last.
async fn held_reviewer(h: &Harness) -> (StageRun, AetherAgentId, AetherAgentId) {
    assert!(common::REVIEW_MODEL.starts_with(REVIEW_PROVIDER));
    h.enqueue("review on a spent quota").await;
    h.dispatch().await;
    let stage = h.running().await.pop().expect("a running stage");
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

/// One quota sweep, as the daemon's own timer runs it.
async fn sweep(h: &Harness) {
    quota_wait::sweep_running(&h.ctx, archon_core::now())
        .await
        .expect("a quota sweep");
}

/// Other running stages, started before the one with the reviewer, whose
/// every read hangs for `delay`.
async fn slow_stages(h: &Harness, count: usize, delay: Duration) {
    for n in 0..count {
        h.enqueue(&format!("unrelated {n}")).await;
    }
    h.dispatch().await;
    let others = h.running().await;
    assert_eq!(others.len(), count, "{count} unrelated stages run");
    for other in others.iter().filter_map(|s| s.aether_agent.as_ref()) {
        h.backend.slow_answers(other, delay);
    }
}

/// Advance the paused clock a second at a time until `done`, at most
/// `limit`; how long it took.
async fn advance_until(limit: Duration, mut done: impl AsyncFnMut() -> bool) -> Duration {
    let started = tokio::time::Instant::now();
    while !done().await {
        assert!(started.elapsed() < limit, "not within {limit:?}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    started.elapsed()
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
    sweep(&h).await;
    assert!(
        h.backend.cancelled().is_empty(),
        "a fresh wait is not stopped"
    );
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    read_review_quota(&h, 0);
    sweep(&h).await;
    assert_eq!(h.backend.cancelled(), vec![reviewer.clone()]);
    let reasons = h.backend.cancel_reasons();
    assert!(reasons[0].1.contains("Re-plan the review"), "{reasons:?}");
    let told = told(&h, &lead);
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains(reviewer.as_str()), "{}", told[0]);
    assert!(told[0].contains("Re-plan the review"), "{}", told[0]);
    assert!(told[0].contains("fail_task"), "{}", told[0]);
    assert!(told[0].contains("resets at"), "{}", told[0]);
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "a stopped reviewer's wait is forgotten");
}

#[tokio::test]
async fn a_quota_restored_before_the_limit_stops_nothing() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    read_review_quota(&h, 0);
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(2)).await;
    // Aether runs the reviewer again once its quota is back.
    let working = TreePulse {
        running: true,
        ..TreePulse::default()
    };
    h.backend.set_pulse(&reviewer, working);
    read_review_quota(&h, 40);
    sweep(&h).await;
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "a restored quota clears the wait");
    // Blocked again later: the wait starts over, so nothing is stopped yet.
    let blocked = TreePulse {
        held: true,
        held_for_cause: true,
        quota_blocked: true,
        ..TreePulse::default()
    };
    h.backend.set_pulse(&reviewer, blocked);
    read_review_quota(&h, 0);
    sweep(&h).await;
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
        sweep(&h).await;
        let key = format!("quota_wait:{}:{reviewer}", stage.id);
        let mark = h.ctx.store.lock().await.kv_get(&key).unwrap();
        assert_eq!(mark, None, "{pulse:?} does not wait on the quota");
    }
    assert!(h.backend.cancelled().is_empty());
    assert!(told(&h, &lead).is_empty());
}

/// Aether's block is enough: a reading that shows the provider with quota
/// left, or none at all, does not keep a blocked reviewer waiting.
#[tokio::test]
async fn a_quota_blocked_reviewer_fails_without_a_matching_reading() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    read_review_quota(&h, 40);
    sweep(&h).await;
    assert_eq!(h.backend.cancelled(), vec![reviewer.clone()]);
    let told = told(&h, &lead);
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("names no reset"), "{}", told[0]);
}

/// Aether resumes the reviewer after the sweep listed it as blocked and
/// before its cancel: the block is read again, so nothing is stopped.
#[tokio::test]
async fn a_reviewer_that_resumes_before_its_cancel_is_not_stopped() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    let working = TreePulse {
        running: true,
        ..TreePulse::default()
    };
    h.backend.set_pulse_after_reads(&reviewer, 1, working);
    sweep(&h).await;
    assert!(
        h.backend.cancelled().is_empty(),
        "a working reviewer is kept"
    );
    assert!(told(&h, &lead).is_empty(), "its lead is told nothing");
    // One read a sweep, and one more right before the cancel it then skips.
    assert_eq!(h.backend.quota_reads(), vec![reviewer.clone(); 3]);
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "its wait is forgotten");
}

/// Aether refuses the cancel, most likely as the reviewer ended: the lead is
/// told nothing and the wait is forgotten.
#[tokio::test]
async fn a_refused_cancel_tells_the_lead_nothing() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    let refusal = archon_core::error::ArchonError::Invalid("agent has ended".into());
    h.backend.fail_next_cancel(refusal);
    sweep(&h).await;
    assert_eq!(h.backend.cancel_reasons().len(), 1, "one cancel was tried");
    assert!(h.backend.cancelled().is_empty());
    assert!(told(&h, &lead).is_empty(), "the lead is told nothing");
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "its wait is forgotten");
}

/// A cancel Aether accepts comes first, with the hint as its reason; the
/// lead's message follows it and repeats the same words.
#[tokio::test]
async fn the_cancel_carries_the_hint_and_comes_before_the_message() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    // The message is refused: the cancel was sent all the same, before it.
    let refusal = archon_core::error::ArchonError::Invalid("lead has ended".into());
    h.backend.fail_next_message(refusal);
    sweep(&h).await;
    assert_eq!(h.backend.cancelled(), vec![reviewer.clone()]);
    let reasons = h.backend.cancel_reasons();
    assert!(reasons[0].1.contains("Re-plan the review"), "{reasons:?}");
    assert!(told(&h, &lead).is_empty(), "the message was refused");
}

/// A wait saved ahead of the clock, as a wall clock stepped back across a
/// restart leaves it, counts from the sweep that finds it and is saved so:
/// the reviewer is stopped three minutes later, not when the clock catches up.
#[tokio::test]
async fn a_wait_recorded_ahead_of_the_clock_counts_from_now() {
    let h = Harness::new().await;
    let (stage, _, reviewer) = held_reviewer(&h).await;
    let now = archon_core::now();
    quota_wait::sweep_running(&h.ctx, now).await.unwrap();
    backdate_wait(&h, &stage, &reviewer, -TimeDelta::minutes(20)).await;
    quota_wait::sweep_running(&h.ctx, now).await.unwrap();
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let saved = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(saved, Some(now.to_rfc3339()), "the wait is saved from now");
    let due = now + TimeDelta::minutes(3);
    quota_wait::sweep_running(&h.ctx, due).await.unwrap();
    assert_eq!(h.backend.cancelled(), vec![reviewer.clone()]);
}

/// A due stage slow in every request of its check, each within the request
/// limit, does not push back the next sweep: another stage's reviewer is
/// still read every period, and the slow check is not started a second time.
#[tokio::test(start_paused = true)]
async fn a_stage_slow_in_every_request_does_not_push_back_the_next_sweep() {
    let h = Harness::new().await;
    let (_, _, fast) = held_reviewer(&h).await;
    let (slow_stage, slow_lead, slow) = held_reviewer(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    let reads = || {
        h.backend
            .quota_reads()
            .iter()
            .filter(|r| **r == fast)
            .count()
    };
    advance_until(quota_wait::SWEEP_INTERVAL, || async { reads() == 1 }).await;
    let listings = || {
        let listed = h.backend.descendant_checks();
        listed.iter().filter(|lead| **lead == slow_lead).count()
    };
    let listed_before = listings();
    // From the next sweep on, the other stage is due and answers slowly.
    backdate_wait(&h, &slow_stage, &slow, TimeDelta::minutes(3)).await;
    let delay = quota_wait::REQUEST_LIMIT - Duration::from_secs(1);
    h.backend.slow_answers(&slow_lead, delay);
    h.backend.slow_answers(&slow, delay);
    let period = quota_wait::SWEEP_INTERVAL;
    for sweep in 2..=4 {
        let took = advance_until(period, || async { reads() == sweep }).await;
        assert_eq!(took, period, "sweep {sweep} starts a period on");
    }
    assert_eq!(h.backend.cancelled(), vec![slow.clone()]);
    assert_eq!(told(&h, &slow_lead).len(), 1, "its lead is told once");
    // Its check outlasted a period: the sweep due meanwhile passed it over.
    assert!(delay * 6 > period && delay * 6 < period * 2, "{delay:?}");
    assert_eq!(listings(), listed_before + 1, "one check, not two at once");
    watcher.abort();
}

/// The sweep keeps its own 30-second timer: with no dispatch pass at all, a
/// blocked reviewer is stopped within minutes of the paused clock.
#[tokio::test(start_paused = true)]
async fn the_quota_timer_stops_a_reviewer_without_any_dispatch() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL + Duration::from_secs(1)).await;
    assert!(h.backend.cancelled().is_empty(), "a fresh wait is kept");
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL).await;
    assert_eq!(h.backend.cancelled(), vec![reviewer.clone()]);
    assert_eq!(told(&h, &lead).len(), 1);
    watcher.abort();
}

/// Every stage is checked at once: stages checked before a due reviewer's,
/// whose reads hang, do not hold its cancel back by their time limits.
#[tokio::test(start_paused = true)]
async fn slow_unrelated_stages_do_not_delay_a_due_reviewer() {
    let h = Harness::new().await;
    slow_stages(&h, 4, Duration::from_secs(3600)).await;
    let (stage, _, reviewer) = held_reviewer(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    let seen = || async {
        !h.ctx
            .store
            .lock()
            .await
            .kv_prefix("quota_wait:")
            .unwrap()
            .is_empty()
    };
    advance_until(quota_wait::SWEEP_INTERVAL, seen).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    // The next sweep starts a period on, whatever the slow stages' checks do.
    let next_sweep = quota_wait::SWEEP_INTERVAL;
    let stopped = || async { h.backend.cancelled() == vec![reviewer.clone()] };
    let took = advance_until(next_sweep + Duration::from_secs(2), stopped).await;
    assert!(took <= next_sweep + Duration::from_secs(1), "{took:?}");
    watcher.abort();
}

/// With every read of its stage slow, though within the request limit, and
/// other stages not answering at all, a blocked reviewer is still cancelled
/// within five minutes and one sweep period of first being seen blocked.
#[tokio::test(start_paused = true)]
async fn a_blocked_reviewer_is_cancelled_in_time_though_every_read_is_slow() {
    cancelled_in_time(quota_wait::REQUEST_LIMIT - Duration::from_secs(1)).await;
}

/// An Aether that takes five seconds over every answer is slow, not silent:
/// its answers are read, and the blocked reviewer is cancelled in time.
#[tokio::test(start_paused = true)]
async fn a_blocked_reviewer_is_cancelled_in_time_when_every_answer_takes_five_seconds() {
    cancelled_in_time(Duration::from_secs(5)).await;
}

/// A reviewer whose stage answers every request after `slow`, beside stages
/// that never answer, is cancelled within five minutes and a sweep period.
async fn cancelled_in_time(slow: Duration) {
    let h = Harness::new().await;
    slow_stages(&h, 3, Duration::from_secs(3600)).await;
    let (_, lead, reviewer) = held_reviewer(&h).await;
    h.backend.slow_answers(&lead, slow);
    h.backend.slow_answers(&reviewer, slow);
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    let seen = || async {
        !h.ctx
            .store
            .lock()
            .await
            .kv_prefix("quota_wait:")
            .unwrap()
            .is_empty()
    };
    advance_until(Duration::from_secs(60), seen).await;
    let bound = Duration::from_secs(5 * 60) + quota_wait::SWEEP_INTERVAL;
    let stopped = || async { h.backend.cancelled() == vec![reviewer.clone()] };
    let took = advance_until(bound, stopped).await;
    assert!(took >= Duration::from_secs(60), "not at once: {took:?}");
    // The message follows the cancel, as slow as every other answer.
    let is_told = || async { told(&h, &lead).len() == 1 };
    advance_until(quota_wait::REQUEST_LIMIT, is_told).await;
    watcher.abort();
}
