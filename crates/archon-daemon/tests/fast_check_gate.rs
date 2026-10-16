//! The ship gate (issue #82, task AR-I2): archond runs the repository's
//! trusted fast check on the committed change, for the units it affects,
//! before it delivers; a failing check refuses the item with its output.
//! A bare `origin` and a fake `gh`.

mod common;

use std::path::Path;
use std::time::Duration;

use archon_core::autonomy::AutonomyPatch;
use archon_core::id::QueueItemId;
use archon_core::model::ModelPlan;
use archon_core::protocol::{
    Clearable, EnqueueRequest, Reply, RepoPatch, RepoRef, Request, TiersPatch,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, StageOutcome};
use common::delivery::{FakeGh, add_origin, change, origin_of, pushed, record};
use common::{Harness, REPO_NAME, git};
use serde_json::Value;

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

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    h.add_pipeline("recorded", RECORDED);
    let dir = h.repo.join(".archon/pipelines/recorded");
    for prompt in ["plan", "implement"] {
        let text = format!("{prompt} {{{{task.title}}}}\n\n{{{{task.brief}}}}\n");
        std::fs::write(dir.join(format!("{prompt}.md")), text).expect("prompt");
    }
    (h, gh)
}

async fn set_tiers(h: &Harness, tiers: TiersPatch) {
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch {
            tiers,
            ..RepoPatch::default()
        },
    };
    h.cli(request).await.expect("repo update");
}

async fn fast_check(h: &Harness, command: &str) {
    let tiers = TiersPatch {
        fast_check: Clearable::Set(command.into()),
        ..TiersPatch::default()
    };
    set_tiers(h, tiers).await;
}

async fn started(h: &Harness, pipeline: &str) -> (QueueItem, PipelineRun) {
    let request = EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: format!("a change through {pipeline}"),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
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

/// Commit `file` in `workspace`, as a stage agent does; its ship record.
fn commit_in(workspace: &Path, file: &str) -> Value {
    let path = workspace.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "a change\n").unwrap();
    git(workspace, &["add", file]);
    git(workspace, &["commit", "--quiet", "-m", "Change the thing"]);
    record(workspace)
}

/// The one running stage completes with `structured`; then the item is
/// dispatched until it settles, as the gate's check runs in the background.
async fn settled(h: &Harness, item: &QueueItemId, structured: Value) -> QueueState {
    let outcome = StageOutcome::Completed {
        summary: "Changed it".into(),
        structured: Some(structured),
    };
    let settled = h.settle_running(|_| outcome.clone()).await;
    assert_eq!(settled.len(), 1, "one agent ran");
    for _ in 0..200 {
        h.dispatch().await;
        let state = h.state(item).await;
        if state.is_terminal() {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the item never settled: {:?}", h.state(item).await);
}

fn failure(state: QueueState) -> String {
    match state {
        QueueState::Failed { error, .. } => error,
        other => panic!("the item fails: {other:?}"),
    }
}

#[tokio::test]
async fn ship_refuses_an_item_whose_fast_check_fails_with_its_output() {
    let (h, gh) = delivering().await;
    fast_check(&h, "echo checked {packages} >&2; exit 4").await;
    let (item, run) = started(&h, "direct").await;
    let state = settled(&h, &item.id, commit_in(&run.workspace, "pkg/lib.txt")).await;

    let error = failure(state);
    assert!(
        error.contains("the fast check `echo checked pkg >&2; exit 4` failed"),
        "{error}"
    );
    assert!(
        error.contains("checked pkg"),
        "the output is quoted: {error}"
    );
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert_eq!(gh.count("pr create"), 0);
}

#[tokio::test]
async fn a_passing_fast_check_runs_once_for_the_affected_units_and_ships() {
    let (h, gh) = delivering().await;
    let marker = h.dir.path().join("checked");
    let command = format!("echo {{packages}} >> '{}'", marker.display());
    fast_check(&h, &command).await;
    let (item, run) = started(&h, "direct").await;
    let structured = commit_in(&run.workspace, "pkg/lib.txt");
    let state = settled(&h, &item.id, structured.clone()).await;

    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "pkg\n");
    let commit = structured["commit"].as_str().unwrap().to_owned();
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(commit));
    assert_eq!(gh.count("pr create"), 1);
}

#[tokio::test]
async fn a_change_that_affects_no_unit_ships_without_running_the_check() {
    let (h, _gh) = delivering().await;
    let marker = h.dir.path().join("checked");
    fast_check(&h, &format!("echo {{packages}} >> '{}'", marker.display())).await;
    let (item, run) = started(&h, "direct").await;
    let state = settled(&h, &item.id, change(&run.workspace)).await;

    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    assert!(!marker.exists(), "a root file is no unit");
}

#[tokio::test]
async fn a_check_that_changes_the_worktree_is_refused_when_it_is_proved_again() {
    let (h, gh) = delivering().await;
    fast_check(&h, "echo tampered >> change.txt").await;
    let (item, run) = started(&h, "direct").await;
    let state = settled(&h, &item.id, change(&run.workspace)).await;

    let error = failure(state);
    assert!(error.contains("uncommitted changes"), "{error}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert_eq!(gh.count("pr create"), 0);
}

#[tokio::test]
async fn a_committed_check_runs_only_once_the_operator_confirmed_it() {
    let (h, _gh) = delivering().await;
    let marker = h.dir.path().join("checked");
    let command = format!("echo ran >> '{}'", marker.display());
    let file = format!("[checks]\nfast = \"{command}\"\n");
    std::fs::write(h.repo.join(".archon/repo.toml"), file).unwrap();

    let (first, run) = started(&h, "direct").await;
    let state = settled(&h, &first.id, change(&run.workspace)).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    assert!(
        !marker.exists(),
        "an unconfirmed committed check never runs"
    );

    let confirm = TiersPatch {
        confirmed_fast_check: Clearable::Set(command.clone()),
        ..TiersPatch::default()
    };
    set_tiers(&h, confirm).await;
    let (second, run) = started(&h, "direct").await;
    let state = settled(&h, &second.id, change(&run.workspace)).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
}

#[tokio::test]
async fn a_later_stage_s_contract_names_the_command_for_the_change_so_far() {
    let (h, _gh) = delivering().await;
    fast_check(&h, "cargo test {packages:-p}").await;
    let (item, run) = started(&h, "recorded").await;
    let first = h.backend.requests().remove(0);
    assert!(
        first.instructions.contains("as committed now affects none"),
        "{}",
        first.instructions
    );
    assert!(first.instructions.contains("Archon runs it again"));
    commit_in(&run.workspace, "web/app.txt");
    let outcome = StageOutcome::Completed {
        summary: "Planned".into(),
        structured: None,
    };
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    let requests = h.backend.requests();
    let implement = requests.last().expect("the implement stage");
    assert_ne!(implement.instructions, first.instructions);
    assert!(
        implement
            .instructions
            .contains("For the change as committed now: `cargo test -p web`."),
        "{}",
        implement.instructions
    );
    assert!(!h.state(&item.id).await.is_terminal());
}
