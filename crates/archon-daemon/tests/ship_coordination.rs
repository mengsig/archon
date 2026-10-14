//! archond's own delivery (ARCHITECTURE §8.7) of work that is coordinated:
//! a stacked item ships onto its parent's branch (§8.8), and the members of
//! an atomic initiative open as drafts that are readied together (§8.9).
//! What then becomes of a pull request is learned from GitHub (§19).
//! Every pull request here is opened by `gh pr create`, never by an agent.

mod common;

use archon_core::id::QueueItemId;
use archon_core::initiative::{
    EnqueueInitiative, InitiativeDetail, InitiativeMember, InitiativeState,
};
use archon_core::model::ModelPlan;
use archon_core::outcome::{Correction, Outcome};
use archon_core::protocol::{EnqueueRequest, Reply, RepoAdd, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::PipelineRun;
use archon_daemon::executor::{self, pull_requests};
use archon_daemon::triggers::{self, TriggerEvent};
use common::delivery::{add_origin, origin_of, pushed, shipped_by};
use common::initiative::{gh_calls, install_explicit_gh, prs, set_pr};
use common::{Harness, REPO_NAME, git};
use serde_json::json;

const PIPELINE: &str = "quick-pr";

/// A harness whose repository has an `origin` and whose GitHub holds only
/// the pull requests archond opens.
async fn delivering() -> Harness {
    let h = Harness::new().await;
    add_origin(&h);
    install_explicit_gh(&h);
    h
}

/// Enqueue a change on [`PIPELINE`] with no model plan, after `parents`.
async fn enqueue(h: &Harness, title: &str, parents: &[&QueueItem]) -> QueueItem {
    let request = EnqueueRequest {
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: parents.iter().map(|p| p.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: ModelPlan::default(),
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

/// The item's one agent commits a change and completes with the ship record
/// for it; archond delivers the run and the item succeeds.
async fn ship(h: &Harness, item: &QueueItemId) -> PipelineRun {
    h.dispatch().await;
    let run = h.run(item).await;
    let running = h.running().await;
    let mine = running.iter().find(|stage| stage.run == run.id);
    let stage = mine.expect("the item's agent runs");
    let outcome = shipped_by(h, stage, "Changed it; cargo test passes", json!({})).await;
    let agent = stage.aether_agent.as_ref().expect("an agent");
    let settled = executor::stage_settled(&h.ctx, agent, outcome).await;
    settled.expect("the stage settles");
    h.dispatch().await;
    let state = h.state(item).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    run
}

/// The `gh pr create` call that opened the pull request of `branch`.
fn creation(h: &Harness, branch: &str) -> Vec<String> {
    let calls = gh_calls(h);
    let opens = |call: &&Vec<String>| call[1] == "create" && call.contains(&branch.to_owned());
    let created: Vec<&Vec<String>> = calls.iter().filter(opens).collect();
    assert_eq!(created.len(), 1, "one pull request per branch: {calls:?}");
    created[0].clone()
}

fn flag<'a>(call: &'a [String], name: &str) -> &'a str {
    let at = call.iter().position(|arg| arg == name);
    &call[at.expect("the flag") + 1]
}

/// The delivery reads the stack: the child's pull request is for its parent's
/// branch, and for the repository's once the parent merged.
#[tokio::test]
async fn a_stacked_item_ships_onto_its_parent_and_moves_to_the_base_once_it_merged() {
    let h = delivering().await;
    let parent = enqueue(&h, "the parent change", &[]).await;
    let child = enqueue(&h, "the child change", &[&parent]).await;
    let parent_run = ship(&h, &parent.id).await;
    assert_eq!(flag(&creation(&h, &parent_run.branch), "--base"), "main");

    let child_run = ship(&h, &child.id).await;
    let created = creation(&h, &child_run.branch);
    assert_eq!(flag(&created, "--base"), parent_run.branch);
    assert!(!created.contains(&"--draft".to_owned()), "{created:?}");
    let head = git(&h.repo, &["rev-parse", &child_run.branch]);
    assert_eq!(pushed(&origin_of(&h), &child_run.branch), Some(head));
    let parent_head = git(&h.repo, &["rev-parse", &parent_run.branch]);
    let within = [
        "merge-base",
        "--is-ancestor",
        &parent_head,
        &child_run.branch,
    ];
    git(&h.repo, &within);

    set_pr(&h, &parent_run.branch, "state", json!("MERGED"));
    h.dispatch().await;
    assert_eq!(prs(&h)[&child_run.branch]["baseRefName"], "main");
    let stacked = h.ctx.store.lock().await.stack_base(&child.id).unwrap();
    assert_eq!(stacked, None, "nothing unmerged is left under it");
    assert_eq!(prs(&h).as_object().unwrap().len(), 2, "never a second one");
}

/// A second repository with an `origin`, whose ceiling allows auto-merge.
async fn add_client(h: &Harness) {
    let path = common::init_repo(&h.dir.path().join("client"));
    let origin = h.dir.path().join("client-origin.git");
    let bare = origin.to_str().expect("utf-8");
    git(h.dir.path(), &["init", "--quiet", "--bare", bare]);
    git(&path, &["remote", "add", "origin", bare]);
    git(&path, &["push", "--quiet", "origin", "main"]);
    let add = RepoAdd {
        path,
        name: Some("client".into()),
        ship: Some(ShipMode::Pr),
        base_branch: Some("main".into()),
        default_pipeline: Some(PIPELINE.into()),
        auto_merge: true,
    };
    h.cli(Request::RepoAdd(add)).await.expect("repo add");
}

fn member(repo: &str) -> InitiativeMember {
    InitiativeMember {
        repo: RepoRef(repo.into()),
        brief: format!("Change {repo} to account_id"),
        title: None,
        pipeline: Some(PIPELINE.into()),
        model_plan: None,
        autonomy: Default::default(),
        after: Vec::new(),
    }
}

async fn atomic_initiative(h: &Harness) -> InitiativeDetail {
    let request = EnqueueInitiative {
        title: "Rename the account field".into(),
        contract: "Every API uses account_id.".into(),
        atomic: true,
        idempotency_key: None,
        members: vec![member(REPO_NAME), member("client")],
    };
    let enqueued = h.cli(Request::EnqueueInitiative(request)).await;
    match enqueued.expect("enqueue") {
        Reply::Initiative(detail) => *detail,
        other => panic!("{other:?}"),
    }
}

fn finished_initiatives(held: &[TriggerEvent]) -> usize {
    let finished = |event: &&TriggerEvent| matches!(event, TriggerEvent::InitiativeFinished { .. });
    held.iter().filter(finished).count()
}

/// Atomic members open as drafts, readied together once all passed and never
/// auto-merged; a passing member is routine, the finished initiative wakes.
#[tokio::test]
async fn an_atomic_initiative_opens_drafts_and_readies_them_together() {
    let h = delivering().await;
    add_client(&h).await;
    let detail = atomic_initiative(&h).await;
    let (first, second) = (&detail.members[0].id, &detail.members[1].id);

    let client = ship(&h, second).await;
    let created = creation(&h, &client.branch);
    assert!(created.contains(&"--draft".to_owned()), "{created:?}");
    assert_eq!(prs(&h)[&client.branch]["isDraft"], true);
    let merges = |h: &Harness| gh_calls(h).iter().filter(|c| c[1] == "merge").count();
    assert_eq!(merges(&h), 0, "a draft is not handed to auto-merge");
    let held = triggers::held(&h.ctx).await.unwrap();
    assert_eq!(finished_initiatives(&held), 0, "{held:?}");
    assert!(!triggers::due(&h.ctx).await.unwrap(), "a member is routine");

    let demo = ship(&h, first).await;
    h.dispatch().await;
    let all = prs(&h);
    for branch in [&demo.branch, &client.branch] {
        assert_eq!(all[branch]["isDraft"], false, "{all}");
        let body = all[branch]["body"].as_str().unwrap();
        assert!(body.contains("Part of Rename the account field"), "{body}");
        assert!(body.starts_with("Why, and how it was verified."), "{body}");
    }
    assert_eq!(merges(&h), 0, "the operator merges an atomic set together");
    let state = h.ctx.store.lock().await.initiative(&detail.initiative.id);
    assert_eq!(state.unwrap().initiative.state, InitiativeState::Done);
    let held = triggers::held(&h.ctx).await.unwrap();
    assert_eq!(finished_initiatives(&held), 1, "{held:?}");
    assert!(triggers::due(&h.ctx).await.unwrap(), "the initiative wakes");
    let results = held.iter().find_map(|event| match event {
        TriggerEvent::InitiativeFinished { results, .. } => Some(results),
        _ => None,
    });
    let results = results.expect("the initiative's news");
    let items: Vec<_> = results.iter().map(|result| &result.item).collect();
    assert_eq!(items, [first, second], "every member, in order");
    for result in results {
        assert_eq!(result.state, "succeeded", "{result:?}");
        assert!(result.summary.starts_with("Done: "), "{result:?}");
    }
}

async fn outcome(h: &Harness, run: &PipelineRun) -> Outcome {
    let recorded = h.ctx.store.lock().await.outcome(&run.id);
    recorded
        .expect("readable")
        .expect("a finished run has an outcome")
}

/// GitHub tells learning what became of each delivered pull request: changes
/// requested and a close correct its run, a merge is kept, neither asked again.
#[tokio::test]
async fn what_became_of_a_delivered_pull_request_is_learned_from_github() {
    let h = delivering().await;
    let kept = enqueue(&h, "the change that merges", &[]).await;
    let dropped = enqueue(&h, "the change nobody wanted", &[]).await;
    let kept = ship(&h, &kept.id).await;
    let dropped = ship(&h, &dropped.id).await;
    let calls = |h: &Harness| gh_calls(h).len();
    let before = calls(&h);
    let seen = pull_requests::observe(&h.ctx).await.unwrap();
    assert_eq!(seen, pull_requests::Observed::default(), "open, unreviewed");
    let listed = &gh_calls(&h)[before..];
    assert_eq!(listed.len(), 1, "one listing answers for both: {listed:?}");
    assert!(!listed[0].contains(&"--head".to_owned()), "{listed:?}");
    assert!(outcome(&h, &kept).await.is_win());

    let sent_back = json!("CHANGES_REQUESTED");
    set_pr(&h, &kept.branch, "reviewDecision", sent_back);
    let seen = pull_requests::observe(&h.ctx).await.unwrap();
    assert_eq!(seen.changes_requested, 1, "{seen:?}");
    let again = pull_requests::observe(&h.ctx).await.unwrap();
    assert_eq!(again.changes_requested, 0, "a correction is counted once");
    let corrected = outcome(&h, &kept).await;
    assert_eq!(corrected.corrections, [Correction::ChangesRequested]);
    assert_eq!(corrected.merged, None, "still open");

    set_pr(&h, &kept.branch, "state", json!("MERGED"));
    set_pr(&h, &dropped.branch, "state", json!("CLOSED"));
    let seen = pull_requests::observe(&h.ctx).await.unwrap();
    assert_eq!((seen.merged, seen.closed, seen.unanswered), (1, 1, 0));
    assert_eq!(outcome(&h, &kept).await.merged, Some(true));
    let closed = outcome(&h, &dropped).await;
    assert_eq!(closed.merged, Some(false));
    assert_eq!(closed.corrections, [Correction::PrClosed]);
    assert!(!closed.is_win(), "work nobody merged confirms no lesson");

    let before = calls(&h);
    let settled = pull_requests::observe(&h.ctx).await.unwrap();
    assert_eq!(settled, pull_requests::Observed::default());
    assert_eq!(calls(&h), before, "neither is asked about again");
}
