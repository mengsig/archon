//! What keeps archond's own delivery (ARCHITECTURE §8.7) honest: the stages
//! of a delivering run no longer change, a steer never leaves a run with
//! nothing to ship, and no program the repository, its worktree or its
//! `origin` configures runs as the daemon.

mod common;

use archon_core::autonomy::AutonomyPatch;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::ModelPlan;
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, StageOutcome};
use archon_core::steer::{InsertStage, Steer, Then};
use common::delivery::{FakeGh, add_origin, change, origin_of, plant_everywhere, pushed, record};
use common::{Harness, REPO_NAME, git};
use serde_json::{Value, json};

/// A pipeline archond delivers from its second stage's record.
const RECORDED: &str = r#"[pipeline]
name = "recorded"
description = "plan, then implement what archond ships"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.ship]
stage = "implement"

[[stage]]
id = "plan"
kind = "work"
prompt = "plan.md"
timeout_minutes = 30

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30
"#;

/// Stages a steer may bring, none of which ships.
const EXTRA: &str = r#"[pipeline]
name = "extra"
description = "stages to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "plan"
kind = "work"
prompt = "plan.md"
timeout_minutes = 30

[[stage]]
id = "verify"
kind = "work"
prompt = "verify.md"
timeout_minutes = 30

[[stage]]
id = "check"
kind = "review"
prompt = "check.md"
gate = "all_pass"
timeout_minutes = 30
"#;
/// A pipeline whose agent ships.
const AGENT_SHIPPED: &str = r#"[pipeline]
name = "agent-shipped"
description = "an agent ships"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "plan"
kind = "work"
prompt = "plan.md"
timeout_minutes = 30

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 30
"#;

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    for (name, toml) in [
        ("recorded", RECORDED),
        ("extra", EXTRA),
        ("agent-shipped", AGENT_SHIPPED),
    ] {
        h.add_pipeline(name, toml);
        let dir = h.repo.join(".archon/pipelines").join(name);
        for prompt in ["plan", "implement", "verify", "check", "ship"] {
            let text = format!("{prompt} {{{{task.title}}}}\n\n{{{{task.brief}}}}\n");
            std::fs::write(dir.join(format!("{prompt}.md")), text).expect("prompt");
        }
    }
    (h, gh)
}

async fn started(h: &Harness, pipeline: &str) -> (QueueItem, PipelineRun) {
    started_after(h, pipeline, &[]).await
}

/// An item of `pipeline` depending on `parents`, started.
async fn started_after(
    h: &Harness,
    pipeline: &str,
    parents: &[&QueueItem],
) -> (QueueItem, PipelineRun) {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: format!("a change through {pipeline}"),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: parents.iter().map(|p| p.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch::default(),
        origin: None,
    };
    let item = match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    };
    h.dispatch().await;
    let run = h.run(&item.id).await;
    (item, run)
}

/// The one running stage completes with `structured`; then a dispatch.
async fn settle_with(h: &Harness, structured: Value) {
    let outcome = StageOutcome::Completed {
        summary: "Changed it".into(),
        structured: Some(structured),
    };
    let settled = h.settle_running(|_| outcome.clone()).await;
    assert_eq!(settled.len(), 1, "one agent ran");
    h.dispatch().await;
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

fn insert(stage: &str) -> Steer {
    Steer {
        insert_stage: Some(InsertStage {
            stage: stage.into(),
            from: Some("extra".into()),
            after: None,
        }),
        ..Steer::default()
    }
}

fn abort(stage: &str, then: Then) -> Steer {
    Steer {
        stage: Some(stage.into()),
        abort: Some(then),
        ..Steer::default()
    }
}

fn switch(pipeline: &str) -> Steer {
    Steer {
        switch_pipeline: Some(pipeline.into()),
        ..Steer::default()
    }
}

/// A delivery queued, retrying or recorded ships the commit its stages left:
/// a steer that would change those stages is refused, and a note is not.
#[tokio::test]
async fn the_stages_of_a_run_that_is_delivering_no_longer_change() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    gh.fail_next("pr list", "HTTP 502: Bad Gateway");
    settle_with(&h, change(&run.workspace)).await;
    let state = h.state(&item.id).await;
    assert!(
        matches!(state, QueueState::Running { .. }),
        "it retries: {state:?}"
    );
    assert!(h.kv(&format!("ship_retry:{}", run.id)).await.is_some());

    let inserted = refused(&h, &item.id, insert("check")).await;
    assert!(
        inserted.contains("is delivering its change already"),
        "{inserted}"
    );
    let again = refused(&h, &item.id, abort("implement", Then::Retry)).await;
    assert!(
        again.contains("is delivering its change already"),
        "{again}"
    );
    let note = Steer {
        note: Some("Mention the migration in the pull request.".into()),
        ..Steer::default()
    };
    steer(&h, &item.id, note)
        .await
        .expect("a note changes no stage");

    let head = git(&h.repo, &["rev-parse", &run.branch]);
    let recorded = json!({"outcome": "shipped", "commit": head,
        "pr_url": "https://github.com/o/r/pull/9"});
    let key = format!("ship:{}", run.id);
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &recorded.to_string())
        .unwrap();
    let after = refused(&h, &item.id, insert("check")).await;
    assert!(
        after.contains("is delivering its change already"),
        "{after}"
    );
}

/// The stage that writes the ship record may not be removed or passed over
/// unless another stage writes one or ships: else the run delivers nothing.
#[tokio::test]
async fn a_steer_that_leaves_nothing_to_ship_is_refused() {
    let (h, _gh) = delivering().await;
    let (item, _) = started(&h, "recorded").await;
    let removed = refused(&h, &item.id, abort("implement", Then::Skip)).await;
    assert!(
        removed.contains("stage `implement` writes the ship record"),
        "{removed}"
    );
    assert!(removed.contains("deliver nothing"), "{removed}");
    let switched = refused(&h, &item.id, switch("extra")).await;
    assert!(switched.contains("deliver nothing"), "{switched}");

    steer(&h, &item.id, insert("verify"))
        .await
        .expect("the record stage stays");
    steer(&h, &item.id, switch("agent-shipped"))
        .await
        .expect("a stage that ships replaces the record");
}

/// The ship's checks, its push and its pull request run nothing the
/// repository or its `origin` configures, and still deliver.
#[tokio::test]
async fn a_delivery_runs_no_program_the_repository_or_its_origin_configures() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    std::fs::write(
        run.workspace.join(".gitattributes"),
        "* filter=planted merge=planted\n",
    )
    .unwrap();
    git(&run.workspace, &["add", ".gitattributes"]);
    let structured = change(&run.workspace);
    let marker = h.dir.path().join("planted-ran");
    plant_everywhere(&h.repo, &origin_of(&h), &marker);
    settle_with(&h, structured.clone()).await;

    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(ran.is_empty(), "a planted program ran as the daemon: {ran}");
    let commit = structured["commit"].as_str().unwrap().to_owned();
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(commit));
    assert_eq!(gh.count("pr create"), 1);
    assert!(!run.workspace.exists(), "the clean worktree was removed");
}

/// A stacked run whose branch lacks a parent commit its stack recorded is
/// refused before anything is pushed or a pull request is opened.
#[tokio::test]
async fn a_stacked_run_missing_a_parent_commit_pushes_nothing() {
    let (h, gh) = delivering().await;
    let (parent, parent_run) = started(&h, "direct").await;
    settle_with(&h, change(&parent_run.workspace)).await;
    let state = h.state(&parent.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let (child, child_run) = started_after(&h, "direct", &[&parent]).await;
    let tip = git(&h.repo, &["rev-parse", &parent_run.branch]);
    let tree = format!("{tip}^{{tree}}");
    let moved = git(
        &h.repo,
        &["commit-tree", &tree, "-p", &tip, "-m", "Parent moved on"],
    );
    {
        let mut store = h.ctx.store.lock().await;
        let mut stacked = store.stack_parents(&child.id).unwrap().remove(0);
        assert_eq!(
            stacked.parent_sha, tip,
            "the child forked the parent's head"
        );
        stacked.parent_sha = moved.clone();
        store.save_stack_parent(&stacked).unwrap();
    }
    settle_with(&h, change(&child_run.workspace)).await;

    let error = match h.state(&child.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the child fails: {other:?}"),
    };
    assert!(
        error.contains(&format!("missing parent {} at {moved}", parent.id)),
        "{error}"
    );
    assert_eq!(pushed(&origin_of(&h), &child_run.branch), None);
    assert_eq!(gh.count("pr create"), 1, "only the parent's pull request");
}

/// The commits a ship admits are by the identity recorded at registration:
/// one an agent configures later in the repository does not count, until
/// the operator updates the registration.
#[tokio::test]
async fn the_identity_a_ship_admits_is_the_one_recorded_by_the_operator() {
    let (h, _gh) = delivering().await;
    git(
        &h.repo,
        &["config", "user.email", "agent@elsewhere.invalid"],
    );
    let (item, run) = started(&h, "direct").await;
    std::fs::write(run.workspace.join("change.txt"), "a change\n").unwrap();
    git(&run.workspace, &["add", "change.txt"]);
    let committed = std::process::Command::new("git")
        .arg("-C")
        .arg(&run.workspace)
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "Change it",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(committed.success());
    settle_with(&h, record(&run.workspace)).await;

    let error = match h.state(&item.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the item fails: {other:?}"),
    };
    assert!(error.contains("<agent@elsewhere.invalid>"), "{error}");
    assert!(error.contains("`archon repo set`"), "{error}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);

    let update = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch::default(),
    };
    h.cli(update).await.expect("repo update");
    let stored = h.kv(&format!("repo_identity:{}", run_repo(&h).await)).await;
    assert!(stored.is_some_and(|s| s.contains("agent@elsewhere.invalid")));
}

async fn run_repo(h: &Harness) -> String {
    let store = h.ctx.store.lock().await;
    let repo = store.repo(&RepoRef(REPO_NAME.into())).expect("registered");
    repo.id.to_string()
}
