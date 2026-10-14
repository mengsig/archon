//! The delivery archond makes itself (ARCHITECTURE §8.7): no agent ships a
//! built-in pipeline. What is proved before anything leaves the machine,
//! the pull request found or opened once, a restart mid-delivery, the review
//! level an item asks for, `direct`, and a stage that escalates.

mod common;

use std::path::Path;
use std::process::Command;

use archon_core::autonomy::{AutonomyPatch, MergeMode, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::model::ModelPlan;
use archon_core::notify::NotificationKind;
use archon_core::ports::DescendantState;
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_daemon::executor;
use archon_daemon::triggers::{self, TriggerEvent};
use common::delivery::{FakeGh, add_origin, change, origin_of, pushed, record, shipped_by};
use common::{Harness, REPO_NAME, REVIEW_MODEL, git, verdict};
use serde_json::{Value, json};

const PR_ONE: &str = "https://github.com/o/r/pull/1";

/// A harness whose repository has an `origin` and whose `gh` keeps the pull
/// requests archond opens.
async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// Enqueue into `pipeline` with no model plan, asking for `autonomy`.
async fn enqueue(
    h: &Harness,
    pipeline: &str,
    autonomy: AutonomyPatch,
) -> Result<QueueItem, ArchonError> {
    let request = EnqueueRequest {
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: format!("a change through {pipeline}"),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy,
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

/// An item on `pipeline`, dispatched: its first stage's agent runs.
async fn started(h: &Harness, pipeline: &str) -> (QueueItem, PipelineRun) {
    let item = enqueue(h, pipeline, AutonomyPatch::default())
        .await
        .expect("enqueue");
    h.dispatch().await;
    let run = h.run(&item.id).await;
    (item, run)
}

/// The one running stage commits a change and completes with its ship
/// record, plus `extra`; then the dispatcher runs once.
async fn settle_changed(h: &Harness, extra: Value) -> StageRun {
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(h, &stage, "Changed it; cargo test passes", extra).await;
    let settled = h.settle_running(|_| outcome.clone()).await;
    assert_eq!(settled.len(), 1, "one agent ran");
    h.dispatch().await;
    stage
}

/// The one running stage completes with `structured`, written beforehand.
async fn settle_with(h: &Harness, structured: Value) {
    let outcome = StageOutcome::Completed {
        summary: "Changed it".into(),
        structured: Some(structured),
    };
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
}

async fn succeeded(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Succeeded { summary, .. } => summary,
        other => panic!("the item ships: {other:?}"),
    }
}

async fn failed(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the item fails: {other:?}"),
    }
}

fn stage_labels(h: &Harness) -> Vec<String> {
    let requests = h.backend.requests();
    let label = |r: &archon_core::ports::SpawnRequest| r.labels["archon.stage"].clone();
    requests.iter().map(label).collect()
}

/// The acceptance case: one agent implements, archond pushes the branch and
/// opens the pull request, and GitHub's URL is what the run records.
#[tokio::test]
async fn archond_opens_the_pull_request_and_no_agent_ships() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    // Scratch files an agent leaves untracked are not part of the change.
    std::fs::write(run.workspace.join("notes.tmp"), "scratch").unwrap();
    let forged = json!({"pr_url": "https://github.com/evil/fork/pull/9"});
    let stage = settle_changed(&h, forged).await;

    let head = git(&h.repo, &["rev-parse", &run.branch]);
    let summary = succeeded(&h, &item).await;
    assert!(
        summary.ends_with(&format!("SHIPPED: {PR_ONE} (commit {head})")),
        "{summary}"
    );
    assert_eq!(stage_labels(&h), ["implement"], "no agent ships");
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(head));
    assert_eq!(gh.count("pr create"), 1);
    let create = format!("pr create --base main --head {}", run.branch);
    assert!(gh.calls().iter().any(|c| c.starts_with(&create)));
    let (title, body) = gh.created();
    assert_eq!(title, "Change the thing");
    assert!(body.starts_with("Why, and how it was verified."), "{body}");
    assert!(
        body.ends_with(&format!("Archon item: {}\n", item.id)),
        "{body}"
    );

    let stages = h.stages(&item.id).await;
    assert_eq!(archon_core::run::pr_url(stages.iter()), Some(PR_ONE));
    let stored = h.ctx.store.lock().await.stage_run(&stage.id).unwrap();
    assert_eq!(
        stored.structured.unwrap()["pr_url"],
        PR_ONE,
        "never the agent's"
    );
    assert!(h.run(&item.id).await.shipped);
    let held = triggers::held(&h.ctx).await.unwrap();
    assert!(held.iter().any(|event| matches!(event,
        TriggerEvent::ItemFinished { item: id, pr_url: Some(url), .. }
            if id == &item.id && url == PR_ONE)));
}

/// GitHub opened the pull request and its answer was lost: the run stays
/// live, and the archond that restarts finds that pull request, not a second.
#[tokio::test]
async fn a_restart_mid_delivery_resumes_without_a_second_pull_request() {
    let (mut h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    gh.lose_next_create_reply();
    settle_changed(&h, json!({})).await;
    assert_eq!(gh.count("pr create"), 1);
    assert!(
        matches!(h.state(&item.id).await, QueueState::Running { .. }),
        "an outage fails nothing: {:?}",
        h.state(&item.id).await
    );
    let key = format!("ship:{}", run.id);
    assert_eq!(h.kv(&key).await, None, "nothing was delivered for certain");
    let lookups = gh.count("pr list");
    h.dispatch().await;
    assert_eq!(gh.count("pr list"), lookups, "the next try waits its turn");

    h.restart();
    executor::recover(&h.ctx).await.unwrap();
    assert_eq!(
        gh.count("pr list"),
        lookups,
        "a start is not held up by GitHub"
    );
    h.dispatch().await;
    let summary = succeeded(&h, &item).await;
    assert!(summary.contains(&format!("SHIPPED: {PR_ONE}")), "{summary}");
    assert_eq!(gh.count("pr create"), 1, "the open pull request is reused");
    assert_eq!(stage_labels(&h), ["implement"], "and no stage runs again");
    let head = git(&h.repo, &["rev-parse", &run.branch]);
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(head));
}

/// The outcome is stored before the run finishes: an archond that died in
/// between replays it, and asks GitHub nothing again.
#[tokio::test]
async fn a_recorded_delivery_is_replayed_after_a_restart_not_repeated() {
    let (mut h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    gh.fail_next("pr list", "HTTP 502: Bad Gateway");
    settle_changed(&h, json!({})).await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    let head = git(&h.repo, &["rev-parse", &run.branch]);
    let recorded = json!({"outcome": "shipped", "commit": head,
        "pr_url": "https://github.com/o/r/pull/9"});
    let key = format!("ship:{}", run.id);
    let stored = h.ctx.store.lock().await.kv_set(&key, &recorded.to_string());
    stored.unwrap();
    let calls = gh.calls().len();

    h.restart();
    executor::recover(&h.ctx).await.unwrap();
    h.dispatch().await;
    let summary = succeeded(&h, &item).await;
    assert!(
        summary.contains("SHIPPED: https://github.com/o/r/pull/9"),
        "{summary}"
    );
    assert_eq!(
        gh.calls().len(),
        calls,
        "a recorded delivery is not made again"
    );
    assert_eq!(gh.count("pr create"), 0);
}

/// The tree that ships is the tree that was verified: a commit made after
/// the record was written is refused, and nothing leaves the machine.
#[tokio::test]
async fn a_tree_that_changed_after_the_ship_record_is_refused() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    let verified = change(&run.workspace);
    change(&run.workspace);
    settle_with(&h, verified.clone()).await;

    let error = failed(&h, &item).await;
    assert!(
        error.starts_with("stage ship: the run branch holds tree"),
        "{error}"
    );
    let tree = verified["tree"].as_str().unwrap();
    assert!(
        error.contains(&format!("not the tree {tree} that was verified")),
        "{error}"
    );
    assert!(
        error.contains("changed after the ship record was written"),
        "{error}"
    );
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(
        gh.calls().is_empty(),
        "GitHub is never asked: {:?}",
        gh.calls()
    );
    assert!(!h.run(&item.id).await.shipped);
}

/// A tracked change left uncommitted would not ship with the commits.
#[tokio::test]
async fn uncommitted_changes_refuse_the_delivery() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    let verified = change(&run.workspace);
    std::fs::write(run.workspace.join("README.md"), "edited, not committed\n").unwrap();
    settle_with(&h, verified).await;

    let error = failed(&h, &item).await;
    assert!(
        error.contains("the run worktree has uncommitted changes"),
        "{error}"
    );
    assert!(error.contains("README.md"), "{error}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty());
}

/// A commit made under an identity the agent set itself is not the
/// repository's: it is named, and the branch is not pushed.
#[tokio::test]
async fn a_commit_under_another_identity_refuses_the_delivery() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    std::fs::write(run.workspace.join("change.txt"), "a change\n").unwrap();
    git(&run.workspace, &["add", "change.txt"]);
    let committed = Command::new("git")
        .arg("-C")
        .arg(&run.workspace)
        .args(["commit", "--quiet", "-m", "Change the thing"])
        .env("GIT_AUTHOR_EMAIL", "agent@elsewhere.invalid")
        .env("GIT_COMMITTER_EMAIL", "agent@elsewhere.invalid")
        .status()
        .unwrap();
    assert!(committed.success());
    settle_with(&h, record(&run.workspace)).await;

    let error = failed(&h, &item).await;
    assert!(
        error.contains("1 commit(s) on the run branch were not made under this repository's git identity (<test@archon.invalid>)"),
        "{error}"
    );
    assert!(
        error.contains("author <agent@elsewhere.invalid>"),
        "{error}"
    );
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty());
}

/// A record that names commits the branch does not add ships nothing.
#[tokio::test]
async fn a_branch_with_no_commit_of_its_own_has_nothing_to_ship() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    settle_with(&h, record(&run.workspace)).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("nothing to ship"), "{error}");
    assert!(error.contains("has no commit beyond `main`"), "{error}");
    assert!(gh.calls().is_empty());
}

/// A stage that leaves no usable record is told what is missing before
/// anything is delivered.
#[tokio::test]
async fn a_missing_ship_record_fails_the_stage_that_owes_it() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    let mut partial = change(&run.workspace);
    partial.as_object_mut().unwrap().remove("pr");
    settle_with(&h, partial).await;
    let error = failed(&h, &item).await;
    assert!(
        error.contains("the ship record archond delivers is not on record"),
        "{error}"
    );
    assert!(error.contains("`pr` is missing"), "{error}");
    assert!(gh.calls().is_empty());
}

/// `direct` is the cheapest path: no model plan, one leaf agent, no review,
/// and a pull request archond opened.
#[tokio::test]
async fn direct_runs_one_agent_from_enqueue_to_pull_request() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    assert_eq!(item.autonomy.review, ReviewLevel::None);
    let request = h.backend.requests().remove(0);
    assert_eq!(request.labels["archon.stage"], "implement");
    assert!(
        request.leaf,
        "with no review to delegate, it delegates nothing"
    );
    assert!(request.instructions.contains("\n## Ship record\n"));
    assert!(!request.instructions.contains("## Review on record"));
    assert!(!request.instructions.contains("## Delegation"));

    settle_changed(&h, json!({})).await;
    let summary = succeeded(&h, &item).await;
    assert!(summary.contains(&format!("SHIPPED: {PR_ONE}")), "{summary}");
    assert_eq!(h.backend.created(), 1, "one agent, end to end");
    assert_eq!(gh.count("pr create"), 1);
    assert!(pushed(&origin_of(&h), &run.branch).is_some());
    let (_, body) = gh.created();
    assert!(
        body.contains("REVIEW: none, as this item's review level asks"),
        "{body}"
    );
}

/// The running lead delegates `count` reviewers that complete, and settles
/// with a change, its ship record and a pass naming `named` of them.
async fn settle_reviewed_by(h: &Harness, count: usize, named: usize) -> (StageRun, Vec<String>) {
    let lead = h.running().await.remove(0);
    let agent = lead.aether_agent.clone().unwrap();
    let mut reviewers = Vec::new();
    for _ in 0..count {
        let reviewer = h.backend.delegate(&agent, REVIEW_MODEL);
        h.backend
            .end_delegated(&reviewer, DescendantState::Completed);
        reviewers.push(reviewer.to_string());
    }
    let models = vec![REVIEW_MODEL; named];
    let review = json!({"review": {"verdict": "pass", "reviewer_models": models}});
    let built = shipped_by(h, &lead, "Built; reviewed", review).await;
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    (lead, reviewers)
}

fn asking(review: ReviewLevel) -> AutonomyPatch {
    AutonomyPatch {
        review: Some(review),
        ..AutonomyPatch::default()
    }
}

/// The review level is the item's: `none`, `standard` and `strict` make the
/// lead of `adaptive` owe 0, 1 and 3 reviewers, each of which Archon finds.
#[tokio::test]
async fn the_review_level_decides_how_many_reviewers_adaptive_must_show() {
    for (level, owed, told) in [
        (
            ReviewLevel::None,
            0,
            "review level is `none`: delegate no reviewer",
        ),
        (
            ReviewLevel::Standard,
            1,
            "reviewed by an agent you delegate to",
        ),
        (
            ReviewLevel::Strict,
            3,
            "reviewed by 3 agents you delegate to",
        ),
    ] {
        let (h, gh) = delivering().await;
        let item = enqueue(&h, "adaptive", asking(level)).await.unwrap();
        h.dispatch().await;
        let contract = h.backend.requests().remove(0).instructions;
        assert!(contract.contains(told), "{level}: {contract}");

        let (lead, reviewers) = settle_reviewed_by(&h, owed, owed).await;
        let summary = succeeded(&h, &item).await;
        assert!(
            summary.contains(&format!("SHIPPED: {PR_ONE}")),
            "{level}: {summary}"
        );
        let stored = h.ctx.store.lock().await.stage_run(&lead.id).unwrap();
        let review = &stored.structured.as_ref().unwrap()["review"];
        let (_, body) = gh.created();
        if owed == 0 {
            assert_eq!(review.get("reviewer_agents"), None, "none was asked for");
            assert!(body.contains("REVIEW: none, as this item's review level asks"));
            continue;
        }
        let mut found: Vec<String> = serde_json::from_value(review["reviewer_agents"].clone())
            .unwrap_or_else(|e| panic!("{level}: {e}: {review}"));
        found.sort();
        assert_eq!(
            found, reviewers,
            "{level}: every reviewer is a different agent"
        );
        assert_eq!(body.matches("(agent agt_child").count(), owed, "{body}");
    }
}

/// Fewer reviewers than the level asks for is no review: the item fails at
/// the lead's stage, and nothing is delivered.
#[tokio::test]
async fn too_few_reviewers_for_the_level_fail_the_item() {
    for (named, why) in [
        (
            2,
            "it names 2 reviewer(s), and this item's review level needs 3",
        ),
        (
            3,
            "no further agent the lead delegated to completed on that model",
        ),
    ] {
        let (h, gh) = delivering().await;
        let item = enqueue(&h, "adaptive", asking(ReviewLevel::Strict))
            .await
            .unwrap();
        h.dispatch().await;
        settle_reviewed_by(&h, 2, named).await;
        let error = failed(&h, &item).await;
        assert!(
            error.contains("the review this pipeline declares is not on record"),
            "{error}"
        );
        assert!(error.contains(why), "{error}");
        assert!(
            gh.calls().is_empty(),
            "nothing is delivered: {:?}",
            gh.calls()
        );
    }
}

/// `direct` holds its review too: asked for one, its agent may delegate, is
/// told to, and Archon checks the reviewer like any lead's.
#[tokio::test]
async fn direct_delegates_a_reviewer_when_the_item_asks_for_one() {
    let (h, gh) = delivering().await;
    let item = enqueue(&h, "direct", asking(ReviewLevel::Standard))
        .await
        .unwrap();
    h.dispatch().await;
    let request = h.backend.requests().remove(0);
    assert!(!request.leaf, "its reviewer is an agent it delegates to");
    assert!(request.instructions.contains("\n## Review on record\n"));

    let unreviewed = h.running().await.remove(0);
    let alone = shipped_by(&h, &unreviewed, "Changed it", json!({})).await;
    h.settle_running(|_| alone.clone()).await;
    h.dispatch().await;
    let error = failed(&h, &item).await;
    assert!(
        error.contains("the review this pipeline declares is not on record"),
        "{error}"
    );
    assert!(gh.calls().is_empty());

    let again = enqueue(&h, "direct", asking(ReviewLevel::Standard))
        .await
        .unwrap();
    h.dispatch().await;
    let (_, reviewers) = settle_reviewed_by(&h, 1, 1).await;
    succeeded(&h, &again).await;
    let (_, body) = gh.created();
    let line = format!("REVIEW: pass, by {REVIEW_MODEL} (agent {})", reviewers[0]);
    assert!(body.contains(&line), "{body}");
}

/// A stage that finds the work needs another shape hands the item back: it
/// fails at once, with no fix loop, and the orchestrator is told why.
#[tokio::test]
async fn an_escalating_stage_fails_the_item_with_what_it_recommends() {
    let (h, gh) = delivering().await;
    let (item, _) = started(&h, "default-pr-review").await;
    settle_changed(&h, json!({})).await;
    assert_eq!(stage_labels(&h), ["implement", "review"]);
    let advice = "ESCALATE: this rewrites the session token check; run it through \
                  strict-pr-review";
    h.settle_running(|_| StageOutcome::Failed {
        error: advice.into(),
        infra: false,
    })
    .await;
    h.dispatch().await;

    let error = failed(&h, &item).await;
    assert_eq!(error, format!("stage review: {advice}"));
    assert_eq!(
        stage_labels(&h),
        ["implement", "review"],
        "no fix loop runs"
    );
    assert!(
        gh.calls().is_empty(),
        "an escalated change is not delivered"
    );
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
    let held = triggers::held(&h.ctx).await.unwrap();
    assert!(held.iter().any(|event| matches!(event,
        TriggerEvent::ItemFinished { item: id, state, summary, .. }
            if id == &item.id && state == "failed" && summary.contains(advice))));
}

async fn update_repo(h: &Harness, patch: RepoPatch) {
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

/// `local` ship mode proves the same worktree and stops there: nothing is
/// pushed and GitHub is never asked.
#[tokio::test]
async fn local_mode_delivers_on_the_run_branch_and_nothing_leaves_the_machine() {
    let (h, gh) = delivering().await;
    let local = RepoPatch {
        ship: Some(ShipMode::Local),
        ..RepoPatch::default()
    };
    update_repo(&h, local).await;
    let (item, run) = started(&h, "quick-pr").await;
    settle_changed(&h, json!({})).await;

    let head = git(&h.repo, &["rev-parse", &run.branch]);
    let summary = succeeded(&h, &item).await;
    let shipped = format!("SHIPPED: commit {head} on the run branch");
    assert!(summary.ends_with(&shipped), "{summary}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    let stages = h.stages(&item.id).await;
    assert_eq!(archon_core::run::pr_url(stages.iter()), None);
    assert!(h.run(&item.id).await.shipped);
}

/// The same proof holds in `local` mode: a changed tree is refused there too.
#[tokio::test]
async fn local_mode_refuses_a_tree_that_changed() {
    let (h, _gh) = delivering().await;
    let local = RepoPatch {
        ship: Some(ShipMode::Local),
        ..RepoPatch::default()
    };
    update_repo(&h, local).await;
    let (item, run) = started(&h, "quick-pr").await;
    let verified = change(&run.workspace);
    change(&run.workspace);
    settle_with(&h, verified).await;
    let error = failed(&h, &item).await;
    assert!(
        error.contains("changed after the ship record was written"),
        "{error}"
    );
}

/// A pull request open already for the branch is the run's: it is reused,
/// and moved onto the base the change is for.
#[tokio::test]
async fn an_open_pull_request_is_reused_and_moved_onto_the_base() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    gh.open(&run.branch, "some-old-base");
    settle_changed(&h, json!({})).await;

    let summary = succeeded(&h, &item).await;
    let url = "https://github.com/o/r/pull/7";
    assert!(summary.contains(&format!("SHIPPED: {url}")), "{summary}");
    assert_eq!(gh.count("pr create"), 0, "never a second pull request");
    let moved = format!("pr edit {url} --base main");
    assert!(gh.calls().contains(&moved), "{:?}", gh.calls());
}

/// A run stacked on another item's branch delivers onto that branch: the
/// base its start recorded, not the repository's.
#[tokio::test]
async fn a_stacked_run_delivers_onto_its_parent_s_branch() {
    let (h, gh) = delivering().await;
    git(&h.repo, &["branch", "archon/parent", "main"]);
    let (item, run) = started(&h, "quick-pr").await;
    let key = format!("run_vars:{}", run.id);
    let mut vars: Value = serde_json::from_str(&h.kv(&key).await.unwrap()).unwrap();
    vars["stack_parent"] = "archon/parent".into();
    let stored = h.ctx.store.lock().await.kv_set(&key, &vars.to_string());
    stored.unwrap();
    settle_changed(&h, json!({})).await;

    succeeded(&h, &item).await;
    let create = format!("pr create --base archon/parent --head {}", run.branch);
    assert!(
        gh.calls().iter().any(|c| c.starts_with(&create)),
        "{:?}",
        gh.calls()
    );
}

/// An item that asks for it has auto-merge enabled once its pull request is
/// open; GitHub refusing that leaves the change delivered and says so.
#[tokio::test]
async fn auto_merge_is_enabled_on_request_and_its_refusal_fails_nothing() {
    let (h, gh) = delivering().await;
    let auto = AutonomyPatch {
        merge: Some(MergeMode::Auto),
        ..AutonomyPatch::default()
    };
    let ceiling = RepoPatch {
        autonomy_max: Some(auto),
        ..RepoPatch::default()
    };
    update_repo(&h, ceiling).await;
    let first = enqueue(&h, "quick-pr", auto).await.unwrap();
    h.dispatch().await;
    settle_changed(&h, json!({})).await;
    succeeded(&h, &first).await;
    let merge = format!("pr merge {PR_ONE} --auto --squash");
    assert!(gh.calls().contains(&merge), "{:?}", gh.calls());
    assert!(
        h.notices(NotificationKind::Progress)
            .await
            .iter()
            .all(|n| !n.contains("auto-merge"))
    );

    let second = enqueue(&h, "quick-pr", auto).await.unwrap();
    h.dispatch().await;
    gh.fail_next("pr merge", "Pull request is in clean status");
    settle_changed(&h, json!({})).await;
    succeeded(&h, &second).await;
    let notices = h.notices(NotificationKind::Progress).await;
    let told =
        |n: &&String| n.contains("auto-merge could not be enabled") && n.contains("clean status");
    assert_eq!(notices.iter().filter(told).count(), 1, "{notices:?}");
}

/// A commit `origin` holds on the run branch and the run does not is never
/// overwritten: the push is refused as it stands.
#[tokio::test]
async fn a_remote_branch_holding_other_commits_is_never_forced() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    let tree = git(&h.repo, &["rev-parse", "main^{tree}"]);
    let theirs = git(
        &h.repo,
        &["commit-tree", &tree, "-p", "main", "-m", "theirs"],
    );
    let onto = format!("{theirs}:refs/heads/{}", run.branch);
    git(&h.repo, &["push", "--quiet", "origin", &onto]);
    settle_changed(&h, json!({})).await;

    let error = failed(&h, &item).await;
    assert!(error.contains("already has a"), "{error}");
    assert!(error.contains("Archon never force-pushes"), "{error}");
    assert_eq!(
        pushed(&origin_of(&h), &run.branch),
        Some(theirs),
        "left as it was"
    );
    assert!(
        gh.calls().is_empty(),
        "no pull request for an unpushed branch"
    );
}

/// Without an `origin` only the operator can help: the item fails as a
/// blocker that says what to set.
#[tokio::test]
async fn a_repository_without_an_origin_blocks_for_the_operator() {
    let h = Harness::new().await;
    let gh = FakeGh::install(&h);
    let (item, _) = started(&h, "quick-pr").await;
    settle_changed(&h, json!({})).await;
    let error = failed(&h, &item).await;
    assert!(
        error.contains("BLOCKED: the repository has no `origin` remote"),
        "{error}"
    );
    assert!(error.contains("ship mode to `local`"), "{error}");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
    assert!(gh.calls().is_empty());
}

/// GitHub refusing for want of access is the operator's to fix, not a retry.
#[tokio::test]
async fn a_github_login_that_lapsed_blocks_for_the_operator() {
    let (h, gh) = delivering().await;
    let (item, _) = started(&h, "quick-pr").await;
    gh.fail_next(
        "pr list",
        "To get started with GitHub CLI, please run:  gh auth login",
    );
    settle_changed(&h, json!({})).await;
    let error = failed(&h, &item).await;
    assert!(
        error.starts_with("stage ship: BLOCKED: looking up the pull request"),
        "{error}"
    );
    assert!(error.contains("gh auth login"), "{error}");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
}

/// Review stages of a pipeline's own run before the delivery, and what they
/// said goes into the pull request the daemon opens.
#[tokio::test]
async fn a_pipeline_with_review_stages_ships_what_its_reviewers_passed() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "default-pr-review").await;
    settle_changed(&h, json!({})).await;
    assert!(gh.calls().is_empty(), "nothing ships before the review");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;

    succeeded(&h, &item).await;
    assert_eq!(stage_labels(&h), ["implement", "review"]);
    let (_, body) = gh.created();
    assert!(body.contains("## Review and verification"), "{body}");
    assert!(body.contains("### `review`"), "{body}");
    assert!(body.contains("VERDICT: pass"), "{body}");
}

/// What the worktree holds is read with the repository's own git: a path
/// outside the run's worktree is never shipped from.
#[tokio::test]
async fn a_worktree_that_is_gone_refuses_the_delivery() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "quick-pr").await;
    let verified = change(&run.workspace);
    remove_worktree(&h.repo, &run.workspace);
    settle_with(&h, verified).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("the run worktree"), "{error}");
    assert!(error.contains("is gone"), "{error}");
    assert!(gh.calls().is_empty());
}

fn remove_worktree(repo: &Path, workspace: &Path) {
    let path = workspace.to_str().expect("utf-8");
    git(repo, &["worktree", "remove", "--force", path]);
    assert!(!workspace.exists());
}

/// A model plan is optional for every built-in, and none of them names a
/// ship agent: where a built-in ships, archond does.
#[tokio::test]
async fn every_builtin_runs_without_a_model_plan_or_a_ship_agent() {
    let h = Harness::new().await;
    let catalog = archon_pipeline::Catalog::builtin();
    let mut shipping = 0;
    for pipeline in catalog.listing().pipelines {
        let name = &pipeline.name;
        let spec = catalog.get(name).unwrap();
        let item = enqueue(&h, name, AutonomyPatch::default()).await;
        let item = item.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            !item.model_plan.0.is_empty(),
            "{name}: every stage is filled"
        );
        assert!(!item.model_plan.0.contains_key("ship"), "{name}");
        let by_daemon = spec.ship_record_stage().is_some();
        assert_eq!(by_daemon, archon_pipeline::autonomy::ships(spec), "{name}");
        shipping += usize::from(by_daemon);
    }
    assert!(shipping >= 12, "the shipping built-ins: {shipping}");
}
