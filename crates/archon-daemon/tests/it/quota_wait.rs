//! The quota sweep reports held reviews and keeps checking for recovery
//! while leaving the stage and its reviewer alive.

use std::time::Duration;

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::{AgentBackend, DescendantState, TreePulse};
use archon_core::protocol::{Reply, Request};
use archon_core::quota::{ProviderHeadroom, QuotaWindow};
use archon_core::run::{StageRun, StageState};
use archon_daemon::executor::quota_wait;
use archon_daemon::quota::Reading;
use chrono::TimeDelta;
use common::Harness;
use serde_json::json;

/// The provider of [`common::REVIEW_MODEL`].
const REVIEW_PROVIDER: &str = "codex";
const ALTERNATE_PROVIDER: &str = "claude-code";
const ALTERNATE_MODEL: &str = "claude-code/claude-opus-5-5";

/// Aether's reading of the review provider with `remaining` percent left.
fn read_review_quota(h: &Harness, remaining: u64) {
    set_quota(h, &[(REVIEW_PROVIDER, remaining)]);
}

fn set_quota(h: &Harness, providers: &[(&str, u64)]) {
    assert!(!providers.is_empty());
    assert!(providers.iter().all(|(_, percent)| *percent <= 100));
    let now = archon_core::now();
    let headroom = |(provider, remaining): &(&str, u64)| {
        (
            (*provider).to_owned(),
            ProviderHeadroom {
                windows: vec![QuotaWindow {
                    name: "5h".into(),
                    window_minutes: Some(300),
                    remaining_percent: *remaining,
                    resets_at: Some(now + TimeDelta::hours(2)),
                    resets_in_seconds: None,
                }],
                observed_at: Some(now),
                age_seconds: Some(0),
                stale: false,
            },
        )
    };
    h.ctx.quota.set_reading(Reading {
        read_at: now,
        providers: providers.iter().map(headroom).collect(),
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
async fn a_reviewer_without_an_eligible_replacement_stays_live_and_is_reported_once() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    read_review_quota(&h, 0);
    sweep(&h).await;
    assert!(told(&h, &lead).is_empty(), "a fresh wait is not reported");
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    sweep(&h).await;
    sweep(&h).await;

    let reports = told(&h, &lead);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].contains(reviewer.as_str()), "{}", reports[0]);
    assert!(
        reports[0].contains("No eligible replacement"),
        "{}",
        reports[0]
    );
    assert!(reports[0].contains("resets at"), "{}", reports[0]);
    assert_reviewer_waiting(&h, &lead, &reviewer).await;
}

async fn assert_reviewer_waiting(h: &Harness, lead: &AetherAgentId, reviewer: &AetherAgentId) {
    let children = h.backend.descendants(lead).await.unwrap();
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(&children[0].agent, reviewer);
    assert_eq!(children[0].state, DescendantState::Live);
    assert!(h.backend.cancel_reasons().is_empty());
    assert!(h.backend.updates().is_empty());
}

async fn due_review_with_an_alternate(h: &Harness) -> (StageRun, AetherAgentId) {
    let lead = common::choice("codex/small", Effort::Medium);
    let original = common::choice(common::REVIEW_MODEL, Effort::Medium);
    let alternate = common::choice(ALTERNATE_MODEL, Effort::Medium);
    let plan = ModelPlan(
        [
            ("build".into(), vec![lead]),
            ("build.review".into(), vec![original, alternate]),
        ]
        .into(),
    );
    let mut request = common::enqueue_request(common::REPO_NAME, "adaptive", "review quota", plan);
    request.autonomy = AutonomyPatch {
        review: Some(ReviewLevel::Standard),
        ..Default::default()
    };
    let Reply::Item(_) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("expected an enqueued item");
    };
    h.dispatch().await;
    let stage = h.running().await.pop().expect("a running build stage");
    assert_eq!(stage.stage_id, "build");
    let lead = stage.aether_agent.as_ref().expect("a stage lead");
    let reviewer = h.backend.delegate(lead, common::REVIEW_MODEL);
    h.backend.set_pulse(
        &reviewer,
        TreePulse {
            held: true,
            held_for_cause: true,
            quota_blocked: true,
            ..TreePulse::default()
        },
    );
    set_quota(h, &[(REVIEW_PROVIDER, 0), (ALTERNATE_PROVIDER, 80)]);
    sweep(h).await;
    backdate_wait(h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    (stage, reviewer)
}

async fn assert_review_passes(
    h: &Harness,
    stage: &StageRun,
    reviewer: &AetherAgentId,
    model: &str,
) {
    let outcome = common::delivery::reviewed_by(
        h,
        stage,
        "Built and reviewed",
        std::slice::from_ref(reviewer),
    )
    .await;
    h.settle_running(|_| outcome.clone()).await;
    let settled = h.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert_eq!(settled.state, StageState::Completed, "{settled:?}");
    let review = &settled.structured.unwrap()["review"];
    assert_eq!(review["reviewer_agents"], json!([reviewer.as_str()]));
    assert_eq!(review["reviewer_models"], json!([model]));
}

#[tokio::test]
async fn a_replacement_completes_the_same_task_and_passes_the_gate() {
    let h = Harness::new().await;
    let (stage, reviewer) = due_review_with_an_alternate(&h).await;
    let lead = stage.aether_agent.as_ref().unwrap();
    let before = h.backend.descendants(lead).await.unwrap();

    sweep(&h).await;

    let after = h.backend.descendants(lead).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].agent, reviewer);
    assert_eq!(after[0].parent, *lead);
    assert_eq!(after[0].task, before[0].task);
    assert_eq!(after[0].model.to_string(), ALTERNATE_MODEL);
    assert_eq!(
        h.backend.quota_blocked(&reviewer).await.unwrap(),
        Some(false)
    );
    assert!(h.backend.cancelled().is_empty());
    assert_review_passes(&h, &stage, &reviewer, ALTERNATE_MODEL).await;
    assert!(
        h.backend
            .task_outcome(before[0].task.as_ref().unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_reviewer_unblocked_during_replacement_counts_once_on_the_model_that_answered() {
    let h = Harness::new().await;
    let (stage, reviewer) = due_review_with_an_alternate(&h).await;
    let lead = stage.aether_agent.as_ref().unwrap();
    let original = h.backend.descendants(lead).await.unwrap().remove(0);
    h.backend.set_pulse_after_reads(
        &reviewer,
        2,
        TreePulse {
            running: true,
            ..TreePulse::default()
        },
    );
    sweep(&h).await;

    let children = h.backend.descendants(lead).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].task, original.task);
    assert_eq!(children[0].model.to_string(), ALTERNATE_MODEL);
    assert!(h.backend.messages().iter().all(|(to, _)| *to != reviewer));
    h.backend
        .record_review_model(original.task.as_ref().unwrap(), common::REVIEW_MODEL);
    assert_review_passes(&h, &stage, &reviewer, common::REVIEW_MODEL).await;
}

#[tokio::test]
async fn a_replacement_retries_the_release_without_patching_again() {
    let h = Harness::new().await;
    let (stage, reviewer) = due_review_with_an_alternate(&h).await;
    h.backend
        .fail_next_message(archon_core::error::ArchonError::Unavailable(
            "lost reply".into(),
        ));
    let first = quota_wait::sweep_running(&h.ctx, archon_core::now()).await;
    assert!(first.unwrap_err().to_string().contains("lost reply"));
    assert_eq!(
        h.backend.quota_blocked(&reviewer).await.unwrap(),
        Some(true)
    );

    sweep(&h).await;

    assert_eq!(
        h.backend.quota_blocked(&reviewer).await.unwrap(),
        Some(false)
    );
    assert_eq!(h.backend.updates().len(), 1);
    assert_review_passes(&h, &stage, &reviewer, ALTERNATE_MODEL).await;
}

#[tokio::test]
async fn a_planned_model_denied_by_current_policy_cannot_replace_a_reviewer() {
    let h = Harness::new().await;
    let (stage, reviewer) = due_review_with_an_alternate(&h).await;
    let lead = stage.aether_agent.as_ref().unwrap();
    let dir = h.repo.join(".archon");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!("[models]\ndeny = [\"{ALTERNATE_MODEL}\"]\n"),
    )
    .unwrap();

    sweep(&h).await;

    assert_eq!(told(&h, lead).len(), 1);
    assert_reviewer_waiting(&h, lead, &reviewer).await;
    assert_eq!(
        h.ctx.store.lock().await.stage_run(&stage.id).unwrap().state,
        StageState::Running
    );
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

/// Aether's own block is enough to report even when the provider reading differs.
#[tokio::test]
async fn a_quota_block_is_reported_without_a_matching_reading() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    read_review_quota(&h, 40);
    sweep(&h).await;
    assert_reviewer_waiting(&h, &lead, &reviewer).await;
    let told = told(&h, &lead);
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("no reset is known"), "{}", told[0]);
}

/// Aether resumes the reviewer after the sweep listed it as blocked and
/// before the report. The sweep reads the block again and skips the report.
#[tokio::test]
async fn a_reviewer_that_resumes_before_the_report_is_kept() {
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
    // One read per sweep and one more before reporting a due block.
    assert_eq!(h.backend.quota_reads(), vec![reviewer.clone(); 3]);
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let left = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(left, None, "its wait is forgotten");
}

#[tokio::test]
async fn an_undelivered_report_is_retried_without_stopping_the_reviewer() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    sweep(&h).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    h.backend
        .fail_next_message(archon_core::error::ArchonError::Unavailable(
            "lost reply".into(),
        ));
    let result = quota_wait::sweep_running(&h.ctx, archon_core::now()).await;
    assert!(result.unwrap_err().to_string().contains("lost reply"));
    assert!(told(&h, &lead).is_empty());

    sweep(&h).await;
    assert_eq!(told(&h, &lead).len(), 1);
    assert_reviewer_waiting(&h, &lead, &reviewer).await;
}

/// A wait saved ahead of the clock, as a wall clock stepped back across a
/// restart leaves it, counts from the sweep that finds it and is saved so:
/// reporting is due three minutes after that observation.
#[tokio::test]
async fn a_wait_recorded_ahead_of_the_clock_counts_from_now() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    let now = archon_core::now();
    quota_wait::sweep_running(&h.ctx, now).await.unwrap();
    backdate_wait(&h, &stage, &reviewer, -TimeDelta::minutes(20)).await;
    quota_wait::sweep_running(&h.ctx, now).await.unwrap();
    let key = format!("quota_wait:{}:{reviewer}", stage.id);
    let saved = h.ctx.store.lock().await.kv_get(&key).unwrap();
    assert_eq!(saved, Some(now.to_rfc3339()), "the wait is saved from now");
    let due = now + TimeDelta::minutes(3);
    quota_wait::sweep_running(&h.ctx, due).await.unwrap();
    assert_eq!(told(&h, &lead).len(), 1);
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
    assert!(h.backend.cancelled().is_empty());
    assert_eq!(told(&h, &slow_lead).len(), 1, "its lead is told once");
    // Its check outlasted a period: the sweep due meanwhile passed it over.
    assert!(delay * 6 > period && delay * 6 < period * 2, "{delay:?}");
    assert_eq!(listings(), listed_before + 1, "one check, not two at once");
    watcher.abort();
}

/// A stage whose check fails on a wait mark that is no time keeps that
/// failure to itself: the sweeps go on, and another stage's due reviewer
/// is still reported.
#[tokio::test(start_paused = true)]
async fn a_check_that_fails_does_not_end_the_sweeps() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    let (broken_stage, _, broken) = held_reviewer(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL + Duration::from_secs(1)).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    let mark = format!("quota_wait:{}:{broken}", broken_stage.id);
    let marked = h.ctx.store.lock().await.kv_set(&mark, "no time");
    marked.expect("the mark is written");
    // The broken stage's check fails in the second sweep and is reaped in the third.
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL * 3).await;
    assert!(!watcher.is_finished(), "the sweeps go on");
    assert_eq!(told(&h, &lead).len(), 1);
    watcher.abort();
}

/// A request Aether leaves unanswered is cut off at the request limit, so a
/// later sweep checks its stage again: once Aether answers, the due reviewer
/// is reported after a later listing succeeds.
#[tokio::test(start_paused = true)]
async fn an_unanswered_request_is_cut_off_and_its_stage_checked_again() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL + Duration::from_secs(1)).await;
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    // The next sweep's listing hangs; Aether answers at once from then on.
    h.backend.slow_answers(&lead, Duration::from_secs(3600));
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL).await;
    assert!(told(&h, &lead).is_empty(), "the listing hangs");
    h.backend.slow_answers(&lead, Duration::ZERO);
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL * 2).await;
    assert_eq!(told(&h, &lead).len(), 1);
    watcher.abort();
}

/// The sweep keeps its own 30-second timer: with no dispatch pass at all, a
/// blocked reviewer is reported within minutes of the paused clock.
#[tokio::test(start_paused = true)]
async fn the_quota_timer_reports_a_block_without_any_dispatch() {
    let h = Harness::new().await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let watcher = tokio::spawn(quota_wait::watch(h.ctx.clone()));
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL + Duration::from_secs(1)).await;
    assert!(told(&h, &lead).is_empty(), "a fresh wait is kept");
    backdate_wait(&h, &stage, &reviewer, TimeDelta::minutes(3)).await;
    tokio::time::sleep(quota_wait::SWEEP_INTERVAL).await;
    assert_eq!(told(&h, &lead).len(), 1);
    watcher.abort();
}

/// Every stage is checked at once: stages checked before a due reviewer's,
/// whose reads hang, do not delay its report by their time limits.
#[tokio::test(start_paused = true)]
async fn slow_unrelated_stages_do_not_delay_a_due_reviewer() {
    let h = Harness::new().await;
    slow_stages(&h, 4, Duration::from_secs(3600)).await;
    let (stage, lead, reviewer) = held_reviewer(&h).await;
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
    let reported = || async { told(&h, &lead).len() == 1 };
    let took = advance_until(next_sweep + Duration::from_secs(2), reported).await;
    assert!(took <= next_sweep + Duration::from_secs(1), "{took:?}");
    watcher.abort();
}

/// With every read of its stage slow, though within the request limit, and
/// other stages not answering at all, a blocked reviewer is still reported
/// within five minutes and one sweep period of first being seen blocked.
#[tokio::test(start_paused = true)]
async fn a_blocked_reviewer_is_reported_in_time_though_every_read_is_slow() {
    reported_in_time(quota_wait::REQUEST_LIMIT - Duration::from_secs(1)).await;
}

/// An Aether that takes five seconds over every answer is slow, not silent:
/// its answers are read, and the blocked reviewer is reported in time.
#[tokio::test(start_paused = true)]
async fn a_blocked_reviewer_is_reported_in_time_when_every_answer_takes_five_seconds() {
    reported_in_time(Duration::from_secs(5)).await;
}

/// A reviewer whose stage answers every request after `slow`, beside stages
/// that never answer, is reported within five minutes and a sweep period.
async fn reported_in_time(slow: Duration) {
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
    let reported = || async { told(&h, &lead).len() == 1 };
    let took = advance_until(bound, reported).await;
    assert!(took >= Duration::from_secs(60), "not at once: {took:?}");
    watcher.abort();
}
