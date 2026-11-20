//! Steering an item without cancelling it (ARCHITECTURE §6.6): a note, a
//! model, a budget and more time reach the agents working now; a stage is
//! aborted, removed or inserted and the pipeline switched for the stages
//! not yet started; and a failed item is revived on its branch.

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{AutonomyPatch, ProposalMode};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::Effort;
use archon_core::notify::{NotificationKind, Question};
use archon_core::plan_origin::{Authority, PlanSource};
use archon_core::ports::{AgentUpdate, DescendantState};
use archon_core::protocol::{
    EnqueueRequest, ItemDetail, ItemPatch, ItemQuery, Reply, RepoRef, Request, StageQuery,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{RunState, StageOutcome, StageRun, StageState};
use archon_core::steer::{InsertStage, Steer, SteeredBy, Then};
use common::{Harness, MODEL, PIPELINE, REPO_NAME, REVIEW_MODEL, choice, completed, plan, verdict};

const NOTE: &str = "Use the v2 client, not the v1 one.";
/// Where the `propose` stage leaves the page its gate shows.
const PROPOSAL_PAGE: &str = ".archon/reports/proposal/index.html";

/// The pipeline steers borrow stage `verify` from.
const EXTRA: &str = r#"[pipeline]
name = "steer-extra"
description = "a stage to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "verify"
kind = "work"
prompt = "verify.md"
timeout_minutes = 30
"#;
const VERIFY_PROMPT: &str =
    "Verify the change for {{task.title}} by running it.\n\n{{task.brief}}\n";

/// The pipeline steers switch to: no review between the work and the ship.
const QUICK: &str = r#"[pipeline]
name = "steer-quick"
description = "no review"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30

[[stage]]
id = "ship"
kind = "ship"
prompt = "quick-ship.md"
timeout_minutes = 90
"#;
const QUICK_SHIP_PROMPT: &str =
    "Ship {{task.title}} unreviewed: {{previous.implement.summary}}\n\n{{task.brief}}\n";

async fn harness() -> Harness {
    harness_with(archon_core::config::Config::default()).await
}

async fn harness_with(config: archon_core::config::Config) -> Harness {
    let h = Harness::with_config(config).await;
    let pipelines = h.repo.join(".archon/pipelines");
    h.add_pipeline("steer-extra", EXTRA);
    std::fs::write(pipelines.join("steer-extra/verify.md"), VERIFY_PROMPT).expect("prompt");
    h.add_pipeline("steer-quick", QUICK);
    std::fs::write(
        pipelines.join("steer-quick/quick-ship.md"),
        QUICK_SHIP_PROMPT,
    )
    .expect("prompt");
    h
}

fn note(text: &str) -> Steer {
    Steer {
        note: Some(text.into()),
        ..Steer::default()
    }
}

fn abort(stage: Option<&str>, then: Then) -> Steer {
    Steer {
        stage: stage.map(str::to_owned),
        abort: Some(then),
        ..Steer::default()
    }
}

fn insert(stage: &str, after: Option<&str>) -> Steer {
    Steer {
        insert_stage: Some(InsertStage {
            stage: stage.into(),
            from: Some("steer-extra".into()),
            after: after.map(str::to_owned),
        }),
        ..Steer::default()
    }
}

fn switch(pipeline: &str) -> Steer {
    Steer {
        switch_pipeline: Some(pipeline.into()),
        ..Steer::default()
    }
}

async fn steer(h: &Harness, item: &QueueItemId, steer: Steer) -> Result<QueueItem, ArchonError> {
    let request = Request::SteerItem {
        item: item.clone(),
        steer,
    };
    match h.cli(request).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("steer_item replied {other:?}"),
    }
}

/// The refusal a steer gets, as the operator reads it.
async fn refused(h: &Harness, item: &QueueItemId, asked: Steer) -> String {
    let before = h.item(item).await;
    let error = steer(h, item, asked).await.expect_err("refused");
    assert_eq!(
        h.item(item).await,
        before,
        "a refused steer changes nothing"
    );
    error.to_string()
}

async fn detail(h: &Harness, item: &QueueItemId) -> ItemDetail {
    match h.cli(Request::GetItem { item: item.clone() }).await {
        Ok(Reply::ItemDetail(detail)) => *detail,
        other => panic!("get_item replied {other:?}"),
    }
}

/// The pipeline `get_item` and `list_item_summaries` say a steer switched
/// the item to; they must agree.
async fn switched_to(h: &Harness, item: &QueueItemId) -> Option<String> {
    let shown = detail(h, item).await.switched_to;
    let query = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    let Ok(Reply::ItemSummaries(rows)) = h.cli(Request::ListItemSummaries(query)).await else {
        panic!("list_item_summaries failed");
    };
    let row = rows.into_iter().find(|row| &row.item.id == item).unwrap();
    assert_eq!(row.switched_to, shown, "the list and the detail agree");
    shown
}

fn agent(stage: &StageRun) -> AetherAgentId {
    stage.aether_agent.clone().expect("a running stage's agent")
}

/// The running stage runs, which must all be of `stage`.
async fn running(h: &Harness, stage: &str) -> Vec<StageRun> {
    let running = h.running().await;
    assert!(!running.is_empty(), "nothing runs; expected {stage}");
    assert!(running.iter().all(|s| s.stage_id == stage), "{running:?}");
    running
}

/// Enqueue on the harness pipeline and start `implement`.
async fn implementing(h: &Harness, title: &str) -> (QueueItem, StageRun) {
    let item = h.enqueue(title).await;
    h.dispatch().await;
    let working = running(h, "implement").await.remove(0);
    (item, working)
}

/// Enqueue, finish `implement`, and start both reviewers.
async fn reviewing(h: &Harness, title: &str) -> (QueueItem, Vec<StageRun>) {
    let (item, _) = implementing(h, title).await;
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviewers = running(h, "review").await;
    assert_eq!(reviewers.len(), 2);
    (item, reviewers)
}

/// Settle what runs and dispatch, until nothing runs; the stages in the
/// order they ran.
async fn run_out(h: &Harness, item: &QueueItemId) -> Vec<String> {
    for _ in 0..12 {
        if h.running().await.is_empty() {
            break;
        }
        h.settle_running(|stage| match stage.stage_id.as_str() {
            "review" => verdict(true),
            _ => completed("done"),
        })
        .await;
        h.dispatch().await;
    }
    let stages = h.stages(item).await;
    stages.into_iter().map(|s| s.stage_id).collect()
}

#[tokio::test]
async fn a_note_and_a_new_model_reach_the_running_agent_without_a_cancel() {
    let h = harness().await;
    let (item, working) = implementing(&h, "add a greeting").await;
    let lead = agent(&working);
    let asked = Steer {
        model: Some(REVIEW_MODEL.parse().unwrap()),
        effort: Some(Effort::Low),
        ..note(NOTE)
    };
    let steered = steer(&h, &item.id, asked).await.expect("steered");

    let new = choice(REVIEW_MODEL, Effort::Low);
    assert!(
        steered.brief.ends_with(&format!("{NOTE}\n")),
        "{}",
        steered.brief
    );
    assert_eq!(
        steered.model_plan.choices_for("implement"),
        Some(std::slice::from_ref(&new))
    );
    let origin = &crate::plan_origins::shown(&h, &steered).await["implement"][0];
    for field in [&origin.model, &origin.effort] {
        assert_eq!(
            (field.source, field.by),
            (PlanSource::Explicit, Authority::Operator)
        );
    }
    assert_eq!(steered.state, item_running(&h, &item.id).await);
    assert!(h.backend.cancelled().is_empty(), "no agent is cancelled");
    assert_eq!(h.backend.created(), 1, "no agent is spawned");
    let patch = AgentUpdate {
        choice: Some(new),
        ..AgentUpdate::default()
    };
    assert_eq!(h.backend.updates(), [(lead.clone(), patch)]);
    assert_eq!(
        h.backend.interrupts(),
        std::slice::from_ref(&lead),
        "the new model runs now"
    );
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, lead);
    assert!(messages[0].1.contains(NOTE), "{}", messages[0].1);
    assert!(messages[0].1.contains(REVIEW_MODEL), "{}", messages[0].1);

    let still = running(&h, "implement").await.remove(0);
    assert_eq!(still.id, working.id, "the same attempt goes on");
    assert_eq!(agent(&still), lead, "with the same agent");
    assert_eq!(
        still.choice, working.choice,
        "the initial choice stays immutable"
    );
    assert_eq!(still.current_choice(), &choice(REVIEW_MODEL, Effort::Low));
    assert_eq!(still.model_segments.len(), 1);
    let query = StageQuery {
        item: item.id.clone(),
        run: None,
        stage: Some("implement".into()),
        attempt: None,
        slot: None,
        offset: 0,
        limit: None,
    };
    let Ok(Reply::StageDetail(shown)) = h.cli(Request::GetStage(query)).await else {
        panic!("the stage is shown");
    };
    assert_eq!(
        shown.stages[0].choice,
        *still.current_choice(),
        "shown on the choice its agent runs now"
    );
    h.settle_running(|_| completed("implemented")).await;
    h.dispatch().await;
    let reviewers = running(&h, "review").await;
    assert_eq!(reviewers.len(), 2);
    let asked_of = h.backend.requests().pop().expect("a reviewer's request");
    assert!(
        asked_of.objective.contains(NOTE),
        "later prompts read the amended brief"
    );
}

async fn item_running(h: &Harness, item: &QueueItemId) -> QueueState {
    let run = h.run(item).await;
    assert!(!run.state.is_terminal(), "the run is still live");
    QueueState::Running { run: run.id }
}

#[tokio::test]
async fn a_note_reaches_every_live_agent_beneath_the_running_one() {
    let h = harness().await;
    let (item, working) = implementing(&h, "with helpers").await;
    let lead = agent(&working);
    let helper = h.backend.delegate(&lead, MODEL);
    let nested = h.backend.delegate(&helper, MODEL);
    let ended = h.backend.delegate(&lead, MODEL);
    h.backend.end_delegated(&ended, DescendantState::Completed);

    steer(&h, &item.id, note(NOTE)).await.expect("steered");
    let mut told: Vec<AetherAgentId> = h.backend.messages().into_iter().map(|m| m.0).collect();
    told.sort();
    let mut expected = vec![lead, helper, nested];
    expected.sort();
    assert_eq!(
        told, expected,
        "the lead and its live subtree, not an ended helper"
    );
    assert!(h.backend.interrupts().is_empty(), "a note ends no turn");
    assert!(h.backend.updates().is_empty(), "a note patches nothing");
}

#[tokio::test]
async fn a_note_reaches_both_reviewers_and_the_next_stage() {
    let h = harness().await;
    let (item, reviewers) = reviewing(&h, "review under way").await;
    steer(&h, &item.id, note(NOTE)).await.expect("steered");
    let mut told: Vec<AetherAgentId> = h.backend.messages().into_iter().map(|m| m.0).collect();
    told.sort();
    let mut expected: Vec<AetherAgentId> = reviewers.iter().map(agent).collect();
    expected.sort();
    assert_eq!(told, expected);

    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    running(&h, "ship").await;
    let ship = h.backend.requests().pop().expect("the ship request");
    assert!(ship.objective.contains(NOTE), "{}", ship.objective);
}

#[tokio::test]
async fn every_steer_is_recorded_with_who_made_it() {
    let h = harness().await;
    let (item, _) = implementing(&h, "audited").await;
    steer(&h, &item.id, note(NOTE)).await.expect("the operator");
    let by_orchestrator = Request::SteerItem {
        item: item.id.clone(),
        steer: Steer {
            effort: Some(Effort::Low),
            ..Steer::default()
        },
    };
    h.orchestrator(by_orchestrator)
        .await
        .expect("the orchestrator");

    let steers = detail(&h, &item.id).await.steers;
    assert_eq!(steers.len(), 2, "{steers:?}");
    assert_eq!(steers[0].by, SteeredBy::Operator);
    assert_eq!(steers[0].what, "amended the brief");
    assert_eq!(steers[0].note.as_deref(), Some(NOTE));
    assert_eq!(steers[1].by, SteeredBy::Orchestrator);
    assert!(
        steers[1].what.contains("stage `implement` now runs on"),
        "{steers:?}"
    );
    assert!(steers[0].at <= steers[1].at);
    let feed = h.notices(NotificationKind::Progress).await;
    let lines: Vec<&String> = feed.iter().filter(|n| n.starts_with("Steered")).collect();
    assert_eq!(lines.len(), 2, "{feed:?}");
    assert!(
        lines[0].contains("(operator)") && lines[0].contains(NOTE),
        "{lines:?}"
    );
    assert!(lines[1].contains("(orchestrator)"), "{lines:?}");
}

#[tokio::test]
async fn steers_arriving_together_are_each_delivered_once_and_never_panic() {
    let h = harness().await;
    let mut working = Vec::new();
    for title in ["first", "second", "third"] {
        let item = h.enqueue(title).await;
        h.dispatch().await;
        working.push(item);
    }
    let stages = running(&h, "implement").await;
    assert_eq!(stages.len(), 3);

    let notes: Vec<String> = (0..4).map(|n| format!("note {n}")).collect();
    let mut steers = Vec::new();
    for item in &working {
        for text in &notes {
            steers.push(steer(&h, &item.id, note(text)));
        }
    }
    // The sweep runs beside the steers, as it does in the daemon.
    let sweeps = (0..6).map(|_| h.dispatch());
    let (results, _) = tokio::join!(
        futures_util::future::join_all(steers),
        futures_util::future::join_all(sweeps)
    );
    for result in results {
        result.expect("steered");
    }
    h.dispatch().await;

    let told = h.backend.messages();
    for stage in &stages {
        for text in &notes {
            let count = told
                .iter()
                .filter(|(to, said)| *to == agent(stage) && said.contains(text.as_str()))
                .count();
            assert_eq!(count, 1, "{text} reached {} once", stage.id);
        }
        assert_eq!(h.kv(&format!("steer_owed:{}", stage.id)).await, None);
    }
}

#[tokio::test]
async fn a_steer_aether_did_not_take_is_delivered_by_the_sweep_exactly_once() {
    let h = harness().await;
    let (item, working) = implementing(&h, "aether is away").await;
    h.backend
        .fail_next_message(ArchonError::Unavailable("aetherd is restarting".into()));
    let steered = steer(&h, &item.id, note(NOTE))
        .await
        .expect("stored anyway");
    assert!(steered.brief.contains(NOTE));
    assert!(h.backend.messages().is_empty(), "not delivered yet");
    let owed = format!("steer_owed:{}", working.id);
    assert!(h.kv(&owed).await.is_some(), "the delivery is owed durably");

    h.dispatch().await;
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, agent(&working));
    assert_eq!(h.kv(&owed).await, None, "nothing is owed any more");
    h.dispatch().await;
    assert_eq!(
        h.backend.messages().len(),
        1,
        "a made delivery is not repeated"
    );
    let calls = h.backend.message_calls();
    assert_eq!(calls.len(), 2, "one refused call, one delivered");
    assert_eq!(calls[0], calls[1], "the retry repeats the delivery's key");
}

#[tokio::test]
async fn a_patch_aether_refuses_is_reported_and_kept_for_the_next_agents() {
    let h = harness().await;
    let (item, working) = implementing(&h, "refused patch").await;
    h.backend.fail_next_update(ArchonError::Invalid(
        "aether: model not allowed here".into(),
    ));
    let asked = Steer {
        effort: Some(Effort::Low),
        ..Steer::default()
    };
    let steered = steer(&h, &item.id, asked)
        .await
        .expect("the plan still changes");
    let low = choice(MODEL, Effort::Low);
    assert_eq!(
        steered.model_plan.choices_for("implement"),
        Some(&[low][..])
    );
    assert!(h.backend.updates().is_empty());
    assert!(
        h.backend.interrupts().is_empty(),
        "no turn ends for a refused patch"
    );
    let feed = h.notices(NotificationKind::Progress).await;
    let told = feed
        .iter()
        .find(|n| n.contains("did not reach the running agent"));
    assert!(
        told.is_some_and(|n| n.contains("model not allowed here")),
        "{feed:?}"
    );
    assert_eq!(h.kv(&format!("steer_owed:{}", working.id)).await, None);
    assert_eq!(running(&h, "implement").await[0].id, working.id);
}

#[tokio::test]
async fn a_new_budget_and_more_time_reach_the_running_agent() {
    let h = harness().await;
    let item = h.enqueue("capped").await;
    let capped = Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            budget_usd: Some(10.0),
            ..ItemPatch::default()
        },
    };
    h.cli(capped).await.expect("a budget before the run");
    h.dispatch().await;
    let working = running(&h, "implement").await.remove(0);
    let key = format!("spawn_request:{}", working.id);
    let stored = |raw: Option<String>| -> serde_json::Value {
        serde_json::from_str(&raw.expect("a stored spawn")).expect("json")
    };
    // As when Aether enforces the cap it was sent.
    let mut spawn = stored(h.kv(&key).await);
    assert_eq!(spawn["request"]["cost_budget_usd"], 10.0);
    spawn["request"]["aether_cost_budget"] = true.into();
    let before: archon_core::Timestamp =
        serde_json::from_value(spawn["request"]["deadline"].clone()).expect("a deadline");
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &spawn.to_string())
        .expect("kv");

    let asked = Steer {
        budget_usd: Some(25.0),
        extend_minutes: Some(45),
        ..Steer::default()
    };
    let steered = steer(&h, &item.id, asked).await.expect("steered");
    assert_eq!(steered.budget_usd, Some(25.0));
    let updates = h.backend.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0].0, agent(&working));
    assert_eq!(
        updates[0].1.cost_budget_usd,
        Some(25.0),
        "ten granted plus fifteen"
    );
    let later = updates[0].1.deadline.expect("a new deadline");
    assert_eq!(later, before + chrono::Duration::minutes(45));
    assert_eq!(updates[0].1.choice, None);
    assert!(h.backend.interrupts().is_empty() && h.backend.messages().is_empty());
    let spawn = stored(h.kv(&key).await);
    assert_eq!(spawn["request"]["cost_budget_usd"], 25.0);
    let kept: archon_core::Timestamp =
        serde_json::from_value(spawn["request"]["deadline"].clone()).expect("a deadline");
    assert_eq!(kept, later, "the sweep's timer reads the extended deadline");
}

#[tokio::test]
async fn aborting_the_running_stage_retries_it_with_a_new_agent() {
    let h = harness().await;
    let (item, working) = implementing(&h, "gone astray").await;
    let asked = Steer {
        note: Some("Start from src/lib.rs instead.".into()),
        ..abort(None, Then::Retry)
    };
    steer(&h, &item.id, asked).await.expect("aborted");
    h.dispatch().await;
    let retried = running(&h, "implement").await.remove(0);
    assert_eq!(retried.attempt, working.attempt + 1);
    assert_ne!(agent(&retried), agent(&working));
    assert_eq!(
        h.backend.cancelled(),
        [agent(&working)],
        "the aborted agent is stopped"
    );
    let stages = h.stages(&item.id).await;
    let aborted = stages
        .iter()
        .find(|s| s.id == working.id)
        .expect("the attempt");
    assert_eq!(aborted.state, StageState::Cancelled);
    let objective = h.backend.requests().pop().expect("the retry").objective;
    assert!(
        objective.contains("Start from src/lib.rs instead."),
        "{objective}"
    );
    assert_eq!(h.state(&item.id).await, item_running(&h, &item.id).await);

    assert_eq!(
        run_out(&h, &item.id).await,
        ["implement", "implement", "review", "review", "ship"]
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn aborting_a_review_can_send_the_run_back_or_past_it() {
    let h = harness().await;
    let (item, reviewers) = reviewing(&h, "back to work").await;
    let back = abort(
        None,
        Then::Goto {
            to: "implement".into(),
        },
    );
    steer(&h, &item.id, back).await.expect("sent back");
    h.dispatch().await;
    let again = running(&h, "implement").await.remove(0);
    assert_eq!(again.attempt, 2);
    let first = h.stages(&item.id).await.remove(0);
    assert_eq!((first.stage_id.as_str(), first.attempt), ("implement", 1));
    assert_ne!(agent(&again), agent(&first), "no findings: a fresh agent");
    let mut stopped = h.backend.cancelled();
    stopped.sort();
    let mut expected: Vec<AetherAgentId> = reviewers.iter().map(agent).collect();
    expected.push(agent(&first));
    expected.sort();
    assert_eq!(
        stopped, expected,
        "the aborted reviewers, and the parked agent the fresh one replaces"
    );

    h.settle_running(|_| completed("reworked")).await;
    h.dispatch().await;
    let second = running(&h, "review").await;
    assert!(second.iter().all(|s| s.attempt == 2), "{second:?}");
    steer(&h, &item.id, abort(None, Then::Skip))
        .await
        .expect("skipped");
    h.dispatch().await;
    running(&h, "ship").await;
    h.settle_running(|_| completed("shipped")).await;
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(h.item(&item.id).await.autonomy.review.to_string(), "none");
}

#[tokio::test]
async fn a_stage_that_has_not_started_is_removed_and_a_started_one_is_not() {
    let h = harness().await;
    let (item, _) = implementing(&h, "no review this time").await;
    steer(&h, &item.id, abort(Some("review"), Then::Skip))
        .await
        .expect("removed");
    let what = &detail(&h, &item.id).await.steers[0].what;
    assert_eq!(
        what,
        "removed stage `review`; lowered the review level from standard to none"
    );
    assert_eq!(run_out(&h, &item.id).await, ["implement", "ship"]);
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));

    let (second, _) = reviewing(&h, "implement is history").await;
    let gone = refused(&h, &second.id, abort(Some("implement"), Then::Skip)).await;
    assert!(
        gone.contains("already ran and is not running now"),
        "{gone}"
    );
    assert!(
        gone.contains("goto \"implement\""),
        "it says what works: {gone}"
    );
    let not_yet = refused(&h, &second.id, abort(Some("ship"), Then::Retry)).await;
    assert!(not_yet.contains("has not started"), "{not_yet}");
    let unknown = refused(&h, &second.id, abort(Some("deploy"), Then::Skip)).await;
    assert!(unknown.contains("implement, review, ship"), "{unknown}");
}

/// A pipeline whose one stage asks for less thought than the item's work.
const LIGHT: &str = r#"[pipeline]
name = "steer-light"
description = "a light stage to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "verify"
kind = "work"
prompt = "verify.md"
effort = "low"
timeout_minutes = 30
"#;

/// An inserted stage borrows the item's first model: under the effort rubric
/// at its own effort, rolled back at that choice's, as 1.6.3 did.
#[tokio::test]
async fn an_inserted_stage_takes_its_own_effort_under_the_rubric() {
    for (sizing, expected) in [(None, Effort::Low), (Some(false), Effort::Medium)] {
        let mut config = archon_core::config::Config::default();
        config.models.effort_sizing = sizing;
        let h = harness_with(config).await;
        h.add_pipeline("steer-light", LIGHT);
        let prompt = h.repo.join(".archon/pipelines/steer-light/verify.md");
        std::fs::write(prompt, VERIFY_PROMPT).expect("prompt");
        let (item, _) = reviewing(&h, "verify lightly").await;
        let light = Steer {
            insert_stage: Some(InsertStage {
                stage: "verify".into(),
                from: Some("steer-light".into()),
                after: None,
            }),
            ..Steer::default()
        };
        let steered = steer(&h, &item.id, light).await.expect("inserted");
        let first = &steered.model_plan.choices_for("implement").unwrap()[0];
        assert_eq!(first.effort, Effort::Medium, "the work's own choice");
        let verify = &steered.model_plan.choices_for("verify").unwrap()[0];
        assert_eq!(verify.model, first.model, "the item's first model");
        assert_eq!(verify.effort, expected, "sizing {sizing:?}");
    }
}

#[tokio::test]
async fn a_stage_is_inserted_among_those_not_started_only() {
    let h = harness().await;
    let (item, _) = reviewing(&h, "verify before shipping").await;
    let inside = refused(&h, &item.id, insert("verify", Some("implement"))).await;
    assert!(
        inside.contains("review"),
        "it names the started stage in the way: {inside}"
    );
    let unknown = refused(&h, &item.id, insert("lint", None)).await;
    assert!(unknown.contains("lint"), "{unknown}");

    let steered = steer(&h, &item.id, insert("verify", None))
        .await
        .expect("inserted");
    let implementer = steered
        .model_plan
        .choices_for("implement")
        .expect("planned");
    assert_eq!(
        steered.model_plan.choices_for("verify"),
        Some(&implementer[..1])
    );
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    running(&h, "verify").await;
    assert_eq!(
        run_out(&h, &item.id).await,
        ["implement", "review", "review", "verify", "ship"]
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn the_pipeline_is_switched_for_the_stages_not_started() {
    let h = harness().await;
    let (item, working) = implementing(&h, "skip the ceremony").await;
    let before = crate::plan_origins::shown(&h, &item).await;
    let steered = steer(&h, &item.id, switch("steer-quick"))
        .await
        .expect("switched");
    let after = crate::plan_origins::shown(&h, &steered).await;
    assert_eq!(
        after["implement"], before["implement"],
        "a kept stage keeps its origin"
    );
    assert_eq!(crate::plan_origins::unattributed(&h).await, 0);
    assert_eq!(
        steered.pipeline, "wp5-review",
        "the item keeps its pipeline's name"
    );
    assert_eq!(
        switched_to(&h, &item.id).await.as_deref(),
        Some("steer-quick"),
        "and says which pipeline it runs now"
    );
    assert_eq!(
        steered.model_plan.0.len(),
        2,
        "a plan for the stages it has now"
    );
    assert_eq!(
        running(&h, "implement").await[0].id,
        working.id,
        "the work goes on"
    );
    assert_eq!(run_out(&h, &item.id).await, ["implement", "ship"]);
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));

    let (late, _) = reviewing(&h, "too late to switch").await;
    let unknown = refused(&h, &late.id, switch("no-such-pipeline")).await;
    assert!(unknown.contains("no-such-pipeline"), "{unknown}");
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    running(&h, "ship").await;
    let nothing = refused(&h, &late.id, switch("steer-quick")).await;
    assert!(
        nothing.contains("nothing left to run after stage \"ship\""),
        "{nothing}"
    );
}

#[tokio::test]
async fn a_steer_that_moves_the_review_level_says_so() {
    let h = harness().await;
    let item = h.enqueue("reviewed after all").await;
    steer(&h, &item.id, switch("steer-quick"))
        .await
        .expect("switched");
    let review = Steer {
        insert_stage: Some(InsertStage {
            stage: "review".into(),
            from: Some(PIPELINE.into()),
            after: Some("implement".into()),
        }),
        ..Steer::default()
    };
    let steered = steer(&h, &item.id, review).await.expect("inserted");
    assert_eq!(steered.autonomy.review.to_string(), "standard");
    let steers = detail(&h, &item.id).await.steers;
    let what: Vec<&str> = steers.iter().map(|steer| steer.what.as_str()).collect();
    assert_eq!(
        what,
        [
            "switched the stages still to run to `steer-quick`; lowered the review level from \
             standard to none",
            "inserted stage `review` of `wp5-review`; raised the review level from none to \
             standard",
        ]
    );
}

#[tokio::test]
async fn the_review_ceiling_holds_against_a_steer() {
    let h = harness().await;
    h.repo_config("[autonomy.max]\nreview = \"standard\"\n");
    let (item, _) = implementing(&h, "review is required").await;
    for asked in [
        abort(Some("review"), Then::Skip),
        switch("steer-quick"),
        abort(None, Then::Goto { to: "ship".into() }),
    ] {
        let error = steer(&h, &item.id, asked)
            .await
            .expect_err("above the ceiling");
        assert!(matches!(error, ArchonError::Forbidden(_)), "{error:?}");
        assert!(error.to_string().contains("review=none"), "{error}");
    }
    assert!(h.backend.cancelled().is_empty(), "nothing was aborted");
    assert_eq!(
        run_out(&h, &item.id).await,
        ["implement", "review", "review", "ship"]
    );
}

#[tokio::test]
async fn the_model_policy_holds_against_a_steer() {
    const DENIED: &str = "claude-code/claude-opus-5-5";
    let h = harness().await;
    h.repo_config(&format!("[models]\ndeny = [\"{DENIED}\"]\n"));
    let (item, _) = implementing(&h, "a model the repository denies").await;
    let asked = Steer {
        model: Some(DENIED.parse().unwrap()),
        ..Steer::default()
    };
    let error = steer(&h, &item.id, asked.clone())
        .await
        .expect_err("denied");
    assert!(matches!(error, ArchonError::Policy(_)), "{error:?}");
    assert!(error.to_string().contains(DENIED), "{error}");
    assert!(h.backend.updates().is_empty() && h.backend.interrupts().is_empty());

    let waiting = h.enqueue("denied while queued too").await;
    let named = Steer {
        stage: Some("ship".into()),
        ..asked
    };
    let error = steer(&h, &waiting.id, named).await.expect_err("denied");
    assert!(matches!(error, ArchonError::Policy(_)), "{error:?}");
    assert_eq!(*h.item(&waiting.id).await.model_plan, plan());
}

#[tokio::test]
async fn update_item_on_a_running_item_is_steered_into_its_run() {
    let h = harness().await;
    let (item, working) = implementing(&h, "updated mid-run").await;
    let mut replaced = plan();
    replaced
        .0
        .insert("ship".into(), vec![choice(MODEL, Effort::Medium)]);
    let update = Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            brief_append: Some(NOTE.into()),
            model_plan: Some(replaced.clone().into()),
            tags: Some(vec!["steered".into()]),
            ..ItemPatch::default()
        },
    };
    let Reply::Item(updated) = h.cli(update).await.expect("no cancel-and-retry needed") else {
        panic!("update_item replies with the item");
    };
    assert!(updated.brief.ends_with(&format!("{NOTE}\n")));
    assert_eq!(*updated.model_plan, replaced);
    let origins = crate::plan_origins::shown(&h, &updated).await;
    let ship = &origins["ship"][0];
    for field in [&ship.model, &ship.effort] {
        assert_eq!(
            (field.source, field.by),
            (PlanSource::Explicit, Authority::Operator)
        );
    }
    assert_eq!(crate::plan_origins::unattributed(&h).await, 0);
    assert_eq!(updated.tags, ["steered"]);
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, agent(&working));
    assert!(h.backend.cancelled().is_empty());
    let steers = detail(&h, &item.id).await.steers;
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0].what, "amended the brief; replaced the model plan");

    run_out(&h, &item.id).await;
    let ship = h.backend.requests().pop().expect("the ship request");
    assert_eq!(
        ship.choice,
        choice(MODEL, Effort::Medium),
        "later stages use the new plan"
    );
}

#[tokio::test]
async fn a_waiting_item_takes_what_it_runs_next_and_nothing_that_needs_a_run() {
    let h = harness().await;
    let item = h.enqueue("still queued").await;
    let asked = Steer {
        stage: Some("ship".into()),
        effort: Some(Effort::Medium),
        budget_usd: Some(40.0),
        ..note(NOTE)
    };
    let steered = steer(&h, &item.id, asked)
        .await
        .expect("steered while queued");
    assert_eq!(steered.state, QueueState::Queued);
    assert!(steered.brief.contains(NOTE));
    assert_eq!(steered.budget_usd, Some(40.0));
    let high = choice(MODEL, Effort::Medium);
    assert_eq!(steered.model_plan.choices_for("ship"), Some(&[high][..]));
    assert!(h.backend.messages().is_empty() && h.backend.updates().is_empty());

    let no_stage = Steer {
        effort: Some(Effort::Low),
        ..Steer::default()
    };
    let which = refused(&h, &item.id, no_stage).await;
    assert!(which.contains("name the stage"), "{which}");
    let nothing_runs = refused(&h, &item.id, abort(Some("review"), Then::Skip)).await;
    assert!(
        nothing_runs.contains("has no running stage to abort or remove yet"),
        "{nothing_runs}"
    );
    let no_deadline = Steer {
        extend_minutes: Some(10),
        ..Steer::default()
    };
    let no_deadline = refused(&h, &item.id, no_deadline).await;
    assert!(
        no_deadline.contains("has no running stage to give more time"),
        "{no_deadline}"
    );
}

#[tokio::test]
async fn a_queued_item_runs_the_stages_steered_into_it() {
    let h = harness().await;
    let item = h.enqueue("shaped before it runs").await;
    steer(&h, &item.id, insert("verify", Some("implement")))
        .await
        .expect("inserted");
    h.dispatch().await;
    assert_eq!(
        run_out(&h, &item.id).await,
        ["implement", "verify", "review", "review", "ship"]
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));

    assert_eq!(
        switched_to(&h, &item.id).await,
        None,
        "inserted, not switched"
    );

    let other = h.enqueue("back to the catalog").await;
    steer(&h, &other.id, switch("steer-quick"))
        .await
        .expect("switched");
    steer(&h, &other.id, insert("verify", Some("implement")))
        .await
        .expect("inserted");
    assert_eq!(
        switched_to(&h, &other.id).await.as_deref(),
        Some("steer-quick"),
        "an insert keeps the pipeline switched to"
    );
    steer(&h, &other.id, switch("wp5-review"))
        .await
        .expect("switched back");
    assert_eq!(switched_to(&h, &other.id).await, None);
    assert_eq!(
        h.kv(&format!("shape:{}", other.id)).await,
        None,
        "its own pipeline again"
    );
    h.dispatch().await;
    assert_eq!(
        run_out(&h, &other.id).await,
        ["implement", "review", "review", "ship"]
    );
}

#[tokio::test]
async fn a_failed_item_is_revived_on_its_branch_by_a_shape_steer() {
    let h = harness().await;
    let (item, _) = implementing(&h, "too hard for this pipeline").await;
    let escalated = |_: &StageRun| StageOutcome::Failed {
        error: "escalate: this needs a design pass first".into(),
        infra: false,
    };
    h.settle_running(escalated).await;
    h.dispatch().await;
    let failed = h.run(&item.id).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let only_a_note = steer(&h, &item.id, note(NOTE)).await.expect("amended");
    assert!(
        matches!(only_a_note.state, QueueState::Failed { .. }),
        "a note revives nothing"
    );

    let revived = steer(&h, &item.id, insert("verify", None))
        .await
        .expect("revived");
    assert_eq!(revived.state, QueueState::Queued);
    assert_eq!(revived.fresh_retries, 0, "not a fresh start");
    assert!(
        revived.brief.contains("failed at stage `implement`"),
        "{}",
        revived.brief
    );
    assert!(
        revived.brief.contains("this needs a design pass first"),
        "{}",
        revived.brief
    );
    assert!(revived.brief.contains(&failed.branch), "{}", revived.brief);
    let steers = detail(&h, &item.id).await.steers;
    let last = steers.last().expect("recorded");
    assert!(
        last.what.ends_with("queued it again on its branch"),
        "{last:?}"
    );

    h.dispatch().await;
    let runs = h
        .ctx
        .store
        .lock()
        .await
        .runs_for_item(&item.id)
        .expect("runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].branch, failed.branch, "the same branch");
    assert_eq!(runs[1].workspace, failed.workspace, "the same worktree");
    let first = running(&h, "verify").await.remove(0);
    assert_eq!(first.run, runs[1].id);
    let objective = h.backend.requests().pop().expect("its request").objective;
    assert!(
        objective.contains("Continue from it; do not start over."),
        "{objective}"
    );
}

/// Enqueue with a proposal gate, write the page `propose` owes, and settle
/// it: the run then waits for the operator.
async fn at_proposal_gate(h: &Harness, title: &str) -> QueueItem {
    let request = EnqueueRequest {
        intent: None,
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            proposal: Some(ProposalMode::Lavish),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.expect("enqueued") else {
        panic!("enqueue replies with the item");
    };
    let item = common::legacy_gate_item(h, item).await;
    h.dispatch().await;
    running(h, "propose").await;
    let page = h.run(&item.id).await.workspace.join(PROPOSAL_PAGE);
    std::fs::create_dir_all(page.parent().expect("a directory")).expect("page directory");
    std::fs::write(&page, "<html><body>page</body></html>").expect("page");
    h.settle_running(|_| completed("Approach: add a flag."))
        .await;
    h.dispatch().await;
    item
}

async fn open_questions(h: &Harness) -> Vec<Question> {
    h.ctx.store.lock().await.questions(true).expect("questions")
}

#[tokio::test]
async fn a_steer_at_an_operator_gate_keeps_the_gate_and_asks_once() {
    let h = harness().await;
    let item = at_proposal_gate(&h, "waiting for a yes").await;
    let asked = open_questions(&h).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    let waiting = h.run(&item.id).await.state;
    assert!(
        matches!(waiting, RunState::AwaitingOperator { .. }),
        "{waiting:?}"
    );

    let steered = steer(&h, &item.id, note(NOTE)).await.expect("steered");
    assert!(steered.brief.ends_with(&format!("{NOTE}\n")));
    h.dispatch().await;
    assert_eq!(h.run(&item.id).await.state, waiting, "it still waits");
    assert_eq!(open_questions(&h).await, asked, "and is not asked again");
    assert!(h.backend.messages().is_empty(), "nobody runs to be told");

    let answer = Request::Answer {
        chat: false,
        question: asked[0].id.clone(),
        text: "yes".into(),
    };
    h.cli(answer).await.expect("answered");
    h.dispatch().await;
    running(&h, "implement").await;
    let objective = h
        .backend
        .requests()
        .pop()
        .expect("the implementer")
        .objective;
    assert!(
        objective.contains(NOTE),
        "the next stage has the note: {objective}"
    );
}

#[tokio::test]
async fn states_that_cannot_be_steered_say_so() {
    let h = harness().await;
    let (done, _) = implementing(&h, "finished").await;
    run_out(&h, &done.id).await;
    let over = refused(&h, &done.id, note(NOTE)).await;
    assert!(
        over.contains("already succeeded; retry it, then steer it"),
        "{over}"
    );

    let (stopped, _) = implementing(&h, "cancelled").await;
    let cancel = Request::CancelItem {
        item: stopped.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancelled");
    for change in [switch("steer-quick"), note(NOTE)] {
        let cancelled = refused(&h, &stopped.id, change).await;
        assert!(cancelled.contains("was cancelled"), "{cancelled}");
        assert!(cancelled.contains("queue retry"), "{cancelled}");
    }

    let empty = steer(&h, &stopped.id, Steer::default())
        .await
        .expect_err("nothing to change");
    assert!(matches!(empty, ArchonError::Invalid(_)), "{empty:?}");
    let missing = steer(&h, &QueueItemId::new(), note(NOTE))
        .await
        .expect_err("no such item");
    assert!(matches!(missing, ArchonError::NotFound(_)), "{missing:?}");
}

#[tokio::test]
async fn inserting_a_stage_preserves_the_delegated_reviewer_plan() {
    let h = harness().await;
    let reviewer = "claude-code/claude-haiku-5-5";
    let mut model_plan: archon_core::model::ModelPlanDraft = archon_core::model::ModelPlan(
        [("implement".to_owned(), vec![choice(MODEL, Effort::Medium)])].into(),
    )
    .into();
    model_plan.0.insert(
        "implement.review".into(),
        vec![archon_core::model::ModelChoiceDraft {
            model: Some(reviewer.parse().unwrap()),
            effort: Some(Effort::Low),
            ..Default::default()
        }],
    );
    let Reply::Item(queued) = h
        .cli(Request::Enqueue(EnqueueRequest {
            intent: None,
            onto: None,
            repo: Some(RepoRef(REPO_NAME.into())),
            pipeline: Some("direct".into()),
            title: "Keep reviewer".into(),
            brief: "Apply the fix".into(),
            priority: Default::default(),
            depends_on: vec![],
            tags: vec![],
            model_plan,
            model_reasons: None,
            budget_usd: None,
            idempotency_key: None,
            autonomy: AutonomyPatch {
                review: Some(archon_core::autonomy::ReviewLevel::Standard),
                ..Default::default()
            },
            origin: None,
            stage_timeouts: Default::default(),
        }))
        .await
        .unwrap()
    else {
        panic!("item")
    };
    let updated = steer(&h, &queued.id, insert("verify", None)).await.unwrap();
    assert_eq!(
        updated.model_plan.0["implement.review"],
        queued.model_plan.0["implement.review"]
    );
    h.dispatch().await;
    h.cli(Request::UpdateItem {
        item: queued.id.clone(),
        patch: ItemPatch {
            stage_timeouts: [("verify".into(), 75)].into(),
            ..Default::default()
        },
    })
    .await
    .expect("timeouts accept an inserted stage on a running item");
    assert_eq!(h.item(&queued.id).await.stage_timeouts["verify"], 75);
    h.settle_running(|_| completed("verified")).await;
    h.dispatch().await;
    assert!(
        h.backend
            .requests()
            .iter()
            .any(|r| r.instructions.contains(reviewer))
    );
}

/// A steer still owed when a held stage's deadline moved past its own sends
/// the later one: Aether never ends up with the earlier deadline.
#[tokio::test]
async fn an_owed_extension_never_lowers_a_held_stages_moved_deadline() {
    let h = harness().await;
    let (item, working) = implementing(&h, "blocked on a usage limit").await;
    let soon = archon_core::now() + chrono::Duration::minutes(2);
    common::set_deadline(&h, &working, soon).await;
    h.backend
        .fail_next_update(ArchonError::Unavailable("aether busy".into()));
    let asked = Steer {
        extend_minutes: Some(3),
        ..Steer::default()
    };
    steer(&h, &item.id, asked).await.expect("steered");
    assert!(h.backend.updates().is_empty(), "the extension is owed");

    let held = archon_core::ports::TreePulse {
        held: true,
        ..archon_core::ports::TreePulse::default()
    };
    h.backend.set_pulse(&agent(&working), held);
    h.dispatch().await;

    let moved = common::stored_deadline(&h, &working)
        .await
        .expect("a deadline");
    assert!(moved > soon + chrono::Duration::minutes(20), "{moved}");
    let updates = h.backend.updates();
    assert_eq!(
        updates.len(),
        2,
        "the move, then the owed steer: {updates:?}"
    );
    assert_eq!(
        updates[1].1.deadline,
        Some(moved),
        "the steer sends the later one"
    );
}

/// A catalog outage holds a steered model back, never the extension beside it.
#[tokio::test]
async fn more_time_reaches_the_agent_while_its_new_effort_waits_for_the_catalog() {
    let h = harness().await;
    let (item, working) = implementing(&h, "add a greeting").await;
    h.catalog_server.set_default(
        "list_models",
        archon_aether::fake::Scripted::refuse("unavailable", "catalog offline"),
    );
    let store = h.ctx.store.lock().await;
    store.invalidate_model_catalog("catalog offline".into());
    drop(store);

    let asked = Steer {
        effort: Some(Effort::Low),
        extend_minutes: Some(45),
        ..Steer::default()
    };
    steer(&h, &item.id, asked).await.expect("steered");
    let updates = h.backend.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0].0, agent(&working));
    assert!(updates[0].1.deadline.is_some(), "the extension is sent");
    assert_eq!(
        updates[0].1.choice, None,
        "the effort waits for the catalog"
    );
}

/// An extension steered after a model change still waiting for the catalog
/// is sent at once, though the steps go on in order.
#[tokio::test]
async fn an_extension_after_an_effort_waiting_for_the_catalog_is_sent_at_once() {
    let h = harness().await;
    let (item, working) = implementing(&h, "add a greeting").await;
    h.catalog_server.set_default(
        "list_models",
        archon_aether::fake::Scripted::refuse("unavailable", "catalog offline"),
    );
    let store = h.ctx.store.lock().await;
    store.invalidate_model_catalog("catalog offline".into());
    drop(store);

    let effort = Steer {
        effort: Some(Effort::Low),
        ..Steer::default()
    };
    let replied = steer(&h, &item.id, effort).await.expect("steered");
    assert!(h.backend.updates().is_empty(), "the effort waits");
    let deferred = replied
        .model_plan
        .catalog_check
        .as_ref()
        .expect("the reply says it waits");
    assert!(
        deferred.to_string().contains("catalog offline"),
        "{deferred}"
    );
    assert_eq!(
        h.item(&item.id).await.model_plan.catalog_check.as_ref(),
        Some(deferred),
        "queue show says it waits"
    );
    let more_time = Steer {
        extend_minutes: Some(45),
        ..Steer::default()
    };
    steer(&h, &item.id, more_time).await.expect("steered");

    let updates = h.backend.updates();
    let sent: Vec<_> = updates.iter().filter_map(|(_, u)| u.deadline).collect();
    assert!(!sent.is_empty(), "the extension is sent: {updates:?}");
    assert!(
        updates
            .iter()
            .all(|(to, u)| *to == agent(&working) && u.choice.is_none())
    );
    let stored = common::stored_deadline(&h, &working)
        .await
        .expect("a deadline");
    assert_eq!(
        sent.last(),
        Some(&stored),
        "Aether holds the extended deadline"
    );
}

/// Aether stops answering `list_models` and archond drops its catalog.
async fn catalog_offline(h: &Harness) {
    h.catalog_server.set_default(
        "list_models",
        archon_aether::fake::Scripted::refuse("unavailable", "catalog offline"),
    );
    let store = h.ctx.store.lock().await;
    store.invalidate_model_catalog("catalog offline".into());
}

/// Each extension sent while a model step waits for the catalog reaches a
/// helper Aether cannot raise with its lead: a second is never taken for a repeat.
#[tokio::test]
async fn a_second_extension_reaches_a_helper_while_an_effort_waits_for_the_catalog() {
    let h = harness().await;
    let (item, working) = implementing(&h, "with a helper").await;
    let helper = h.backend.delegate(&agent(&working), MODEL);
    let soon = archon_core::now() + chrono::Duration::minutes(5);
    h.backend.limit_delegated(&helper, soon);
    catalog_offline(&h).await;

    let effort = Steer {
        effort: Some(Effort::Low),
        ..Steer::default()
    };
    steer(&h, &item.id, effort).await.expect("steered");
    for minutes in [30, 60] {
        let more_time = Steer {
            extend_minutes: Some(minutes),
            ..Steer::default()
        };
        steer(&h, &item.id, more_time).await.expect("steered");
    }

    let stored = common::stored_deadline(&h, &working)
        .await
        .expect("a deadline");
    let raised: Vec<_> = h
        .backend
        .updates()
        .into_iter()
        .filter(|(to, _)| *to == helper)
        .filter_map(|(_, update)| update.deadline)
        .collect();
    assert_eq!(raised.len(), 2, "one raise per extension: {raised:?}");
    assert_eq!(raised.last(), Some(&stored), "the helper holds the second");
}

/// A model step that waited for the catalog carries the budget stored now
/// when it goes on: a later, larger grant is never replaced by its own.
#[tokio::test]
async fn a_replayed_effort_never_sends_a_budget_a_later_steer_replaced() {
    let h = harness().await;
    let item = h.enqueue("capped").await;
    let capped = Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            budget_usd: Some(10.0),
            ..ItemPatch::default()
        },
    };
    h.cli(capped).await.expect("a budget before the run");
    h.dispatch().await;
    let working = running(&h, "implement").await.remove(0);
    enforce_budget(&h, &working).await;
    catalog_offline(&h).await;

    let effort_and_budget = Steer {
        effort: Some(Effort::Low),
        budget_usd: Some(25.0),
        ..Steer::default()
    };
    steer(&h, &item.id, effort_and_budget)
        .await
        .expect("steered");
    let more = Steer {
        budget_usd: Some(40.0),
        ..Steer::default()
    };
    steer(&h, &item.id, more).await.expect("steered");
    common::catalog::answer(&h.catalog_server, &common::catalog::default_models());
    h.dispatch().await;

    let updates = h.backend.updates();
    assert!(
        updates.iter().any(|(_, u)| u.choice.is_some()),
        "the effort goes on once the catalog answers: {updates:?}"
    );
    let budgets: Vec<f64> = updates
        .iter()
        .filter_map(|(_, u)| u.cost_budget_usd)
        .collect();
    let newest = budgets.iter().position(|usd| *usd == 40.0);
    let newest = newest.expect("the newest budget is sent");
    assert!(
        budgets[newest..].iter().all(|usd| *usd == 40.0),
        "no earlier budget after the newest: {budgets:?}"
    );
}

/// A model step that waited for the catalog and is then refused still tells
/// the lead its note: only the model is dropped, and the refusal is reported.
#[tokio::test]
async fn a_refused_deferred_model_still_tells_the_lead_its_note() {
    const NOTE: &str = "the note the lead is owed";
    let h = harness().await;
    let (item, working) = implementing(&h, "a deferred model").await;
    catalog_offline(&h).await;
    let to_reviewer = Steer {
        model: Some(REVIEW_MODEL.parse().unwrap()),
        note: Some(NOTE.into()),
        ..Steer::default()
    };
    steer(&h, &item.id, to_reviewer).await.expect("steered");
    assert!(
        h.backend.messages().is_empty(),
        "the step waits for the catalog"
    );
    let catalog = common::catalog::default_models();
    let listed = catalog.models().iter().map(|m| m.model.clone());
    let unlisted: Vec<_> = listed.filter(|m| m.to_string() != REVIEW_MODEL).collect();
    common::catalog::answer(&h.catalog_server, &common::catalog::untiered(unlisted));
    h.dispatch().await;

    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, agent(&working));
    assert!(messages[0].1.contains(NOTE), "{}", messages[0].1);
    assert!(
        !messages[0].1.contains("you now run on"),
        "no model change is claimed: {}",
        messages[0].1
    );
    assert!(
        h.backend.interrupts().is_empty(),
        "the lead keeps its turn on its model"
    );
    let updates = h.backend.updates();
    assert!(
        updates.iter().all(|(_, u)| u.choice.is_none()),
        "{updates:?}"
    );
    let owed = h.kv(&format!("steer_owed:{}", working.id)).await;
    assert_eq!(owed, None, "nothing is left owed");
}

/// A model update Aether refuses for good still sends the step's extension and
/// note; the lead keeps its turn, and the refusal is reported.
#[tokio::test]
async fn a_model_aether_refuses_still_sends_the_steps_note_and_extension() {
    const NOTE: &str = "the note beside the refused model";
    let h = harness().await;
    let (item, working) = implementing(&h, "a refused model update").await;
    let soon = archon_core::now() + chrono::Duration::minutes(30);
    common::set_deadline(&h, &working, soon).await;
    h.backend.fail_next_update(ArchonError::Invalid(
        "aether: model not allowed here".into(),
    ));
    let asked = Steer {
        effort: Some(Effort::Low),
        extend_minutes: Some(10),
        note: Some(NOTE.into()),
        ..Steer::default()
    };
    steer(&h, &item.id, asked).await.expect("steered");

    let updates = h.backend.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0].1.choice, None, "the refused model is left out");
    assert!(
        updates[0].1.deadline.is_some_and(|d| d > soon),
        "{updates:?}"
    );
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].1.contains(NOTE), "{}", messages[0].1);
    assert!(
        !messages[0].1.contains("you now run on"),
        "{}",
        messages[0].1
    );
    assert!(h.backend.interrupts().is_empty(), "no turn ends");
    let feed = h.notices(NotificationKind::Progress).await;
    let told = feed.iter().find(|n| n.contains("model was refused"));
    let told = told.expect("the refusal is reported");
    assert!(
        told.contains("note and deadline extension reached the agent"),
        "{told}"
    );
    assert!(!told.contains("did not reach"), "{told}");
    assert_eq!(h.kv(&format!("steer_owed:{}", working.id)).await, None);
}

/// A model step owed before its note was split from the lead's message keeps
/// the old delivery when refused: the lead's message is told and its turn ends.
#[tokio::test]
async fn a_refused_model_step_owed_before_split_notes_tells_its_lead_message() {
    const LEAD: &str = "the lead message an older daemon owed";
    let h = harness().await;
    let (_item, working) = implementing(&h, "an owed legacy step").await;
    let owed = format!("steer_owed:{}", working.id);
    let legacy = serde_json::json!([{
        "key": "legacy-step",
        "choice": choice(MODEL, Effort::Low),
        "interrupt": true,
        "lead": LEAD,
    }]);
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&owed, &legacy.to_string())
        .expect("kv");
    h.backend.fail_next_update(ArchonError::Invalid(
        "aether: model not allowed here".into(),
    ));

    h.dispatch().await;
    let messages = h.backend.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0], (agent(&working), LEAD.to_owned()));
    assert_eq!(h.backend.interrupts(), [agent(&working)]);
    assert_eq!(h.kv(&owed).await, None, "nothing is left owed");
}

/// As when Aether enforces the cost budget `stage` was sent.
async fn enforce_budget(h: &Harness, stage: &StageRun) {
    let key = format!("spawn_request:{}", stage.id);
    let raw = h.kv(&key).await.expect("a stored spawn");
    let mut spawn: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(spawn["request"]["cost_budget_usd"].is_number(), "{spawn}");
    spawn["request"]["aether_cost_budget"] = true.into();
    let mut store = h.ctx.store.lock().await;
    store.kv_set(&key, &spawn.to_string()).expect("kv");
}
