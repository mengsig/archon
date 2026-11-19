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
use common::delivery::{add_origin, origin_of, pass_by, pushed, shipped_by, verdict_on};
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

/// The first run: three whole-change reviewers of the lead's commit, one failing
/// it with a blocking finding, then the lead escalates. Returns them and the head.
async fn first_run_escalates(h: &Harness, item: &QueueItem) -> (Vec<AetherAgentId>, Value) {
    let lead = h.running().await.remove(0);
    let agent = lead_agent(&lead);
    let reviewers: Vec<AetherAgentId> = (0..3)
        .map(|_| h.backend.delegate(&agent, REVIEW_MODEL))
        .collect();
    let read = shipped_by(h, &lead, "the first head", json!({})).await;
    let mut failing = verdict_on(&read, "fail");
    failing["findings"] = json!([{"title": "drops the tail", "blocking": true}]);
    h.backend.complete_delegated(&reviewers[0], failing);
    for passing in &reviewers[1..] {
        h.backend
            .complete_delegated(passing, verdict_on(&read, "pass"));
    }
    h.settle_running(|_| StageOutcome::Failed {
        error: "ESCALATE: the fix needs a decision".into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    (reviewers, verdict_on(&read, "pass")["head"].clone())
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
/// delegated one delta reviewer of its fix: the record names all four.
#[tokio::test]
async fn a_retry_on_the_branch_ships_on_one_delta_pass_of_the_new_commits() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let (first, head) = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    assert!(!first.contains(&lead_agent(&lead)), "a new lead");
    let delta = one_delta_pass(&h, &lead, &head).await;
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
    let (_, head) = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, true).await;
    one_delta_pass(&h, &lead, &head).await;
    let error = refused(&h, &item).await;
    assert!(error.contains("names 1 reviewer"), "{error}");
}

/// The new lead rewrote the reviewed commit away: the first run's reads are of
/// a history the branch no longer holds, so they count for nothing.
#[tokio::test]
async fn a_rewritten_history_drops_the_earlier_runs_reviews() {
    let h = Harness::new().await;
    let item = strict_item(&h).await;
    let (_, head) = first_run_escalates(&h, &item).await;
    let lead = retried(&h, &item, false).await;
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    git(&workspace, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let fork = json!(git(&workspace, &["rev-parse", "HEAD"]));
    // Unlike a replayed commit, this one makes the rewritten history differ.
    git(
        &workspace,
        &["commit", "--quiet", "--allow-empty", "-m", "Start over"],
    );
    assert_ne!(fork, head, "the reviewed commit is gone");
    one_delta_pass(&h, &lead, &fork).await;
    let error = refused(&h, &item).await;
    assert!(error.contains("1 of the reviewers"), "{error}");
}
