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
use common::delivery::{FakeGh, add_origin, origin_of, pass_by, pushed, shipped_by};
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
    let lead = h.running().await.remove(0);
    let agent = lead.aether_agent.clone().unwrap();
    let reviewer = h.backend.delegate(&agent, REVIEW_MODEL);
    let built = shipped_by(
        h,
        &lead,
        "Built; reviewed",
        pass_by(std::slice::from_ref(&reviewer)),
    )
    .await;
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
/// shadow, while the operator's `main` and checkout stay as they were.
#[tokio::test]
async fn a_new_branch_forks_from_origin_s_base_and_leaves_the_local_one() {
    let (h, _gh) = delivering().await;
    let local = git(&h.repo, &["rev-parse", "main"]);
    let upstream = advance_origin(&h, "upstream.txt", "merged on GitHub\n");
    let item = enqueue(&h, "on a fresh base").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), upstream);
    assert!(run.workspace.join("upstream.txt").exists());
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), local, "never moved");
    assert_eq!(git(&h.repo, &["rev-parse", "HEAD"]), local);
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
    merge_by_hand(
        &fixing.workspace,
        &upstream,
        "change.txt",
        "both wordings\n",
    );
    settle_reviewed(&h, Some(&upstream)).await;

    let state = h.state(&fix.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
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

/// A refresh whose agent did not merge the base delivers nothing.
#[tokio::test]
async fn a_refresh_that_did_not_merge_the_base_pushes_nothing() {
    let (h, _gh) = delivering().await;
    let (owner, run) = shipped(&h, "the change").await;
    let delivered = pushed(&origin_of(&h), &run.branch);
    let upstream = advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.expect("queued");
    h.dispatch().await;
    settle_reviewed(&h, None).await;
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
    settle_reviewed(&h, None).await;
    let error = match h.state(&fix.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the refresh fails: {other:?}"),
    };
    assert!(
        error.contains(&format!("has not merged `main` at {moved}")),
        "{error}"
    );
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

/// Jobs GitHub never started for billing are the operator's blocker and a
/// `not_started` event, never a failure of the change; told once.
#[tokio::test]
async fn checks_github_never_started_are_not_reported_as_failing() {
    let (h, gh) = delivering().await;
    let (_, run) = shipped(&h, "the change").await;
    let started = "2026-10-15T09:20:44Z";
    gh.set_checks(&json!([
        job("lint", "fail", started, 11),
        job("test", "fail", started, 12)
    ]));
    let why = "The job was not started because recent account payments have failed";
    gh.set_annotations(&json!([{"message": why, "annotation_level": "failure"}]));
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert!(
        gh.count("pr checks") == 0,
        "the first look waits for checks to appear"
    );

    age_watch(&h, &run, 3).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let blockers = h.notices(NotificationKind::Blocker).await;
    let told = |n: &&String| n.contains("never started") && n.contains(why) && n.contains(PR_ONE);
    assert_eq!(blockers.iter().filter(told).count(), 1, "{blockers:?}");
    let news = pr_checks_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].0, "not_started");
    assert!(
        news[0].1.starts_with("lint, test: The job was not started"),
        "{news:?}"
    );
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

/// Steering changes a run's plan without changing the item's pipeline name.
#[tokio::test]
async fn a_refresh_maps_a_steered_build_and_its_reviewers_to_implement() {
    let (h, _gh) = delivering().await;
    let (owner, _) = shipped(&h, "steered change").await;
    let mut stored = h.ctx.store.lock().await.item(&owner.id).unwrap();
    let implement = stored.model_plan.0.remove("implement").unwrap();
    stored
        .model_plan
        .0
        .insert("build".into(), implement.clone());
    let reviewers = vec![common::choice(
        REVIEW_MODEL,
        archon_core::model::Effort::Medium,
    )];
    stored
        .model_plan
        .0
        .insert("build.review".into(), reviewers.clone());
    stored.model_reasons = Some([("build".into(), "retain the implementer".into())].into());
    h.ctx.store.lock().await.update_item(&stored).unwrap();
    advance_origin(&h, "upstream.txt", "elsewhere\n");
    let fix = refresh(&h, &owner).await.unwrap();
    assert_eq!(fix.pipeline, "direct");
    assert_eq!(
        fix.model_plan.choices_for("implement"),
        Some(implement.as_slice())
    );
    assert_eq!(
        fix.model_plan.choices_for("implement.review"),
        Some(reviewers.as_slice())
    );
    assert!(!fix.model_plan.0.contains_key("build"));
    assert_eq!(
        fix.model_reasons.unwrap()["implement"],
        "retain the implementer"
    );
    h.dispatch().await;
    assert_eq!(h.running().await[0].stage_id, "implement");
    assert!(matches!(h.state(&fix.id).await, QueueState::Running { .. }));
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
    assert_eq!(fix.autonomy.review, stored.autonomy.review);
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
