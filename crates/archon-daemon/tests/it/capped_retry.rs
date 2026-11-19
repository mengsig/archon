//! A retry after a stage escalated continues on the same branch: the item's
//! earlier runs' reviews still count while the branch holds their heads, so
//! the new lead owes only a delta review of its own commits. A fresh retry,
//! or a rewritten history, owes the whole review again.

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use common::delivery::{add_origin, origin_of, pass_by, pushed, record, shipped_by, verdict_on};
use common::{Harness, REVIEW_MODEL, git};
use serde_json::{Value, json};

/// A strict `adaptive` item, dispatched, in a repository with an `origin`.
async fn strict_item(h: &Harness) -> QueueItem {
    add_origin(h);
    let request = EnqueueRequest {
        intent: None,
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(common::REPO_NAME.into())),
        pipeline: Some("adaptive".into()),
        title: "a strict change".into(),
        brief: "brief for a strict change".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: Default::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review: Some(ReviewLevel::Strict),
            ..Default::default()
        },
        origin: None,
    };
    let Reply::Item(item) = h.cli(Request::Enqueue(request)).await.unwrap() else {
        panic!("enqueue replies with the item");
    };
    h.dispatch().await;
    item
}

fn lead_agent(lead: &StageRun) -> AetherAgentId {
    lead.aether_agent.clone().expect("a lead agent")
}

/// What the first run left: its lead, its three whole-change reviewers and the head.
struct FirstRun {
    lead: AetherAgentId,
    reviewers: Vec<AetherAgentId>,
    head: Value,
}

/// The first run: three whole-change reviewers of the lead's commit, one failing
/// it with a blocking finding, and a worker whose own check passed it; then the
/// lead escalates. Any failure ends a run alike; the cap's own is in `executor`.
async fn first_run_escalates(h: &Harness, item: &QueueItem) -> FirstRun {
    let first = first_run_reviews(h).await;
    escalate(h, item).await;
    first
}

/// The lead of the first run escalates, which ends its run.
async fn escalate(h: &Harness, item: &QueueItem) {
    h.settle_running(|_| StageOutcome::Failed {
        error: "ESCALATE: the fix needs a decision".into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
}

/// The first run's whole-change reviews, as [`first_run_escalates`] describes, before it escalates.
async fn first_run_reviews(h: &Harness) -> FirstRun {
    let lead = h.running().await.remove(0);
    let agent = lead_agent(&lead);
    let reviewers: Vec<AetherAgentId> = (0..3)
        .map(|_| h.backend.delegate(&agent, REVIEW_MODEL))
        .collect();
    let worker = h.backend.delegate_as(&agent, REVIEW_MODEL, "worker");
    let read = shipped_by(h, &lead, "the first head", json!({})).await;
    h.backend
        .complete_delegated(&worker, verdict_on(&read, "pass"));
    let mut failing = verdict_on(&read, "fail");
    failing["findings"] = json!([{"title": "drops the tail", "blocking": true}]);
    h.backend.complete_delegated(&reviewers[0], failing);
    for passing in &reviewers[1..] {
        h.backend
            .complete_delegated(passing, verdict_on(&read, "pass"));
    }
    FirstRun {
        lead: agent,
        reviewers,
        head: verdict_on(&read, "pass")["head"].clone(),
    }
}

/// Retry `item`, `fresh` or not, and return the new run's lead.
async fn retried(h: &Harness, item: &QueueItem, fresh: bool) -> StageRun {
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("retry");
    h.dispatch().await;
    let lead = h.running().await.remove(0);
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    assert_eq!(runs.len(), 2, "the retry is a new run");
    lead
}

/// The new lead commits a fix and names one delta reviewer, which passed it
/// from `base`; the lead's stage then completes with that pass.
async fn one_delta_pass(h: &Harness, lead: &StageRun, base: &Value) -> AetherAgentId {
    let delta = h.backend.delegate(&lead_agent(lead), REVIEW_MODEL);
    let built = shipped_by(h, lead, "fixed", pass_by(std::slice::from_ref(&delta))).await;
    let mut verdict = verdict_on(&built, "pass");
    verdict["base"] = base.clone();
    h.backend.complete_delegated(&delta, verdict);
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    delta
}

/// The item's failure, once the gate refused its review; nothing was pushed.
async fn refused(h: &Harness, item: &QueueItem) -> String {
    let QueueState::Failed { error, run } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    let branch = h.ctx.store.lock().await.run(&run.unwrap()).unwrap().branch;
    assert_eq!(pushed(&origin_of(h), &branch), None, "nothing is delivered");
    assert!(error.contains("is not on record"), "{error}");
    error
}

/// The first run's three whole reads count for the second run's lead, which
/// delegated one delta reviewer of its fix: the record names all four, and not
/// the worker, whose pass is no review.
#[tokio::test]
async fn a_retry_on_the_branch_ships_on_one_delta_pass_of_the_new_commits() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    assert_ne!(first.lead, lead_agent(&lead), "a new lead");
    let delta = one_delta_pass(&h, &lead, &first.head).await;
    assert!(
        matches!(h.state(&item.id).await, QueueState::Succeeded { .. }),
        "{:?}",
        h.state(&item.id).await
    );
    let stored = h.ctx.store.lock().await.stage_run(&lead.id).unwrap();
    let review = &stored.structured.as_ref().unwrap()["review"];
    let mut found: Vec<String> = serde_json::from_value(review["reviewer_agents"].clone())
        .unwrap_or_else(|e| panic!("{e}: {review}"));
    found.sort();
    let mut expected: Vec<String> = first
        .reviewers
        .iter()
        .chain([&delta])
        .map(ToString::to_string)
        .collect();
    expected.sort();
    assert_eq!(
        found, expected,
        "the carried whole reads and the delta pass"
    );
}

/// A fresh retry starts over from the base: the first run's reviews read
/// commits its branch never holds, so one delta pass is no review.
#[tokio::test]
async fn a_fresh_retry_owes_the_whole_review_again() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, true).await;
    one_delta_pass(&h, &lead, &first.head).await;
    let error = refused(&h, &item).await;
    assert!(error.contains("names 1 reviewer"), "{error}");
}

/// The new lead rewrote the reviewed commit away: the first run's reads are of
/// a history the branch no longer holds, so they count for nothing.
#[tokio::test]
async fn a_rewritten_history_drops_the_earlier_runs_reviews() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    git(&workspace, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let fork = json!(git(&workspace, &["rev-parse", "HEAD"]));
    // Unlike a replayed commit, this one makes the rewritten history differ.
    git(
        &workspace,
        &["commit", "--quiet", "--allow-empty", "-m", "Start over"],
    );
    assert_ne!(fork, first.head, "the reviewed commit is gone");
    one_delta_pass(&h, &lead, &fork).await;
    let error = refused(&h, &item).await;
    assert!(error.contains("1 of the reviewers"), "{error}");
}

/// The first run's fix of its whole reads failed as a delta, and the new lead amended
/// only that fix: the history is rewritten though the whole reads' head stands, so
/// one delta pass from that head is no review.
#[tokio::test]
async fn amending_only_the_failed_fix_drops_the_earlier_runs_reviews() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_reviews(&h).await;
    let lead = h.running().await.remove(0);
    let delta = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let fix = shipped_by(&h, &lead, "a fix", json!({})).await;
    let mut failing = verdict_on(&fix, "fail");
    failing["base"] = first.head.clone();
    failing["findings"] = json!([{"title": "still drops the tail", "blocking": true}]);
    h.backend.complete_delegated(&delta, failing);
    escalate(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    let amend = [
        "commit",
        "--quiet",
        "--amend",
        "--allow-empty",
        "-m",
        "Fix again",
    ];
    git(&workspace, &amend);
    assert_eq!(json!(git(&workspace, &["rev-parse", "HEAD~1"])), first.head);
    one_delta_pass(&h, &lead, &first.head).await;
    let error = refused(&h, &item).await;
    let from = &first.head.as_str().unwrap()[..12];
    assert!(error.contains(&format!("read only from {from}")), "{error}");
}

/// The first run's fail is no waiver: a new pass of the head it failed reads
/// nothing past it, so the carried fail still stands and nothing ships.
#[tokio::test]
async fn an_earlier_runs_fail_stands_against_a_pass_of_the_same_head() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    assert_eq!(json!(git(&workspace, &["rev-parse", "HEAD"])), first.head);
    let again = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    let mut unchanged = record(&workspace);
    unchanged["review"] = pass_by(std::slice::from_ref(&again))["review"].clone();
    let verdict = json!({"verdict": "pass", "head": first.head, "findings": []});
    h.backend.complete_delegated(&again, verdict);
    h.settle_running(|_| StageOutcome::Completed {
        summary: "nothing to fix".into(),
        structured: Some(unchanged.clone()),
    })
    .await;
    h.dispatch().await;
    let error = refused(&h, &item).await;
    let failed = first.reviewers[0].to_string();
    assert!(
        error.contains(&format!("{failed} failed the change")),
        "{error}"
    );
}

/// Aether no longer lists the reviewer whose fail the first run recorded: the
/// other reads carry no longer, so one pass of that head is no review.
#[tokio::test]
async fn a_recorded_fail_aether_forgot_stops_the_carry() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_reviews(&h).await;
    // Aether's event of the failing reviewer: the first run's tally records its fail.
    archon_daemon::executor::reviewer_settled(&h.ctx, &first.lead, &first.reviewers[0])
        .await
        .unwrap();
    escalate(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    let again = h.backend.delegate(&lead_agent(&lead), REVIEW_MODEL);
    h.backend.forget_delegated(&first.reviewers[0]);
    let mut unchanged = record(&workspace);
    assert_eq!(unchanged["commit"], first.head, "the failed head itself");
    unchanged["review"] = pass_by(std::slice::from_ref(&again))["review"].clone();
    let verdict = json!({"verdict": "pass", "head": first.head, "findings": []});
    h.backend.complete_delegated(&again, verdict);
    h.settle_running(|_| StageOutcome::Completed {
        summary: "nothing to fix".into(),
        structured: Some(unchanged.clone()),
    })
    .await;
    h.dispatch().await;
    let error = refused(&h, &item).await;
    assert!(error.contains("names 1 reviewer"), "{error}");
}

/// The orchestrator accepts the escalated head: its retry moves the open findings
/// to follow-ups, and the new lead's own whole review of that head stands alone.
#[tokio::test]
async fn accepting_the_escalated_head_ships_it_on_a_whole_review_of_its_own() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let first = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    let agent = lead_agent(&lead);
    let again: Vec<AetherAgentId> = (0..3)
        .map(|_| h.backend.delegate(&agent, REVIEW_MODEL))
        .collect();
    let mut accepted = record(&workspace);
    assert_eq!(accepted["commit"], first.head, "the escalated head itself");
    accepted["review"] = pass_by(&again)["review"].clone();
    for reviewer in &again {
        let verdict = json!({"verdict": "pass", "head": first.head, "findings": []});
        h.backend.complete_delegated(reviewer, verdict);
    }
    h.settle_running(|_| StageOutcome::Completed {
        summary: "accepted; the open findings are follow-ups".into(),
        structured: Some(accepted.clone()),
    })
    .await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let branch = h.ctx.store.lock().await.run(&lead.run).unwrap().branch;
    let head = first.head.as_str().map(str::to_owned);
    assert_eq!(
        pushed(&origin_of(&h), &branch),
        head,
        "the head is delivered"
    );
}
