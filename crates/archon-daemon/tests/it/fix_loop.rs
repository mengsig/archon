//! One agent through the fix loop (ARCHITECTURE §6.7), on a repository
//! pipeline shaped like 1.0's `default-pr-review`: the implementer outlives its task and is
//! resumed with the reviewer's findings, reviewers stay fresh, and a fresh
//! implementer takes over only when the kept one cannot go on. archond ships
//! the run (§8.7) from the ship record the implementer's last answer names.

use crate::common;

use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::{DescendantState, resumed_reply_outcome};
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, StageOutcome, StageRun, StageState};
use archon_core::steer::Steer;
use archon_daemon::executor;
use common::delivery::{FakeGh, add_origin, change, origin_of, pushed, shipped_by};
use common::{Harness, MODEL, REVIEW_MODEL, REVIEWED, REVIEWED_TOML, choice, expire_deadline, git};

const FINDINGS: &str = "the greeting is never tested";

/// The harness pipeline with an implementer that may delegate.
const DELEGATING: &str = r#"[pipeline]
name = "fix-delegating"
description = "an implementer with helpers"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.ship]
stage = "implement"

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
"#;

/// A harness whose repository has [`REVIEWED`].
async fn harness() -> Harness {
    let h = Harness::new().await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
    h
}

fn plan() -> ModelPlan {
    let mut plan = ModelPlan::default();
    let stages = [
        ("implement", choice(MODEL, Effort::Medium)),
        ("review", choice(REVIEW_MODEL, Effort::Medium)),
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

/// The running `implement` attempt completes its task with `summary`: a
/// change committed in the run's worktree and the ship record that names it.
async fn settle_recorded(h: &Harness, summary: &str) -> StageRun {
    let first = running(h, "implement").await;
    let outcome = shipped_by(h, &first, summary, serde_json::json!({})).await;
    h.settle_running(|_| outcome.clone()).await;
    first
}

/// Enqueue and run `implement` to its end: its agent is now kept.
async fn implemented(h: &Harness, title: &str) -> (QueueItem, StageRun) {
    let item = h
        .enqueue_in(REVIEWED, title, plan())
        .await
        .expect("enqueue");
    h.dispatch().await;
    let first = settle_recorded(h, "added the greeting in src/greet.rs").await;
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

/// The last message of the agent resumed for `fix`: `summary`, then the ship
/// record of its fix, read as Aether's answer would be.
async fn fixed(h: &Harness, fix: &StageRun, summary: &str) -> StageOutcome {
    let run = h.ctx.store.lock().await.run(&fix.run).expect("run");
    let record = change(&run.workspace);
    let outcome = resumed_reply_outcome(&format!("{summary}\n\n```json\n{record}\n```"));
    assert!(
        matches!(&outcome, StageOutcome::Completed { structured, .. } if structured.as_ref() == Some(&record)),
        "{outcome:?}"
    );
    outcome
}

/// The kept agent ends the turn it was resumed in for `fix` with `answer`.
async fn answers(h: &Harness, fix: &StageRun, answer: &str) {
    let outcome = fixed(h, fix, answer).await;
    h.backend.answer(&agent(fix), outcome);
    let ended = executor::turn_ended(&h.ctx, &agent(fix)).await;
    ended.expect("turn end");
    h.dispatch().await;
}

/// The whole loop to its delivery: the parked implementer neither holds the
/// delivery back nor outlives its run, and what ships is its last fix.
#[tokio::test]
async fn the_implementer_is_resumed_through_every_fix_and_reviewers_stay_fresh() {
    let h = harness().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
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
        answers(&h, &fix, "added tests/greet.rs; cargo test passes").await;
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
    let run = h.run(&item.id).await;
    let head = git(&h.repo, &["rev-parse", &run.branch]);
    let state = h.state(&item.id).await;
    let QueueState::Succeeded { summary, .. } = &state else {
        panic!("archond ships with the implementer parked: {state:?}");
    };
    let shipped = format!("SHIPPED: https://github.com/o/r/pull/1 (commit {head})");
    assert!(summary.ends_with(&shipped), "{summary}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(head));
    assert_eq!(gh.count("pr create"), 1, "{:?}", gh.calls());

    let requests = h.backend.requests();
    let kept: Vec<bool> = requests.iter().map(|r| r.keep).collect();
    assert_eq!(kept, [true, false, false, false], "one implementer");
    assert_eq!(
        h.backend.cancelled(),
        h.backend.node_ids(),
        "the kept agent ends with its run's node"
    );
    learned_as_three_attempts(&h, &run).await;
}

/// What is learned counts the one kept agent's three turns as three attempts
/// of its stage, each reviewed, in a run that is a win.
async fn learned_as_three_attempts(h: &Harness, run: &PipelineRun) {
    let outcome = h.ctx.store.lock().await.outcome(&run.id).unwrap();
    let outcome = outcome.expect("captured as the run finished");
    let attempts = |id: &str| {
        outcome
            .stages
            .iter()
            .find(|s| s.stage == id)
            .map(|s| s.attempts)
    };
    assert_eq!(attempts("implement"), Some(3), "{outcome:?}");
    assert_eq!(outcome.review_rounds, 3);
    assert!(outcome.is_win(), "{outcome:?}");
}

#[tokio::test]
async fn a_kept_agent_waits_parked_and_gets_its_stage_deadline_back() {
    let h = harness().await;
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

/// Aether's cap counts the kept agent's whole life: resumed, it may spend
/// what it spent so far plus the new attempt's grant, not the old remainder.
#[tokio::test]
async fn a_resumed_agent_gets_its_spend_so_far_plus_a_new_grant() {
    let h = harness().await;
    let item = h
        .enqueue_in(REVIEWED, "capped", plan())
        .await
        .expect("enqueue");
    let patch = archon_core::protocol::ItemPatch {
        budget_usd: Some(10.0),
        ..Default::default()
    };
    h.cli(Request::UpdateItem {
        item: item.id.clone(),
        patch,
    })
    .await
    .expect("a budget");
    h.dispatch().await;
    let first = running(&h, "implement").await;
    let key = |stage: &StageRun| format!("spawn_request:{}", stage.id);
    let mut spawn: serde_json::Value =
        serde_json::from_str(&h.kv(&key(&first)).await.expect("stored")).unwrap();
    assert_eq!(spawn["request"]["cost_budget_usd"], 10.0);
    // As when Aether enforces the cap it was created with.
    spawn["request"]["aether_cost_budget"] = true.into();
    let raw = spawn.to_string();
    h.ctx.store.lock().await.kv_set(&key(&first), &raw).unwrap();
    settle_recorded(&h, "added the greeting").await;
    let row = archon_queue::store::SpendRow {
        agent: agent(&first),
        item: Some(item.id.clone()),
        cost_usd: 7.5,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: 0.25,
        input_tokens: 10,
        output_tokens: 5,
        at: archon_core::now(),
    };
    h.ctx.store.lock().await.record_spend(&row).unwrap();
    h.dispatch().await;

    let fix = sent_back(&h).await;
    assert_eq!(agent(&fix), agent(&first), "resumed");
    let stored: serde_json::Value =
        serde_json::from_str(&h.kv(&key(&fix)).await.expect("stored")).unwrap();
    let grant = stored["request"]["cost_budget_usd"]
        .as_f64()
        .expect("a grant");
    let updates = h.backend.updates();
    let resumed = &updates.last().expect("the resume's update").1;
    assert_eq!(resumed.cost_budget_usd, Some(7.5 + grant), "{updates:?}");
    assert!(resumed.deadline.is_some(), "and its stage deadline");
}

/// The turn-end event was missed: the sweep reads the answer.
#[tokio::test]
async fn a_resumed_attempt_settles_on_the_sweep() {
    let h = harness().await;
    let (item, first) = implemented(&h, "sweep").await;
    let fix = sent_back(&h).await;
    h.dispatch().await;
    assert_eq!(running(&h, "implement").await.id, fix.id, "still working");

    let answer = fixed(&h, &fix, "fixed").await;
    h.backend.answer(&agent(&first), answer);
    h.dispatch().await;
    running(&h, "review").await;
    let stages = h.stages(&item.id).await;
    let fixed = stages.iter().find(|s| s.id == fix.id).expect("the fix");
    assert_eq!(fixed.state, StageState::Completed);
}

#[tokio::test]
async fn a_kept_agent_that_is_gone_is_replaced_with_findings_and_a_digest() {
    let h = harness().await;
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
    let h = harness().await;
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
    let h = harness().await;
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
    let h = harness().await;
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
    let h = harness().await;
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

/// The kept implementer ends its fix with `summary` and no ship record; it is
/// asked once for the record, and that message is returned.
async fn ends_without_its_record(h: &Harness, fix: &StageRun, summary: &str) -> String {
    let before = h.backend.messages().len();
    h.backend
        .answer(&agent(fix), resumed_reply_outcome(summary));
    let ended = executor::turn_ended(&h.ctx, &agent(fix)).await;
    ended.expect("turn end");
    h.dispatch().await;
    let messages = h.backend.messages();
    assert_eq!(messages.len(), before + 1, "asked once: {messages:?}");
    let (to, asked) = messages.last().cloned().expect("the request");
    assert_eq!(to, agent(fix));
    asked
}

#[tokio::test]
async fn a_resumed_agent_that_leaves_out_its_record_is_asked_for_it_once() {
    let h = harness().await;
    let (item, _) = implemented(&h, "forgot the record").await;
    let fix = sent_back(&h).await;
    let asked = ends_without_its_record(&h, &fix, "added the test in tests/greet.rs").await;
    assert!(asked.contains("it has no fenced `json` block"), "{asked}");
    assert!(asked.contains("\"commit\""), "it shows the record: {asked}");
    assert_eq!(running(&h, "implement").await.id, fix.id, "the fix goes on");

    answers(&h, &fix, "Here is the record.").await;
    let stages = h.stages(&item.id).await;
    let done = stages.iter().find(|s| s.id == fix.id).expect("the fix");
    assert_eq!(done.state, StageState::Completed);
    assert_eq!(
        done.summary.as_deref(),
        Some("added the test in tests/greet.rs"),
        "the summary is the fix's, not the record's"
    );
    running(&h, "review").await;
}

#[tokio::test]
async fn a_resumed_agent_asked_for_its_record_in_vain_fails_its_stage() {
    let h = harness().await;
    let (item, _) = implemented(&h, "never records").await;
    let fix = sent_back(&h).await;
    ends_without_its_record(&h, &fix, "added the test").await;
    h.backend
        .answer(&agent(&fix), resumed_reply_outcome("I committed it."));
    let ended = executor::turn_ended(&h.ctx, &agent(&fix)).await;
    ended.expect("turn end");
    h.dispatch().await;
    let state = h.state(&item.id).await;
    let QueueState::Failed { error, .. } = &state else {
        panic!("{state:?}");
    };
    assert!(
        error.contains("ship record archond delivers is not on record"),
        "{error}"
    );
    assert!(
        error.contains("added the test"),
        "the fix's own summary: {error}"
    );
}

#[tokio::test]
async fn a_resumed_attempt_past_its_deadline_is_stopped_as_timed_out() {
    let h = harness().await;
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
    let h = harness().await;
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
    let h = harness().await;
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
    settle_recorded(&h, "implemented").await;
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
    let mut h = harness().await;
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

    answers(&h, &again, "fixed after the restart").await;
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

/// Retuned while it ran, the kept agent runs on its new choice: the next
/// fix resumes it when the plan asks for that choice, and not otherwise.
#[tokio::test]
async fn a_kept_agent_is_resumed_only_on_the_choice_it_runs_now() {
    for (back_to_high, resumed) in [(false, true), (true, false)] {
        let h = harness().await;
        let (item, first) = implemented(&h, "retuned mid-fix").await;
        let implementer = agent(&first);
        let fix = sent_back(&h).await;
        let lower = Steer {
            effort: Some(Effort::Low),
            ..Steer::default()
        };
        steer(&h, &item, lower).await;
        let low = choice(MODEL, Effort::Low);
        assert_eq!(retunes(&h), [(implementer.clone(), low.clone())]);
        answers(&h, &fix, "localised the greeting; tests pass").await;
        running(&h, "review").await;
        if back_to_high {
            let higher = Steer {
                stage: Some("implement".into()),
                effort: Some(Effort::Medium),
                ..Steer::default()
            };
            steer(&h, &item, higher).await;
            assert_eq!(retunes(&h).len(), 1, "no agent of that stage works now");
        }

        let again = sent_back(&h).await;
        let planned = if back_to_high {
            choice(MODEL, Effort::Medium)
        } else {
            low
        };
        assert_eq!(again.choice, planned);
        assert_eq!(
            agent(&again) == implementer,
            resumed,
            "back to high: {back_to_high}"
        );
        assert_eq!(again.aether_task.is_none(), resumed, "resumed, not spawned");
    }
}

/// A note steered in while the implementer waits parked is not sent to it
/// then: it comes with the findings it is resumed with.
#[tokio::test]
async fn a_note_steered_while_the_agent_is_parked_comes_with_the_findings() {
    let h = harness().await;
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
    let h = harness().await;
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

    answers(&h, &fix, "localised the greeting; tests pass").await;
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
    let h = harness().await;
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
    assert_eq!(fix.choice, choice(REVIEW_MODEL, Effort::Medium));
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

#[tokio::test]
async fn resumed_attempts_retain_the_created_agents_host_context() {
    use archon_core::config::{Config, HostContext};
    use archon_core::ports::AgentBackend;
    let mut config = Config::default();
    config.aether.host_context = HostContext::Lean;
    let h = Harness::with_config(config).await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
    h.backend.set_host_context_supported(true);
    let (_item, first) = implemented(&h, "retain the host context").await;
    assert_eq!(first.host_context, HostContext::Lean);
    h.backend.set_host_context_supported(false);
    let fix = sent_back(&h).await;
    assert_eq!(agent(&fix), agent(&first));
    assert_eq!(fix.host_context, HostContext::Lean);
    assert_eq!(
        h.backend.host_context(HostContext::Lean).await.unwrap(),
        HostContext::Full
    );
}
