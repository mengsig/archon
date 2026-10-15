//! What a dependent item's agents are told about the items it was queued
//! after (ARCHITECTURE §8.6): its own branch still forks from the base.

mod common;

use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::SpawnRequest;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use common::{Harness, MODEL, PIPELINE, REPO_NAME, choice, completed, plan, verdict};
use serde_json::{Value, json};

const HEADING: &str = "\n## Dependencies\n";
const SCRATCH: &str = "scratch-note";
const SCRATCH_TOML: &str = r#"[pipeline]
name = "scratch-note"
description = "works in scratch"
version = 1
workspace = "scratch"
requires_repo = false

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30
"#;

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        _ => completed("done"),
    }
}

fn scratch_plan() -> ModelPlan {
    let mut plan = ModelPlan::default();
    plan.0
        .insert("implement".into(), vec![choice(MODEL, Effort::Medium)]);
    plan
}

/// Enqueue `title` into `pipeline`, to run once every item of `after` succeeded.
async fn enqueue_after(
    h: &Harness,
    pipeline: &str,
    title: &str,
    after: &[&QueueItem],
) -> QueueItem {
    let model_plan = match pipeline {
        SCRATCH => scratch_plan(),
        _ => plan(),
    };
    let request = EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: after.iter().map(|item| item.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: model_plan.into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// Settle the running item's agents until it succeeds; the dispatch that
/// follows starts whatever waited for it.
async fn succeed(h: &Harness, item: &QueueItem) {
    for _ in 0..3 {
        if matches!(h.state(&item.id).await, QueueState::Succeeded { .. }) {
            break;
        }
        h.settle_running(passing).await;
        h.dispatch().await;
    }
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    h.dispatch().await;
}

/// The spawn requests of `item`'s agents, oldest first.
fn requests_of(h: &Harness, item: &QueueItem) -> Vec<SpawnRequest> {
    let mut requests = h.backend.requests();
    requests.retain(|request| request.labels["archon.item"] == item.id.as_str());
    requests
}

/// The JSON facts of the contract's Dependencies section, one per item.
fn facts(instructions: &str) -> Vec<Value> {
    let section = instructions.split_once(HEADING).expect("the section").1;
    section
        .lines()
        .filter_map(|line| line.strip_prefix("- ").filter(|fact| fact.starts_with('{')))
        .map(|fact| serde_json::from_str(fact).expect("each fact is JSON"))
        .collect()
}

/// A change stacked on a dependency's branch must be reviewed from that
/// branch, not from the base: every agent is told the branch and the rule.
#[tokio::test]
async fn a_dependent_items_agents_are_told_where_each_dependency_left_its_work() {
    let h = Harness::new().await;
    let first = h.enqueue("first change").await;
    let second = enqueue_after(&h, PIPELINE, "second change", &[&first]).await;
    h.dispatch().await;
    assert!(
        requests_of(&h, &second).is_empty(),
        "it waits for the first"
    );
    succeed(&h, &first).await;

    let shipped = h.run(&first.id).await;
    let expected = [json!({
        "item": first.id.as_str(),
        "branch": shipped.branch,
        "pull_request": "https://github.com/o/r/pull/1",
    })];
    let told_before = requests_of(&h, &first);
    assert_eq!(told_before.len(), 4, "implement, two reviewers, ship");
    for request in &told_before {
        assert!(!request.instructions.contains(HEADING), "no dependency");
    }
    let implement = requests_of(&h, &second).remove(0);
    assert_eq!(implement.labels["archon.stage"], "implement");
    assert_eq!(facts(&implement.instructions), expected);
    assert!(
        implement
            .instructions
            .contains(&format!("prepared this branch from `{}`", shipped.branch))
    );
    assert!(
        implement
            .instructions
            .contains("pass this base to reviewers")
    );
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .stack_base(&second.id)
            .unwrap()
            .unwrap()
            .0,
        shipped.branch
    );
    assert_ne!(h.run(&second.id).await.branch, shipped.branch);

    h.settle_running(passing).await;
    h.dispatch().await;
    let reviewers = requests_of(&h, &second).split_off(1);
    assert_eq!(reviewers.len(), 2, "both reviewers of the second item");
    for reviewer in &reviewers {
        assert_eq!(reviewer.labels["archon.stage"], "review");
        assert_eq!(facts(&reviewer.instructions), expected);
    }
}

/// A dependency that worked in scratch left no branch to stack on, and an
/// item that works in scratch has no branch of its own to stack.
#[tokio::test]
async fn only_a_branch_of_the_repository_is_named_and_only_to_a_worktree_item() {
    let h = Harness::new().await;
    h.add_pipeline(SCRATCH, SCRATCH_TOML);
    let note = enqueue_after(&h, SCRATCH, "a note", &[]).await;
    let change = enqueue_after(&h, PIPELINE, "a change", &[&note]).await;
    let follow_up = enqueue_after(&h, SCRATCH, "a follow-up note", &[&note]).await;
    h.dispatch().await;
    succeed(&h, &note).await;

    let implement = requests_of(&h, &change).remove(0);
    let expected = json!({"item": note.id.as_str(), "branch": null, "pull_request": null});
    assert_eq!(facts(&implement.instructions), [expected]);
    let scratch = requests_of(&h, &follow_up);
    assert_eq!(scratch.len(), 1, "the scratch item started too");
    assert!(!scratch[0].instructions.contains(HEADING));
}
