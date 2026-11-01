//! Fresh bases and refreshes (ARCHITECTURE §13): a new branch forks from
//! `origin`'s base, `refresh_item` merges a moved base into the same branch
//! and pull request with its review read from the new merge base, and a
//! delivered pull request's checks that never started are told apart from
//! checks that failed (§8.7). A bare `origin` and a fake `gh`; nothing real.

use crate::common;

use std::path::{Path, PathBuf};

use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::model::ModelPlan;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::PipelineRun;
use archon_daemon::executor;
use archon_daemon::triggers::{self, TriggerEvent};
use common::delivery::{FakeGh, add_origin, origin_of, pass_by, pushed, record, shipped_by};
use common::{Harness, REPO_NAME, REVIEW_MODEL, git};
use serde_json::{Value, json};

const PR_ONE: &str = "https://github.com/o/r/pull/1";

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// Enqueue a `direct` change whose review owes one reviewer.
async fn enqueue(h: &Harness, title: &str) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: title.into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review: Some(ReviewLevel::Standard),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// Commit `file` as `content` on `origin`'s `main` from a clone of its own;
/// the harness repository's `main` stays where it was.
fn advance_origin(h: &Harness, file: &str, content: &str) -> String {
    let clone = h.dir.path().join(format!("upstream-{file}"));
    let origin = origin_of(h);
    git(
        h.dir.path(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    std::fs::write(clone.join(file), content).unwrap();
    git(&clone, &["add", file]);
    git(&clone, &["commit", "--quiet", "-m", "Upstream change"]);
    git(&clone, &["push", "--quiet", "origin", "HEAD:main"]);
    git(&clone, &["rev-parse", "HEAD"])
}

/// The running lead commits a change and passes it with one reviewer that
/// read it from `base` (or the whole change); the dispatcher then runs.
async fn settle_reviewed(h: &Harness, base: Option<&str>) {
    settle_judged(h, base, Value::Null).await;
}

/// [`settle_reviewed`], the lead proposing the `risk` level it names.
async fn settle_judged(h: &Harness, base: Option<&str>, risk: Value) {
    let lead = h.running().await.remove(0);
    let agent = lead.aether_agent.clone().unwrap();
    let reviewer = h.backend.delegate(&agent, REVIEW_MODEL);
    let mut extra = pass_by(std::slice::from_ref(&reviewer));
    if !risk.is_null() {
        extra["risk"] = risk;
    }
    let built = shipped_by(h, &lead, "Built; reviewed", extra).await;
    let head = match &built {
        archon_core::run::StageOutcome::Completed {
            structured: Some(record),
            ..
        } => record["commit"].clone(),
        other => panic!("a completed change: {other:?}"),
    };
    let mut verdict = json!({"verdict": "pass", "head": head, "findings": []});
    if let Some(base) = base {
        verdict["base"] = json!(base);
    }
    h.backend.complete_delegated(&reviewer, verdict);
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
}

/// The running lead hands in the branch as its merge left it, naming no
/// reviewer; the dispatcher then runs.
async fn settle_merged(h: &Harness) {
    let lead = h.running().await.remove(0);
    let workspace = h.ctx.store.lock().await.run(&lead.run).unwrap().workspace;
    let built = archon_core::run::StageOutcome::Completed {
        summary: "Merged the base".into(),
        structured: Some(record(&workspace)),
    };
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
}

/// Run `act` in a worktree of `branch` made for it, then remove that worktree.
fn on_branch(h: &Harness, branch: &str, act: impl FnOnce(&Path)) {
    let tree = h.dir.path().join("by-hand");
    let path = tree.to_str().unwrap();
    git(&h.repo, &["worktree", "add", "--quiet", path, branch]);
    act(&tree);
    git(&h.repo, &["worktree", "remove", path]);
}

async fn shipped(h: &Harness, title: &str) -> (QueueItem, PipelineRun) {
    let item = enqueue(h, title).await;
    h.dispatch().await;
    settle_reviewed(h, None).await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let run = h.run(&item.id).await;
    (item, run)
}

async fn refresh(h: &Harness, item: &QueueItem) -> Result<QueueItem, ArchonError> {
    let request = Request::RefreshItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        stage_timeouts: Default::default(),
    };
    match h.orchestrator(request).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("refresh replied {other:?}"),
    }
}

/// What a refresh agent does: merge `sha`, resolving the conflict on `file`.
fn merge_by_hand(tree: &Path, sha: &str, file: &str, content: &str) {
    let merging = std::process::Command::new("git")
        .arg("-C")
        .arg(tree)
        .args([
            "-c",
            "user.name=Archon Test",
            "-c",
            "user.email=test@archon.invalid",
        ])
        .args(["merge", "--quiet", sha])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        !merging.status.success(),
        "the base conflicts with the change"
    );
    std::fs::write(tree.join(file), content).unwrap();
    git(tree, &["add", file]);
    git(tree, &["commit", "--quiet", "--no-edit"]);
}

/// A new branch forks from what GitHub's base holds, fetched through the
/// shadow, and the operator's clean checkout of `main`, behind it, is
/// fast-forwarded to it (#341) with nothing to tell.
#[tokio::test]
async fn a_new_branch_forks_from_origin_s_base_and_fast_forwards_the_clean_local_one() {
    let (h, _gh) = delivering().await;
    let upstream = advance_origin(&h, "upstream.txt", "merged on GitHub\n");
    let item = enqueue(&h, "on a fresh base").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), upstream);
    assert!(run.workspace.join("upstream.txt").exists());
    assert_eq!(
        git(&h.repo, &["rev-parse", "main"]),
        upstream,
        "fast-forwarded"
    );
    assert_eq!(git(&h.repo, &["rev-parse", "HEAD"]), upstream);
    assert!(h.repo.join("upstream.txt").exists(), "the checkout follows");
    let tracked = ["status", "--porcelain", "--untracked-files=no"];
    assert_eq!(git(&h.repo, &tracked), "", "nothing left changed");
    assert_eq!(git(&h.repo, &["branch", "--show-current"]), "main");
    assert_eq!(git(&h.repo, &["rev-parse", "origin/main"]), upstream);
    assert!(h.notices(NotificationKind::Decision).await.is_empty());
}

/// A fetch that fails forks from the local base, and the item's feed says so.
#[tokio::test]
async fn a_base_that_cannot_be_fetched_forks_from_the_local_one_with_a_warning() {
    let h = Harness::new().await;
    FakeGh::install(&h);
    let gone: PathBuf = h.dir.path().join("gone.git");
    git(
        &h.repo,
        &["remote", "add", "origin", gone.to_str().unwrap()],
    );
    let local = git(&h.repo, &["rev-parse", "main"]);
    let item = enqueue(&h, "offline").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), local);
    let said = h.notices(NotificationKind::Decision).await;
    let warned =
        |n: &&String| n.contains("Could not fetch `origin/main`") && n.contains(item.id.as_str());
    assert_eq!(said.iter().filter(warned).count(), 1, "{said:?}");
}

/// A refresh merges the moved base into the same branch, its conflict
/// resolved by its agent; a review read only from the new base covers it,
/// and it ships into the pull request the branch already has.
#[tokio::test]
async fn a_refresh_merges_the_new_base_into_the_same_branch_and_pull_request() {
    let (h, gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let local = git(&h.repo, &["rev-parse", "main"]);
    let upstream = advance_origin(&h, "change.txt", "upstream wording\n");

    let fix = refresh(&h, &owner).await.expect("queued");
    assert_eq!(fix.title, format!("Refresh {} onto main", owner.id));
    assert!(
        fix.brief.contains(&format!("git merge {upstream}")),
        "{}",
        fix.brief
    );
    assert!(
        fix.brief.contains("read-only context") && fix.brief.ends_with("> brief\n"),
        "the owner's brief is quoted, not asked for: {}",
        fix.brief
    );
    assert_eq!(
        fix.autonomy.review,
        ReviewLevel::Standard,
        "a code conflict"
    );
    assert!(fix.brief.contains("merge resolution only"), "{}", fix.brief);
    assert_eq!(
        refresh(&h, &owner).await.unwrap().id,
        fix.id,
        "asked twice, queued once"
    );
    assert_eq!(
        git(&h.repo, &["rev-parse", "main"]),
        local,
        "the operator's main stays"
    );

    h.dispatch().await;
    let fixing = h.run(&fix.id).await;
    assert_eq!(fixing.branch, run.branch, "the same branch");
    let before = git(&fixing.workspace, &["rev-parse", "HEAD"]);
    merge_by_hand(
        &fixing.workspace,
        &upstream,
        "change.txt",
        "both wordings\n",
    );
    assert_eq!(git(&fixing.workspace, &["rev-parse", "HEAD^1"]), before);
    settle_reviewed(&h, Some(&before)).await;

    let state = h.state(&fix.id).await;
    assert!(
        matches!(state, QueueState::Succeeded { .. }),
        "one read of the resolution from the merge's first parent: {state:?}"
    );
    let head = git(&h.repo, &["rev-parse", &run.branch]);
    git(&h.repo, &["merge-base", "--is-ancestor", &upstream, &head]);
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(head.clone()));
    assert_eq!(
        gh.count("pr create"),
        1,
        "never a second pull request: {:?}",
        gh.calls()
    );
    let summary = match state {
        QueueState::Succeeded { summary, .. } => summary,
        _ => unreachable!(),
    };
    assert!(
        summary.ends_with(&format!("SHIPPED: {PR_ONE} (commit {head})")),
        "{summary}"
    );
    let again = refresh(&h, &owner).await.unwrap_err().to_string();
    assert!(again.contains("nothing to refresh"), "{again}");
}

/// A refresh's prompts hand reviewers the base commit it merges, never the
/// old fork, whose diff would hold every upstream commit the merge brings in.
#[tokio::test]
async fn a_refresh_reviews_from_the_base_it_merges_not_the_old_fork() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let old_fork = git(&h.repo, &["merge-base", &run.branch, "origin/main"]);
    let upstream = advance_origin(&h, "upstream.txt", "upstream\n");
    let fix = refresh(&h, &owner).await.expect("queued");

    h.dispatch().await;
    let mut requests = h.backend.requests();
    requests.retain(|request| request.labels["archon.item"] == fix.id.as_str());
    let prompt = &requests[0].objective;
    assert!(
        prompt.contains(&format!("the base (`{upstream}`) and head commits")),
        "{prompt}"
    );
    assert!(!prompt.contains(&old_fork), "{prompt}");
}

/// A refresh judges the whole change again: its level replaces the label
/// the pull request had, lower or higher.
#[tokio::test]
async fn a_refresh_relabels_the_pull_request_by_its_own_call() {
    let (h, gh) = delivering().await;
    let owner = enqueue(&h, "the change").await;
    h.dispatch().await;
    settle_judged(&h, None, json!({"level": "low", "why": "A wording fix."})).await;
    assert_eq!(gh.pr_labels(1).0, ["risk: low"]);
    let upstream = advance_origin(&h, "change.txt", "upstream wording\n");

    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    let fixing = h.run(&fix.id).await;
    merge_by_hand(
        &fixing.workspace,
        &upstream,
        "change.txt",
        "both wordings\n",
    );
    let medium = json!({"level": "medium", "why": "The merge changes what it prints."});
    settle_judged(&h, Some(&upstream), medium).await;

    let state = h.state(&fix.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let (labels, body) = gh.pr_labels(1);
    assert_eq!(labels, ["risk: medium"], "{:?}", gh.calls());
    assert!(
        gh.calls()
            .iter()
            .any(|c| c.ends_with("--add-label risk: medium --remove-label risk: low"))
    );
    assert!(
        body.starts_with("<!-- archon-risk -->\n## Risk: medium\n"),
        "{body}"
    );
    assert_eq!(
        body.matches("<!-- archon-risk -->").count(),
        1,
        "replaced: {body}"
    );
    let stored = h.ctx.store.lock().await.item(&fix.id).unwrap().risk;
    assert_eq!(stored, Some(archon_core::risk::RiskLevel::Medium));
}

/// A refresh whose agent did not merge the base delivers nothing.
#[tokio::test]
async fn a_refresh_that_did_not_merge_the_base_pushes_nothing() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let delivered = pushed(&origin_of(&h), &run.branch);
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    settle_merged(&h).await;
    let error = match h.state(&fix.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the refresh fails: {other:?}"),
    };
    assert!(
        error.contains(&format!("has not merged `main` at {upstream}")),
        "{error}"
    );
    assert_eq!(
        pushed(&origin_of(&h), &run.branch),
        delivered,
        "nothing pushed"
    );
}

/// In `local` mode too, a refresh that did not merge the local base is refused.
#[tokio::test]
async fn a_local_refresh_that_did_not_merge_the_base_fails() {
    use archon_core::protocol::RepoPatch;
    use archon_core::repo::ShipMode;
    let h = Harness::new().await;
    let local = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch {
            ship: Some(ShipMode::Local),
            ..RepoPatch::default()
        },
    };
    h.cli(local).await.unwrap();
    let (owner, _) = shipped(&h, "the change").await;
    std::fs::write(h.repo.join("upstream.txt"), "elsewhere\n").unwrap();
    git(&h.repo, &["add", "upstream.txt"]);
    git(&h.repo, &["commit", "--quiet", "-m", "Local base moves"]);
    let moved = git(&h.repo, &["rev-parse", "main"]);
    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    settle_merged(&h).await;
    let error = match h.state(&fix.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the refresh fails: {other:?}"),
    };
    assert!(
        error.contains(&format!("has not merged `main` at {moved}")),
        "{error}"
    );
}

/// A reviewed local item whose base moved merges with no retry once its
/// refresh, a clean merge its owner's review covers, brought the branch up to date.
#[tokio::test]
async fn a_refreshed_local_branch_merges_at_the_refresh_s_reviewed_head() {
    use archon_core::merge::MergeState;
    use archon_core::protocol::RepoPatch;
    use archon_core::repo::ShipMode;
    let h = Harness::new().await;
    let local = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch {
            ship: Some(ShipMode::Local),
            ..RepoPatch::default()
        },
    };
    h.cli(local).await.unwrap();
    let (owner, run) = shipped(&h, "the change").await;
    std::fs::write(h.repo.join("upstream.txt"), "elsewhere\n").unwrap();
    git(&h.repo, &["add", "upstream.txt"]);
    git(&h.repo, &["commit", "--quiet", "-m", "Local base moves"]);
    let moved = git(&h.repo, &["rev-parse", "main"]);

    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    let fixing = h.run(&fix.id).await;
    assert_eq!(fixing.branch, run.branch, "the same branch");
    git(
        &fixing.workspace,
        &["merge", "--quiet", "--no-edit", &moved],
    );
    assert_eq!(fix.autonomy.review, ReviewLevel::None, "a clean merge");
    settle_merged(&h).await;
    let state = h.state(&fix.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let head = git(&h.repo, &["rev-parse", &run.branch]);

    let merge = Request::MergeItem {
        item: owner.id.clone(),
        ignore_checks: false,
        chat: false,
    };
    let merge = match h.cli(merge).await.expect("merge") {
        Reply::Merge(merge) => *merge,
        other => panic!("a merge request: {other:?}"),
    };
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    assert_eq!(merge.head, head, "the refreshed head, not the stale one");
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), head);
    git(&h.repo, &["merge-base", "--is-ancestor", &moved, &head]);
}

/// Only a finished item with a branch of its own is refreshed.
#[tokio::test]
async fn a_refresh_is_refused_for_work_that_is_not_finished_or_is_a_refresh() {
    let (h, _gh) = delivering().await;
    let running = enqueue(&h, "still running").await;
    h.dispatch().await;
    let refused = refresh(&h, &running).await.unwrap_err().to_string();
    assert!(
        refused.contains("only a finished item is refreshed"),
        "{refused}"
    );
    settle_reviewed(&h, None).await;
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &running).await.expect("finished now");
    let nested = refresh(&h, &fix).await.unwrap_err().to_string();
    assert!(nested.contains("refresh that item instead"), "{nested}");
}

/// The pull request a run delivered to ended and its branch carries a newer
/// one: that one is not the run's, so nothing is refreshed.
#[tokio::test]
async fn a_refresh_is_refused_once_the_run_s_own_pull_request_ended() {
    let (h, gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    gh.open(&run.branch, "main");
    let refused = refresh(&h, &owner).await.unwrap_err().to_string();
    assert!(
        refused.contains("was delivered to is not open"),
        "{refused}"
    );
    assert_eq!(gh.count("pr create"), 1, "{:?}", gh.calls());
}

/// Move the watch of `run`'s checks to `minutes` after its delivery, due now.
async fn age_watch(h: &Harness, run: &PipelineRun, minutes: i64) {
    let key = format!("ship_checks:{}", run.id);
    let mut watch: Value = serde_json::from_str(&h.kv(&key).await.expect("watched")).unwrap();
    let delivered = archon_core::now() - chrono::Duration::minutes(minutes);
    watch["delivered_at"] = json!(delivered);
    watch["next_at"] = json!("2000-01-01T00:00:00Z");
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &watch.to_string())
        .unwrap();
}

fn job(name: &str, bucket: &str, started: &str, id: u64) -> Value {
    json!({"name": name, "bucket": bucket, "startedAt": started,
        "link": format!("https://github.com/o/r/actions/runs/1/job/{id}")})
}

async fn pr_checks_news(h: &Harness) -> Vec<(String, String)> {
    let held = triggers::held(&h.ctx).await.unwrap();
    held.into_iter()
        .filter_map(|event| match event {
            TriggerEvent::PrChecks { state, detail, .. } => Some((state, detail)),
            _ => None,
        })
        .collect()
}

/// Checks GitHub never started for billing, as GitHub reports them.
fn billing_refusal(gh: &FakeGh) -> &'static str {
    let started = "2026-10-15T09:20:44Z";
    gh.set_checks(&json!([
        job("lint", "fail", started, 11),
        job("test", "fail", started, 12)
    ]));
    let why = "The job was not started because recent account payments have failed";
    gh.set_annotations(&json!([{"message": why, "annotation_level": "failure"}]));
    why
}

/// Move the quiet `not_started` line of `run`'s watch `minutes` into the past.
async fn age_not_started(h: &Harness, run: &PipelineRun, minutes: i64) {
    let key = format!("ship_checks:{}", run.id);
    let mut watch: Value = serde_json::from_str(&h.kv(&key).await.expect("watched")).unwrap();
    assert!(watch["not_started_at"].is_string(), "told quietly first");
    watch["not_started_at"] = json!(archon_core::now() - chrono::Duration::minutes(minutes));
    watch["next_at"] = json!("2000-01-01T00:00:00Z");
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &watch.to_string())
        .unwrap();
}

/// Whether each held `pr_checks` event waits quietly, oldest first.
async fn pr_checks_quiet(h: &Harness) -> Vec<bool> {
    let raw = h.kv("trigger_outbox").await.expect("news is held");
    let outbox: Value = serde_json::from_str(&raw).unwrap();
    let pending = outbox["pending"].as_array().expect("pending news");
    pending
        .iter()
        .filter(|news| news["type"] == "pr_checks")
        .map(|news| news["quiet"] == json!(true))
        .collect()
}

/// Jobs GitHub never started for billing are a `not_started` event, never a
/// failure of the change: a quiet feed line first, and the operator's
/// blocker and a wake once they stay unstarted half an hour; told once.
#[tokio::test]
async fn checks_github_never_started_are_not_reported_as_failing() {
    let (h, gh) = delivering().await;
    let (_, run) = shipped(&h, "the change").await;
    let why = billing_refusal(&gh);
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert!(
        gh.count("pr checks") == 0,
        "the first look waits for checks to appear"
    );

    age_watch(&h, &run, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].0, "not_started");
    assert!(
        news[0].1.starts_with("lint, test: The job was not started"),
        "{news:?}"
    );
    assert_eq!(pr_checks_quiet(&h).await, [true], "a quiet line");
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert!(
        !blockers.iter().any(|n| n.contains("never started")),
        "{blockers:?}"
    );

    age_not_started(&h, &run, 10).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert_eq!(pr_checks_news(&h).await.len(), 1, "not yet long enough");

    age_not_started(&h, &run, 31).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let blockers = h.notices(NotificationKind::Blocker).await;
    let told = |n: &&String| n.contains("never started") && n.contains(why) && n.contains(PR_ONE);
    assert_eq!(blockers.iter().filter(told).count(), 1, "{blockers:?}");
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 2, "{news:?}");
    assert_eq!(news[1].0, "not_started");
    assert_eq!(pr_checks_quiet(&h).await, [true, false], "then a wake");
    let failed = h.notices(NotificationKind::Outcome).await;
    assert!(
        !failed.iter().any(|n| n.contains("checks of")),
        "{failed:?}"
    );
    assert_eq!(
        h.kv(&format!("ship_checks:{}", run.id)).await,
        None,
        "told once"
    );
}

/// Under a repository's `checks = "waived"`, checks that never started are
/// a quiet feed line only: no wake, no blocker, and the watch ends.
#[tokio::test]
async fn waived_checks_that_never_started_are_a_quiet_line_only() {
    use archon_core::settings::{ChecksMode, OperatorSettings};
    let (h, gh) = delivering().await;
    let waive = Request::SetSettings {
        repo: Some(RepoRef(REPO_NAME.into())),
        settings: OperatorSettings {
            checks: Some(ChecksMode::Waived),
            ..OperatorSettings::default()
        },
    };
    h.cli(waive).await.expect("waived");
    let (_, run) = shipped(&h, "the change").await;
    billing_refusal(&gh);
    age_watch(&h, &run, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].0, "not_started");
    assert_eq!(pr_checks_quiet(&h).await, [true], "no wake");
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert!(
        !blockers.iter().any(|n| n.contains("never started")),
        "{blockers:?}"
    );
    assert_eq!(h.kv(&format!("ship_checks:{}", run.id)).await, None);
}

/// A pull request that has no checks half an hour after delivery is a quiet
/// `absent` line, watched or waived alike, and the watch ends.
#[tokio::test]
async fn absent_checks_are_a_quiet_line_watched_or_waived() {
    use archon_core::settings::{ChecksMode, OperatorSettings};
    for mode in [None, Some(ChecksMode::Waived)] {
        let (h, gh) = delivering().await;
        let settings = OperatorSettings {
            checks: mode,
            ..OperatorSettings::default()
        };
        let repo = Some(RepoRef(REPO_NAME.into()));
        h.cli(Request::SetSettings { repo, settings })
            .await
            .unwrap();
        let (_, run) = shipped(&h, "the change").await;
        gh.set_checks(&json!([]));
        age_watch(&h, &run, 3).await;
        executor::coordination::reconcile(&h.ctx).await.unwrap();
        assert!(
            pr_checks_news(&h).await.is_empty(),
            "{mode:?}: checks may yet appear"
        );

        age_watch(&h, &run, 31).await;
        executor::coordination::reconcile(&h.ctx).await.unwrap();
        let news = pr_checks_news(&h).await;
        assert_eq!(news.len(), 1, "{mode:?}: {news:?}");
        assert_eq!(news[0].0, "absent");
        assert_eq!(pr_checks_quiet(&h).await, [true], "{mode:?}: no wake");
        assert_eq!(h.kv(&format!("ship_checks:{}", run.id)).await, None);
    }
}

/// Checks that ran and failed are a `failed` event; pending ones are looked
/// at again later, and one queued half an hour never started.
#[tokio::test]
async fn failing_pending_and_queued_checks_are_told_apart() {
    let (h, gh) = delivering().await;
    let (_, run) = shipped(&h, "the change").await;
    let key = format!("ship_checks:{}", run.id);
    gh.set_checks(&json!([
        job("lint", "pass", "2026-10-15T09:20:44Z", 11),
        job("test", "pending", "0001-01-01T00:00:00Z", 12)
    ]));
    age_watch(&h, &run, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let watch: Value = serde_json::from_str(&h.kv(&key).await.expect("still watched")).unwrap();
    let next: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(watch["next_at"].clone()).unwrap();
    assert!(next > archon_core::now(), "looked at again later");
    assert!(pr_checks_news(&h).await.is_empty());

    age_watch(&h, &run, 31).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].0, "not_started");
    assert!(news[0].1.contains("no runner"), "{news:?}");

    let (_, second) = shipped(&h, "another change").await;
    gh.set_checks(&json!([job("test", "fail", "2026-10-15T09:20:44Z", 13)]));
    gh.set_annotations(&json!([{"message": "Process completed with exit code 101."}]));
    age_watch(&h, &second, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 2, "{news:?}");
    assert_eq!(news[1], ("failed".to_owned(), "failed: test".to_owned()));
    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert!(
        outcomes.iter().any(|n| n.contains("failed after delivery")),
        "{outcomes:?}"
    );
}

/// Passing checks end the watch with nothing said.
#[tokio::test]
async fn passing_checks_end_the_watch_quietly() {
    let (h, gh) = delivering().await;
    let (_, run) = shipped(&h, "the change").await;
    gh.set_checks(&json!([job("test", "pass", "2026-10-15T09:20:44Z", 11)]));
    age_watch(&h, &run, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert_eq!(h.kv(&format!("ship_checks:{}", run.id)).await, None);
    assert!(pr_checks_news(&h).await.is_empty());
}

/// A watch whose repository was removed ends quietly, and the pass returns:
/// it once held the store's lock while ending the watch and hung archond.
#[tokio::test]
async fn a_watch_of_a_removed_repository_ends_without_hanging_the_pass() {
    let (h, gh) = delivering().await;
    let (item, run) = shipped(&h, "the change").await;
    {
        let mut store = h.ctx.store.lock().await;
        store.remove_repo(item.repo.as_ref().unwrap()).unwrap();
    }
    age_watch(&h, &run, 3).await;
    let pass = executor::coordination::reconcile(&h.ctx);
    tokio::time::timeout(std::time::Duration::from_secs(30), pass)
        .await
        .expect("the pass returns instead of deadlocking on the store")
        .unwrap();
    assert_eq!(h.kv(&format!("ship_checks:{}", run.id)).await, None);
    assert_eq!(gh.count("pr checks"), 0, "{:?}", gh.calls());
    assert!(pr_checks_news(&h).await.is_empty());
}

/// Past the probes a billing refusal stops the other Actions jobs too, not a
/// check outside Actions; a probe GitHub refuses leaves its check failed.
#[tokio::test]
async fn many_unstarted_jobs_and_refused_probes_are_told_right() {
    let (h, gh) = delivering().await;
    let (_, run) = shipped(&h, "the change").await;
    let started = "2026-10-15T09:20:44Z";
    let mut jobs: Vec<Value> = (0..10)
        .map(|i| job(&format!("job{i}"), "fail", started, 20 + i))
        .collect();
    gh.set_checks(&json!(jobs));
    let why = "The job was not started because recent account payments have failed";
    gh.set_annotations(&json!([{"message": why}]));
    age_watch(&h, &run, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].0, "not_started");
    assert!(
        news[0].1.contains("job9"),
        "all ten never started: {news:?}"
    );

    let external = "https://ci.example.com/build/7";
    jobs.push(
        json!({"name": "external", "bucket": "fail", "startedAt": started, "link": external}),
    );
    gh.set_checks(&json!(jobs));
    let (_, second) = shipped(&h, "another change").await;
    age_watch(&h, &second, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 2, "{news:?}");
    let ran = ("failed".to_owned(), "failed: external".to_owned());
    assert_eq!(news[1], ran, "a check outside Actions ran and failed");

    let (_, third) = shipped(&h, "a third change").await;
    gh.set_checks(&json!([job("test", "fail", started, 40)]));
    gh.set_annotations(&json!("not annotations"));
    age_watch(&h, &third, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 3, "{news:?}");
    assert_eq!(news[2], ("failed".to_owned(), "failed: test".to_owned()));
}

/// A refresh runs on the stage defaults and `[limits]
/// refresh_stage_timeout_minutes`, not the plan, reasons and stage timeouts
/// its owner was given for other work.
#[tokio::test]
async fn a_refresh_runs_the_stage_defaults_not_its_owner_s_overrides() {
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "overridden change").await;
    let defaults = owner.model_plan.clone();
    let mut stored = h.ctx.store.lock().await.item(&owner.id).unwrap();
    let pricey = vec![common::choice(
        REVIEW_MODEL,
        archon_core::model::Effort::Max,
    )];
    assert_ne!(defaults.choices_for("implement"), Some(pricey.as_slice()));
    stored.model_plan.0.insert("implement".into(), pricey);
    stored.model_reasons = Some([("implement".into(), "a hard change".into())].into());
    stored.stage_timeouts = [("implement".to_owned(), 45)].into();
    h.ctx.store.lock().await.update_item(&stored).unwrap();
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.unwrap();
    assert_eq!(fix.pipeline, "direct");
    assert_eq!(fix.model_plan, defaults, "the stage defaults");
    assert_eq!(fix.model_reasons, None);
    assert_eq!(
        fix.stage_timeouts,
        [("implement".to_owned(), 30)].into(),
        "the refresh timeout, not the owner's 45"
    );
    h.dispatch().await;
    assert_eq!(h.running().await[0].stage_id, "implement");
}

/// A refresh asked for a plan, reasons and stage timeouts runs on them,
/// checked as an enqueue's are.
#[tokio::test]
async fn a_refresh_takes_the_plan_and_stage_timeouts_it_is_given() {
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "the change").await;
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let asked = |stage: &str, minutes: u32| Request::RefreshItem {
        item: owner.id.clone(),
        model_plan: Some(
            serde_json::from_value(
                json!({"implement": [{"model": REVIEW_MODEL, "effort": "high"}]}),
            )
            .unwrap(),
        ),
        model_reasons: Some([("implement".into(), "conflicts to resolve".into())].into()),
        stage_timeouts: [(stage.to_owned(), minutes)].into(),
    };
    let refused = h
        .orchestrator(asked("ship_it", 30))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("stage_timeouts names stage \"ship_it\""),
        "{refused}"
    );
    let two = [("medium", REVIEW_MODEL), ("low", REVIEW_MODEL)]
        .map(|(effort, model)| json!({"model": model, "effort": effort}));
    let too_many = Request::RefreshItem {
        item: owner.id.clone(),
        model_plan: Some(serde_json::from_value(json!({"implement": two})).unwrap()),
        model_reasons: None,
        stage_timeouts: Default::default(),
    };
    let refused = h.orchestrator(too_many).await.unwrap_err().to_string();
    assert!(
        refused.contains("1 to 1 model choice(s), not 2"),
        "{refused}"
    );
    let Reply::Item(fix) = h.orchestrator(asked("implement", 30)).await.unwrap() else {
        panic!("a refresh item");
    };
    let high = common::choice(REVIEW_MODEL, archon_core::model::Effort::High);
    assert_eq!(
        fix.model_plan.choices_for("implement"),
        Some([high].as_slice())
    );
    assert_eq!(
        fix.model_reasons.unwrap()["implement"],
        "conflicts to resolve"
    );
    assert_eq!(fix.stage_timeouts, [("implement".to_owned(), 30)].into());
}

#[tokio::test]
async fn a_refresh_turns_off_proposal_and_report_but_keeps_other_knobs() {
    use archon_core::autonomy::{ProposalMode, ReportMode};
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "gated change").await;
    let mut stored = h.ctx.store.lock().await.item(&owner.id).unwrap();
    stored.autonomy.proposal = ProposalMode::Lavish;
    stored.autonomy.report = ReportMode::WithPr;
    h.ctx.store.lock().await.update_item(&stored).unwrap();
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.unwrap();
    assert_eq!(fix.autonomy.proposal, ProposalMode::Off);
    assert_eq!(fix.autonomy.report, ReportMode::Off);
    assert_eq!(
        fix.autonomy.review,
        ReviewLevel::None,
        "a clean merge owes no reviewer"
    );
    assert_eq!(fix.autonomy.merge, stored.autonomy.merge);
    assert_eq!(fix.autonomy.tools, stored.autonomy.tools);
    h.dispatch().await;
    assert_eq!(h.running().await[0].stage_id, "implement");
}

#[tokio::test]
async fn a_refresh_after_a_retry_survives_template_expiry_and_legacy_missing_metadata() {
    for legacy in [false, true] {
        let (h, _gh) = delivering().await;
        let owner = enqueue(&h, "retry change").await;
        h.dispatch().await;
        h.cli(Request::CancelItem {
            item: owner.id.clone(),
            reason: None,
        })
        .await
        .unwrap();
        h.cli(Request::RetryItem {
            item: owner.id.clone(),
            model_plan: None,
            model_reasons: None,
            fresh: false,
            brief_append: None,
            stage_timeouts: Default::default(),
        })
        .await
        .unwrap();
        h.dispatch().await;
        settle_reviewed(&h, None).await;
        let run = h
            .ctx
            .store
            .lock()
            .await
            .runs_for_item(&owner.id)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            h.ctx
                .store
                .lock()
                .await
                .runs_for_item(&owner.id)
                .unwrap()
                .len(),
            2
        );
        if legacy {
            h.ctx
                .store
                .lock()
                .await
                .kv_delete(&format!("run_base:{}", run.id))
                .unwrap();
        }
        archon_daemon::prune::prune_once(&h.ctx, archon_core::now())
            .await
            .unwrap();
        assert_eq!(h.kv(&format!("run_vars:{}", run.id)).await, None);
        assert_eq!(
            h.kv(&format!("run_base:{}", run.id)).await.as_deref(),
            Some("main")
        );
        if legacy {
            h.ctx
                .store
                .lock()
                .await
                .kv_delete(&format!("run_base:{}", run.id))
                .unwrap();
        }
        advance_origin(&h, "upstream.txt", "elsewhere\n");
        let fix = refresh(&h, &owner).await.unwrap();
        h.dispatch().await;
        assert_eq!(h.run(&fix.id).await.branch, run.branch);
        assert!(matches!(h.state(&fix.id).await, QueueState::Running { .. }));
    }
}

#[tokio::test]
async fn a_refresh_defaults_missing_or_disallowed_models_and_refuses_an_empty_policy() {
    for missing in [false, true] {
        let (h, _gh) = delivering().await;
        let (owner, _) = shipped(&h, "model fallback").await;
        let mut stored = h.ctx.store.lock().await.item(&owner.id).unwrap();
        stored.model_plan.0.clear();
        if !missing {
            stored.model_plan.0.insert(
                "build".into(),
                vec![common::choice(
                    "blocked/model",
                    archon_core::model::Effort::Medium,
                )],
            );
        }
        h.ctx.store.lock().await.update_item(&stored).unwrap();
        h.repo_config("[models]\ndeny = [\"blocked/model\"]\n");
        advance_origin(&h, "upstream.txt", "elsewhere\n");
        let fix = refresh(&h, &owner).await.unwrap();
        assert_ne!(
            fix.model_plan.choices_for("implement").unwrap()[0]
                .model
                .to_string(),
            "blocked/model"
        );
        h.dispatch().await;
        assert!(matches!(h.state(&fix.id).await, QueueState::Running { .. }));
    }
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "no permitted model").await;
    h.repo_config("[models]\nallow = [\"blocked/model\"]\ndeny = [\"blocked/model\"]\n");
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let error = refresh(&h, &owner).await.unwrap_err().to_string();
    assert!(
        error.contains("no model allowed for refresh stage"),
        "{error}"
    );
    assert!(
        !h.ctx
            .store
            .lock()
            .await
            .has_pending_stack_fix(&owner.id)
            .unwrap()
    );
}

/// A clean merge owes no reviewer: the owner's passing review stands, and the
/// branch as the merge left it ships.
#[tokio::test]
async fn a_refresh_of_a_clean_merge_ships_with_no_reviewer() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    assert_eq!(fix.autonomy.review, ReviewLevel::None);

    h.dispatch().await;
    let fixing = h.run(&fix.id).await;
    let merging = ["merge", "--quiet", "--no-edit", upstream.as_str()];
    git(&fixing.workspace, &merging);
    settle_merged(&h).await;

    let state = h.state(&fix.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let head = git(&h.repo, &["rev-parse", &run.branch]);
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(head));
}

/// A refresh that owes no reviewer may not change code beyond its merge:
/// none would have read it.
#[tokio::test]
async fn a_refresh_owing_no_reviewer_fails_when_it_changes_code_beyond_the_merge() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let delivered = pushed(&origin_of(&h), &run.branch);
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    let fixing = h.run(&fix.id).await;
    let merging = ["merge", "--quiet", "--no-edit", upstream.as_str()];
    git(&fixing.workspace, &merging);
    std::fs::write(fixing.workspace.join("CHANGELOG.md"), "noted\n").unwrap();
    std::fs::write(fixing.workspace.join("fix.rs"), "fn fixed() {}\n").unwrap();
    git(&fixing.workspace, &["add", "CHANGELOG.md", "fix.rs"]);
    git(&fixing.workspace, &["commit", "--quiet", "-m", "Fix more"]);
    settle_merged(&h).await;

    let error = match h.state(&fix.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the refresh fails: {other:?}"),
    };
    assert!(
        error.contains("changes code beyond that merge: fix.rs."),
        "only the code is named: {error}"
    );
    assert_eq!(
        pushed(&origin_of(&h), &run.branch),
        delivered,
        "nothing pushed"
    );
}

/// A merge conflicting only in the changelog owes no reviewer either.
#[tokio::test]
async fn a_refresh_conflicting_only_in_the_changelog_ships_with_no_reviewer() {
    let (h, _gh) = delivering().await;
    let owner = enqueue(&h, "the change").await;
    h.dispatch().await;
    let workspace = h.run(&owner.id).await.workspace;
    std::fs::write(workspace.join("CHANGELOG.md"), "ours\n").unwrap();
    git(&workspace, &["add", "CHANGELOG.md"]);
    git(&workspace, &["commit", "--quiet", "-m", "Note the change"]);
    settle_reviewed(&h, None).await;
    let upstream = advance_origin(&h, "CHANGELOG.md", "theirs\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    assert_eq!(fix.autonomy.review, ReviewLevel::None, "prose only");

    h.dispatch().await;
    let fixing = h.run(&fix.id).await;
    merge_by_hand(
        &fixing.workspace,
        &upstream,
        "CHANGELOG.md",
        "ours\ntheirs\n",
    );
    settle_merged(&h).await;

    let state = h.state(&fix.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
}

/// A branch head no passing review handed in (a commit made after the owner
/// shipped) waives nothing: the refresh keeps its owner's review, read whole.
#[tokio::test]
async fn a_refresh_of_a_branch_moved_since_its_review_keeps_the_owner_s_review() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    on_branch(&h, &run.branch, |tree| {
        std::fs::write(tree.join("later.rs"), "fn later() {}\n").unwrap();
        git(tree, &["add", "later.rs"]);
        git(tree, &["commit", "--quiet", "-m", "Unreviewed"]);
    });
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    assert_eq!(fix.autonomy.review, ReviewLevel::Standard);
    let target = h.ctx.store.lock().await.stack_fix(&fix.id).unwrap();
    let refresh = target.and_then(|target| target.refresh).expect("a refresh");
    assert_eq!(refresh.from, None, "no start but the base");
    assert!(
        fix.brief.contains("keeps its owner's review level")
            && !fix.brief.contains("merge resolution only"),
        "{}",
        fix.brief
    );
}

/// Moving code into `docs/` removes it: a rename is read as both its paths.
#[tokio::test]
async fn a_refresh_owing_no_reviewer_fails_when_it_moves_code_into_docs() {
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "the change").await;
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    let tree = h.run(&fix.id).await.workspace;
    git(&tree, &["merge", "--quiet", "--no-edit", upstream.as_str()]);
    std::fs::create_dir(tree.join("docs")).unwrap();
    git(&tree, &["mv", "change.txt", "docs/change.txt"]);
    git(&tree, &["commit", "--quiet", "-m", "Move it"]);
    settle_merged(&h).await;

    let error = match h.state(&fix.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the refresh fails: {other:?}"),
    };
    assert!(error.contains("beyond that merge: change.txt."), "{error}");
}

/// Code a side branch brings in through a clean merge is code the refresh
/// changed too: one owing no reviewer fails.
#[tokio::test]
async fn a_refresh_owing_no_reviewer_fails_when_a_merged_side_branch_brings_code() {
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "the change").await;
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    assert_eq!(fix.autonomy.review, ReviewLevel::None);
    h.dispatch().await;
    let tree = h.run(&fix.id).await.workspace;
    git(&tree, &["checkout", "--quiet", "-b", "side"]);
    std::fs::write(tree.join("side.rs"), "fn side() {}\n").unwrap();
    git(&tree, &["add", "side.rs"]);
    git(&tree, &["commit", "--quiet", "-m", "Side"]);
    git(&tree, &["checkout", "--quiet", "-"]);
    git(&tree, &["merge", "--quiet", "--no-edit", upstream.as_str()]);
    git(&tree, &["merge", "--quiet", "--no-edit", "side"]);
    settle_merged(&h).await;

    let error = match h.state(&fix.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the refresh fails: {other:?}"),
    };
    assert!(error.contains("beyond that merge: side.rs."), "{error}");
}

/// A refresh whose branch took the base in after it was queued has nothing to
/// merge: it ends at dispatch, with no run, no checks and no reviewer.
#[tokio::test]
async fn a_refresh_with_nothing_to_merge_ends_at_once() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    let merging = ["merge", "--quiet", "--no-edit", upstream.as_str()];
    on_branch(&h, &run.branch, |tree| {
        git(tree, &merging);
    });

    h.dispatch().await;
    let reason = match h.state(&fix.id).await {
        QueueState::Cancelled {
            reason: Some(reason),
        } => reason,
        other => panic!("the refresh ends: {other:?}"),
    };
    assert!(reason.starts_with("nothing to merge"), "{reason}");
    assert!(reason.contains("no checks and no reviewer"), "{reason}");
    let runs = h.ctx.store.lock().await.runs_for_item(&fix.id).unwrap();
    assert!(runs.is_empty(), "no run started: {runs:?}");
    assert!(h.running().await.is_empty(), "no agent");
}
