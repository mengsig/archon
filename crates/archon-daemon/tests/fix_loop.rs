//! One agent through the fix loop (ARCHITECTURE §6.7), on the built-in
//! `default-pr-review` pipeline: the implementer outlives its task and is
//! resumed with the reviewer's findings, reviewers stay fresh, and a fresh
//! implementer takes over only when the kept one cannot go on.

mod common;

use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::DescendantState;
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_core::steer::Steer;
use archon_daemon::executor;
use common::{Harness, MODEL, REVIEW_MODEL, choice, completed, expire_deadline};

const PIPELINE: &str = "default-pr-review";
const FINDINGS: &str = "the greeting is never tested";

/// The harness pipeline with an implementer that may delegate.
const DELEGATING: &str = r#"[pipeline]
name = "fix-delegating"
description = "an implementer with helpers"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
allow_delegation = true
timeout_minutes = 30

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 2
gate = "all_pass"
timeout_minutes = 30
on_fail = { goto = "implement", max_loops = 1 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
"#;

fn plan() -> ModelPlan {
    let mut plan = ModelPlan::default();
    let stages = [
        ("implement", choice(MODEL, Effort::High)),
        ("review", choice(REVIEW_MODEL, Effort::Medium)),
        ("ship", choice(MODEL, Effort::Low)),
    ];
    for (stage, choice) in stages {
        plan.0.insert(stage.into(), vec![choice]);
    }
    plan
}

fn review(pass: bool) -> StageOutcome {
    let (word, findings) = match pass {
        true => ("pass", "nothing to fix"),
        false => ("fail", FINDINGS),
    };
    StageOutcome::Completed {
        summary: format!("{findings}\nVERDICT: {word}"),
        structured: Some(serde_json::json!({"verdict": word, "findings": []})),
    }
}

/// The only running stage run, which must be of `stage`.
async fn running(h: &Harness, stage: &str) -> StageRun {
    let mut running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].stage_id, stage, "{running:?}");
    running.remove(0)
}

fn agent(stage: &StageRun) -> AetherAgentId {
    stage.aether_agent.clone().expect("a running stage's agent")
}

/// Enqueue and run `implement` to its end: its agent is now kept.
async fn implemented(h: &Harness, title: &str) -> (QueueItem, StageRun) {
    let item = h
        .enqueue_in(PIPELINE, title, plan())
        .await
        .expect("enqueue");
    h.dispatch().await;
    let first = running(h, "implement").await;
    h.settle_running(|_| completed("added the greeting in src/greet.rs"))
        .await;
    h.dispatch().await;
    (item, first)
}

/// The review fails with [`FINDINGS`] and the run goes back to `implement`.
async fn sent_back(h: &Harness) -> StageRun {
    running(h, "review").await;
    h.settle_running(|_| review(false)).await;
    h.dispatch().await;
    running(h, "implement").await
}

/// The kept agent ends the turn it was resumed in with `answer`.
async fn answers(h: &Harness, agent: &AetherAgentId, answer: &str) {
    h.backend.answer(agent, completed(answer));
    executor::turn_ended(&h.ctx, agent).await.expect("turn end");
    h.dispatch().await;
}

#[tokio::test]
async fn the_implementer_is_resumed_through_every_fix_and_reviewers_stay_fresh() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "add a greeting").await;
    let implementer = agent(&first);
    assert!(h.backend.requests()[0].keep, "created to outlive its task");

    let mut reviewers = BTreeSet::new();
    for attempt in 2..=3 {
        reviewers.insert(agent(&running(&h, "review").await));
        let fix = sent_back(&h).await;
        assert_eq!(fix.attempt, attempt);
        assert_eq!(
            agent(&fix),
            implementer,
            "the same agent fixes its own work"
        );
        assert_eq!(fix.aether_task, None, "resumed by message, not spawned");
        let told = h.backend.resumed_with(&fix);
        assert!(told.contains(FINDINGS), "{told}");
        assert!(told.contains(&format!("attempt {attempt}")), "{told}");
        answers(&h, &implementer, "added tests/greet.rs; cargo test passes").await;
        let fixed = &h.stages(&item.id).await;
        let fixed = fixed.iter().find(|s| s.id == fix.id).expect("the fix");
        assert_eq!(fixed.state, StageState::Completed);
        assert_eq!(
            fixed.summary.as_deref(),
            Some("added tests/greet.rs; cargo test passes")
        );
    }
    reviewers.insert(agent(&running(&h, "review").await));
    assert_eq!(reviewers.len(), 3, "every review is a fresh agent");
    assert!(!reviewers.contains(&implementer));

    h.settle_running(|_| review(true)).await;
    h.dispatch().await;
    running(&h, "ship").await;
    h.settle_running(|_| completed("opened the pull request"))
        .await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");

    let requests = h.backend.requests();
    let kept: Vec<bool> = requests.iter().map(|r| r.keep).collect();
    assert_eq!(kept, [true, false, false, false, false], "one implementer");
    assert_eq!(
        h.backend.cancelled(),
        h.backend.node_ids(),
        "the kept agent ends with its run's node"
    );
}

#[tokio::test]
async fn a_kept_agent_waits_parked_and_gets_its_stage_deadline_back() {
    let h = Harness::new().await;
    let (_, first) = implemented(&h, "park").await;
    let implementer = agent(&first);
    let parked: Vec<_> = h.backend.updates();
    assert_eq!(parked.len(), 1, "{parked:?}");
    let until = parked[0].1.deadline.expect("a parked agent's deadline");
    assert_eq!(parked[0].0, implementer);
    assert!(until > archon_core::now() + chrono::Duration::days(6));

    let fix = sent_back(&h).await;
    let updates = h.backend.updates();
    assert_eq!(updates.len(), 2, "{updates:?}");
    let deadline = updates[1].1.deadline.expect("the attempt's deadline");
    assert!(deadline < archon_core::now() + chrono::Duration::hours(3));
    assert_eq!(agent(&fix), implementer);
}

/// The turn-end event was missed: the sweep reads the answer.
#[tokio::test]
async fn a_resumed_attempt_settles_on_the_sweep() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "sweep").await;
    let fix = sent_back(&h).await;
    h.dispatch().await;
    assert_eq!(running(&h, "implement").await.id, fix.id, "still working");

    h.backend.answer(&agent(&first), completed("fixed"));
    h.dispatch().await;
    running(&h, "review").await;
    let stages = h.stages(&item.id).await;
    let fixed = stages.iter().find(|s| s.id == fix.id).expect("the fix");
    assert_eq!(fixed.state, StageState::Completed);
}

#[tokio::test]
async fn a_kept_agent_that_is_gone_is_replaced_with_findings_and_a_digest() {
    let h = Harness::new().await;
    let (_, first) = implemented(&h, "gone").await;
    h.backend.kill(&agent(&first));

    let fix = sent_back(&h).await;
    assert_ne!(agent(&fix), agent(&first), "a fresh implementer");
    assert!(fix.aether_task.is_some(), "spawned with a task of its own");
    assert!(h.backend.messages().is_empty(), "nothing sent to the dead");
    let fresh = h.backend.requests().pop().expect("the fresh implementer");
    assert!(fresh.keep, "kept in turn for the next loop");
    assert!(fresh.objective.contains(FINDINGS), "{}", fresh.objective);
    let digest = "`implement`, your stage's last attempt";
    assert!(fresh.objective.contains(digest), "{}", fresh.objective);
    assert!(
        fresh
            .objective
            .contains("added the greeting in src/greet.rs"),
        "{}",
        fresh.objective
    );
}

/// Aether refuses the message (a spent context, an ended agent): the kept
/// agent is stopped and a fresh one takes the attempt.
#[tokio::test]
async fn a_kept_agent_that_cannot_take_the_findings_is_stopped_and_replaced() {
    let h = Harness::new().await;
    let (_, first) = implemented(&h, "refused").await;
    let refusal = ArchonError::Aether("invalid_state: the agent's context is exhausted".into());
    h.backend.fail_next_message(refusal);

    let fix = sent_back(&h).await;
    assert_ne!(agent(&fix), agent(&first));
    assert!(h.backend.cancelled().contains(&agent(&first)), "stopped");
    let fresh = h.backend.requests().pop().expect("the fresh implementer");
    assert!(fresh.objective.contains(FINDINGS), "{}", fresh.objective);
}

/// Aether did not answer the resume: the attempt waits and the same message
/// goes out again under the same key, once.
#[tokio::test]
async fn an_unanswered_resume_is_repeated_under_its_key() {
    let h = Harness::new().await;
    let (_, first) = implemented(&h, "away").await;
    h.backend
        .fail_next_message(ArchonError::Unavailable("aether away".into()));
    running(&h, "review").await;
    h.settle_running(|_| review(false)).await;
    h.dispatch().await;
    let waiting = h.live_stages().await;
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0].state, StageState::Spawning, "not resumed yet");
    h.dispatch().await;

    let fix = running(&h, "implement").await;
    assert_eq!(agent(&fix), agent(&first), "resumed on the second try");
    let calls = h.backend.message_calls();
    assert_eq!(calls, [fix.id.to_string(), fix.id.to_string()]);
    assert_eq!(h.backend.messages().len(), 1, "delivered once");
    assert_eq!(h.backend.requests().len(), 2, "no fresh implementer");
}

/// The kept agent dies in the middle of its fix: an infrastructure failure,
/// so a fresh agent retries the attempt with the findings.
#[tokio::test]
async fn a_kept_agent_lost_mid_fix_is_retried_by_a_fresh_one() {
    let h = Harness::new().await;
    let (_, first) = implemented(&h, "lost").await;
    let fix = sent_back(&h).await;
    h.backend.kill(&agent(&first));
    h.dispatch().await;

    let retry = running(&h, "implement").await;
    assert_eq!(retry.attempt, fix.attempt, "the same attempt, retried");
    assert_ne!(agent(&retry), agent(&first));
    assert!(retry.aether_task.is_some());
    let fresh = h.backend.requests().pop().expect("the fresh implementer");
    assert!(fresh.objective.contains(FINDINGS), "{}", fresh.objective);
}

#[tokio::test]
async fn a_resumed_attempt_that_says_failed_fails_its_stage() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "cannot").await;
    sent_back(&h).await;
    let failed = StageOutcome::Failed {
        error: "the API this needs does not exist".into(),
        infra: false,
    };
    h.backend.answer(&agent(&first), failed);
    executor::turn_ended(&h.ctx, &agent(&first))
        .await
        .expect("turn end");
    let state = h.state(&item.id).await;
    let QueueState::Failed { error, .. } = &state else {
        panic!("{state:?}");
    };
    assert!(error.contains("does not exist"), "{error}");
}

#[tokio::test]
async fn a_resumed_attempt_past_its_deadline_is_stopped_as_timed_out() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "slow").await;
    let fix = sent_back(&h).await;
    expire_deadline(&h, &fix).await;
    h.dispatch().await;

    assert!(h.backend.cancelled().contains(&agent(&first)));
    let stages = h.stages(&item.id).await;
    let timed_out = stages.iter().find(|s| s.id == fix.id).expect("the fix");
    assert_eq!(timed_out.state, StageState::TimedOut);
}

#[tokio::test]
async fn cancelling_the_item_stops_the_resumed_agent() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "cancel").await;
    let fix = sent_back(&h).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("not needed".into()),
    };
    h.cli(cancel).await.expect("cancel");

    let cancelled = h.backend.cancelled();
    assert!(cancelled.contains(&agent(&first)), "{cancelled:?}");
    assert_eq!(cancelled.last(), h.backend.node_ids().first());
    let stages = h.stages(&item.id).await;
    let stopped = stages.iter().find(|s| s.id == fix.id).expect("the fix");
    assert_eq!(stopped.state, StageState::Cancelled);
    assert!(h.live_stages().await.is_empty());
}

/// What a kept agent delegated is stopped when its attempt settles, as for
/// any stage, and the agent itself stays to be resumed.
#[tokio::test]
async fn the_helpers_of_a_kept_agent_are_stopped_and_it_stays() {
    let h = Harness::new().await;
    h.add_pipeline("fix-delegating", DELEGATING);
    let mut plan = plan();
    let reviewer = plan.0["review"][0].clone();
    plan.0
        .insert("review".into(), vec![reviewer.clone(), reviewer]);
    let queued = h.enqueue_in("fix-delegating", "helpers", plan).await;
    let item = queued.expect("enqueue");
    h.dispatch().await;
    let first = running(&h, "implement").await;
    let helper = h.backend.delegate(&agent(&first), MODEL);
    h.settle_running(|_| completed("implemented")).await;
    let stopped = h.backend.cancelled();
    assert_eq!(stopped, std::slice::from_ref(&helper), "only the helper");
    h.backend.end_delegated(&helper, DescendantState::Stopped);
    h.dispatch().await;

    h.settle_running(|_| review(false)).await;
    h.dispatch().await;
    let fix = running(&h, "implement").await;
    assert_eq!(agent(&fix), agent(&first), "still there to be resumed");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

#[tokio::test]
async fn a_restart_mid_fix_keeps_the_agent_and_its_answer() {
    let mut h = Harness::new().await;
    let (item, first) = implemented(&h, "restart").await;
    let fix = sent_back(&h).await;
    let spawned = h.backend.requests().len();

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    let again = running(&h, "implement").await;
    assert_eq!((again.id.clone(), agent(&again)), (fix.id, agent(&first)));
    assert_eq!(h.backend.requests().len(), spawned, "nothing respawned");
    assert_eq!(h.backend.messages().len(), 1, "the findings went out once");

    answers(&h, &agent(&first), "fixed after the restart").await;
    running(&h, "review").await;
    let stages = h.stages(&item.id).await;
    let fixed = stages.iter().find(|s| s.id == again.id).expect("the fix");
    assert_eq!(fixed.summary.as_deref(), Some("fixed after the restart"));
}

const NOTE: &str = "Greet in the user's locale.";

async fn steer(h: &Harness, item: &QueueItem, steer: Steer) {
    let request = Request::SteerItem {
        item: item.id.clone(),
        steer,
    };
    h.cli(request).await.expect("steered");
}

/// The model patches Aether took, apart from the deadlines a kept agent is
/// parked and resumed with.
fn retunes(h: &Harness) -> Vec<(AetherAgentId, archon_core::model::ModelChoice)> {
    let updates = h.backend.updates().into_iter();
    updates
        .filter_map(|(agent, update)| Some((agent, update.choice?)))
        .collect()
}

/// A note steered in while the implementer waits parked is not sent to it
/// then: it comes with the findings it is resumed with.
#[tokio::test]
async fn a_note_steered_while_the_agent_is_parked_comes_with_the_findings() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "noted mid-review").await;
    let implementer = agent(&first);
    let reviewer = agent(&running(&h, "review").await);
    let note = Steer {
        note: Some(NOTE.into()),
        ..Steer::default()
    };
    steer(&h, &item, note).await;
    let told: Vec<AetherAgentId> = h.backend.messages().into_iter().map(|m| m.0).collect();
    assert_eq!(
        told,
        [reviewer],
        "only the agent working now is told at once"
    );

    let fix = sent_back(&h).await;
    assert_eq!(agent(&fix), implementer, "the kept agent takes the fix");
    let resumed = h.backend.resumed_with(&fix);
    assert!(resumed.contains(FINDINGS), "{resumed}");
    assert!(resumed.contains(NOTE), "{resumed}");
}

/// A resumed attempt settles on its turn's last word, so a steer leaves the
/// turn to end: the agent reads the note in it, and its answer still counts.
#[tokio::test]
async fn a_steer_into_a_resumed_fix_ends_no_turn_and_keeps_its_answer() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "steered mid-fix").await;
    let implementer = agent(&first);
    let fix = sent_back(&h).await;
    let asked = Steer {
        note: Some(NOTE.into()),
        effort: Some(Effort::Low),
        ..Steer::default()
    };
    steer(&h, &item, asked).await;
    assert!(
        h.backend.interrupts().is_empty(),
        "the resumed turn is left to end"
    );
    assert_eq!(
        retunes(&h),
        [(implementer.clone(), choice(MODEL, Effort::Low))]
    );
    let last = h.backend.messages().pop().expect("the steer's message");
    assert_eq!(last.0, implementer);
    assert!(last.1.contains(NOTE), "{}", last.1);

    answers(&h, &implementer, "localised the greeting; tests pass").await;
    let stages = h.stages(&item.id).await;
    let fixed = stages.iter().find(|s| s.id == fix.id).expect("the fix");
    assert_eq!(fixed.state, StageState::Completed);
    assert_eq!(
        fixed.summary.as_deref(),
        Some("localised the greeting; tests pass")
    );
    running(&h, "review").await;
}

/// Once a steer plans another model for the stage, the fix goes to a fresh
/// agent on it, with the findings and a digest of the attempt it continues.
#[tokio::test]
async fn a_model_steered_for_the_stage_gives_the_fix_to_a_fresh_agent() {
    let h = Harness::new().await;
    let (item, first) = implemented(&h, "a stronger model for the fix").await;
    let implementer = agent(&first);
    running(&h, "review").await;
    let asked = Steer {
        stage: Some("implement".into()),
        model: Some(REVIEW_MODEL.parse().unwrap()),
        ..Steer::default()
    };
    steer(&h, &item, asked).await;
    assert!(retunes(&h).is_empty(), "no agent of that stage works now");

    let fix = sent_back(&h).await;
    assert_ne!(agent(&fix), implementer, "a fresh agent on the new model");
    assert!(fix.aether_task.is_some(), "spawned, not resumed");
    assert_eq!(fix.choice, choice(REVIEW_MODEL, Effort::High));
    let request = h.backend.requests().pop().expect("the fix's request");
    assert_eq!(request.choice, fix.choice);
    assert!(
        request.objective.contains(FINDINGS),
        "{}",
        request.objective
    );
    assert!(
        request
            .objective
            .contains("added the greeting in src/greet.rs"),
        "the digest of the attempt it continues: {}",
        request.objective
    );
    assert!(
        h.backend.cancelled().contains(&implementer),
        "the kept agent is stopped"
    );
}
