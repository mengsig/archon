//! The delivery archond makes itself (ARCHITECTURE §8.7): no agent ships a
//! built-in pipeline. What is proved before anything leaves the machine,
//! the pull request found or opened once, a restart mid-delivery, the review
//! level an item asks for, `direct`, and a stage that escalates.

use crate::common;

use std::path::Path;
use std::process::Command;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{AutonomyPatch, MergeMode, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::model::ModelPlan;
use archon_core::notify::NotificationKind;
use archon_core::outcome::{Correction, FailureClass};
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Attribution, ShipMode};
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_core::steer::Steer;
use archon_daemon::executor;
use archon_daemon::triggers::{self, TriggerEvent};
use common::delivery::{
    FakeGh, add_origin, change, origin_of, pass_by, pushed, record, shipped_by, verdict_on,
};
use common::{Harness, REPO_NAME, REVIEW_MODEL, REVIEWED, REVIEWED_TOML, git, verdict};
use serde_json::{Value, json};

const PR_ONE: &str = "https://github.com/o/r/pull/1";

/// A harness whose repository has an `origin` and whose `gh` keeps the pull
/// requests archond opens.
async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
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
        onto: None,
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
    let (item, run) = started(&h, "direct").await;
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
    let (item, run) = started(&h, "direct").await;
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

    let retry_key = format!("ship_retry:{}", run.id);
    let retry: Value = serde_json::from_str(&h.kv(&retry_key).await.expect("stored")).unwrap();
    assert_eq!(retry["failures"], 1, "{retry}");

    h.restart();
    executor::recover(&h.ctx).await.unwrap();
    assert_eq!(
        gh.count("pr list"),
        lookups,
        "a start is not held up by GitHub"
    );
    h.dispatch().await;
    assert_eq!(
        gh.count("pr list"),
        lookups,
        "the wait outlives the restart"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));

    let due = json!({"failures": 1, "next_at": "2000-01-01T00:00:00Z"});
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&retry_key, &due.to_string())
        .unwrap();
    h.restart();
    executor::recover(&h.ctx).await.unwrap();
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
    let (item, run) = started(&h, "direct").await;
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
    let (item, run) = started(&h, "direct").await;
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
    let (item, run) = started(&h, "direct").await;
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

const LFS_ABC_OID: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn lfs_pointer(size: u64) -> String {
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{LFS_ABC_OID}\nsize {size}\n")
}

/// A committed pointer is the evidence, even without attributes or git-lfs.
#[tokio::test]
async fn local_mode_ships_smudged_lfs_and_unsmudged_pointers_without_running_filters() {
    let (h, gh) = delivering().await;
    update_repo(
        &h,
        RepoPatch {
            ship: Some(ShipMode::Local),
            ..RepoPatch::default()
        },
    )
    .await;
    let (item, run) = started(&h, "direct").await;
    let pointer = lfs_pointer(3);
    // Hundreds of candidates also exercise bounded, sequential blob and file reads.
    for i in 0..128 {
        std::fs::write(run.workspace.join(format!("asset-{i}.bin")), &pointer).unwrap();
    }
    std::fs::write(run.workspace.join("unsmudged.bin"), &pointer).unwrap();
    std::fs::write(
        run.workspace.join("asset without attributes.uasset"),
        &pointer,
    )
    .unwrap();
    std::fs::write(
        run.workspace.join(".gitattributes"),
        "*.bin filter=hostile\n",
    )
    .unwrap();
    git(&run.workspace, &["add", "."]);
    git(
        &run.workspace,
        &["commit", "--quiet", "-m", "Add LFS pointers"],
    );
    let verified = record(&run.workspace);
    for i in 0..128 {
        std::fs::write(run.workspace.join(format!("asset-{i}.bin")), b"abc").unwrap();
    }
    std::fs::write(
        run.workspace.join("asset without attributes.uasset"),
        b"abc",
    )
    .unwrap();
    let marker = run.workspace.join("filter-ran");
    let command = format!("touch {}; cat", marker.display());
    git(
        &run.workspace,
        &["config", "filter.hostile.clean", &command],
    );
    git(
        &run.workspace,
        &["config", "filter.hostile.smudge", &command],
    );
    settle_with(&h, verified).await;

    succeeded(&h, &item).await;
    assert!(h.run(&item.id).await.shipped);
    assert!(!marker.exists(), "repository filters never run as archond");
    assert!(gh.calls().is_empty());
}

#[tokio::test]
async fn local_mode_refuses_lfs_mismatches_deletions_and_type_changes() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    for case in [
        "hash",
        "size",
        "deleted",
        "symlink",
        "directory",
        "executable",
        "normal",
        "malformed",
        "quoted-path",
        "parent-symlink",
    ] {
        let (h, gh) = delivering().await;
        update_repo(
            &h,
            RepoPatch {
                ship: Some(ShipMode::Local),
                ..RepoPatch::default()
            },
        )
        .await;
        let (item, run) = started(&h, "direct").await;
        let relative = if case == "quoted-path" {
            "asset\nname.bin"
        } else if case == "parent-symlink" {
            "nested/asset.bin"
        } else {
            "asset.bin"
        };
        let asset = run.workspace.join(relative);
        std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
        let pointer = lfs_pointer(if case == "size" { 4 } else { 3 });
        let committed = if case == "malformed" {
            pointer.replace("size 3", "size +3")
        } else {
            pointer
        };
        std::fs::write(&asset, committed).unwrap();
        git(&run.workspace, &["add", "."]);
        git(
            &run.workspace,
            &["commit", "--quiet", "-m", "Add LFS pointer"],
        );
        let verified = record(&run.workspace);
        std::fs::remove_file(&asset).unwrap();
        match case {
            "deleted" => {}
            "symlink" => {
                std::fs::write(run.workspace.join("content"), b"abc").unwrap();
                symlink("content", &asset).unwrap();
            }
            "directory" => std::fs::create_dir(&asset).unwrap(),
            "parent-symlink" => {
                std::fs::remove_dir(run.workspace.join("nested")).unwrap();
                std::fs::create_dir(run.workspace.join("other")).unwrap();
                std::fs::write(run.workspace.join("other/asset.bin"), b"abc").unwrap();
                symlink("other", run.workspace.join("nested")).unwrap();
            }
            "hash" | "quoted-path" => std::fs::write(&asset, b"abd").unwrap(),
            _ => std::fs::write(&asset, b"abc").unwrap(),
        }
        if case == "executable" {
            git(&run.workspace, &["config", "core.fileMode", "true"]);
            std::fs::set_permissions(&asset, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        if case == "normal" {
            std::fs::write(run.workspace.join("README.md"), "edited, not committed\n").unwrap();
        }
        settle_with(&h, verified).await;

        let error = failed(&h, &item).await;
        assert!(error.contains("uncommitted changes"), "{case}: {error}");
        assert!(
            error.contains(if case == "normal" {
                "README.md"
            } else {
                "asset"
            }),
            "{case}: {error}"
        );
        assert!(!h.run(&item.id).await.shipped, "{case}");
        assert!(gh.calls().is_empty(), "{case}");
    }
}

/// A commit made under an identity the agent set itself is not the
/// repository's: it is named, and the branch is not pushed.
#[tokio::test]
async fn a_commit_under_another_identity_refuses_the_delivery() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
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

fn attribute(policy: Attribution) -> RepoPatch {
    RepoPatch {
        attribution: Some(policy),
        ..RepoPatch::default()
    }
}

/// The trailers of a commit `attributed` makes.
const CO_AUTHORS: [&str; 6] = ["a", "b", "c", "d", "e", "Claude"];

/// A change committed with six models' trailers, and a record whose body
/// ends with a tool's footer.
fn attributed(workspace: &Path) -> Value {
    std::fs::write(workspace.join("change.txt"), "a change\n").unwrap();
    git(workspace, &["add", "change.txt"]);
    let trailers: Vec<String> = CO_AUTHORS
        .iter()
        .map(|name| format!("Co-Authored-By: {name} <noreply@anthropic.com>"))
        .collect();
    let message = format!("Change the thing\n\n{}", trailers.join("\n"));
    git(workspace, &["commit", "--quiet", "-m", &message]);
    let mut record = record(workspace);
    record["pr"]["body"] = json!(
        "Why, and how it was verified.\n\n\u{1f916} Generated with [Claude Code](https://claude.com/claude-code)"
    );
    record
}

/// Under `none`, set while the run was under way, the pull request body ends
/// with the record's own text.
#[tokio::test]
async fn without_attribution_the_pull_request_names_no_item() {
    let (h, gh) = delivering().await;
    let (item, _) = started(&h, "direct").await;
    update_repo(&h, attribute(Attribution::None)).await;
    settle_changed(&h, json!({})).await;

    succeeded(&h, &item).await;
    let (_, body) = gh.created();
    assert!(body.starts_with("Why, and how it was verified."), "{body}");
    assert!(!body.contains("Archon item:"), "{body}");
    assert!(!body.contains(item.id.as_str()), "{body}");
}

/// Under `none`, set while the run was under way, trailers and a footer
/// refuse the delivery, every line quoted, and nothing leaves the machine.
#[tokio::test]
async fn an_attribution_line_refuses_the_delivery_under_none() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    update_repo(&h, attribute(Attribution::None)).await;
    settle_with(&h, attributed(&run.workspace)).await;

    let error = failed(&h, &item).await;
    assert!(error.contains("attribution policy is `none`"), "{error}");
    assert!(error.contains("7 line(s) do"), "{error}");
    let footer = "Generated with [Claude Code](https://claude.com/claude-code)";
    assert!(
        error.contains(&format!("`pr.body` has {footer:?}")),
        "{error}"
    );
    let commit = git(&run.workspace, &["rev-parse", "HEAD"]);
    for name in CO_AUTHORS {
        let trailer = format!(
            "commit {} has \"Co-Authored-By: {name} <noreply@anthropic.com>\"",
            &commit[..12]
        );
        assert!(error.contains(&trailer), "{error}");
    }
    assert!(error.contains("Retry the item"), "{error}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}

/// The default `item-id` policy refuses nothing and keeps the item line.
#[tokio::test]
async fn attribution_lines_ship_under_the_default_policy() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    settle_with(&h, attributed(&run.workspace)).await;

    succeeded(&h, &item).await;
    let (_, body) = gh.created();
    assert!(
        body.ends_with(&format!("Archon item: {}\n", item.id)),
        "{body}"
    );
}

/// A record that names commits the branch does not add ships nothing.
#[tokio::test]
async fn a_branch_with_no_commit_of_its_own_has_nothing_to_ship() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    settle_with(&h, record(&run.workspace)).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("nothing to ship"), "{error}");
    assert!(error.contains("has no commit beyond `main`"), "{error}");
    assert!(gh.calls().is_empty());
}

/// A task that needed no change succeeds with that said, and ships nothing.
#[tokio::test]
async fn a_no_change_result_on_a_clean_branch_succeeds_without_shipping() {
    let (h, gh) = delivering().await;
    let (item, _run) = started(&h, "direct").await;
    settle_with(
        &h,
        json!({"no_change": {"reason": "the typo is already fixed"}}),
    )
    .await;
    let summary = succeeded(&h, &item).await;
    assert!(summary.contains("No change was needed"), "{summary}");
    assert!(summary.contains("the typo is already fixed"), "{summary}");
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}

/// The proposal page a propose stage leaves untracked is Archon's, not a change.
#[tokio::test]
async fn a_no_change_result_beside_archon_s_own_report_pages_succeeds() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    let page = run.workspace.join(".archon/reports/proposal/index.html");
    std::fs::create_dir_all(page.parent().unwrap()).unwrap();
    std::fs::write(&page, "<p>the proposal</p>\n").unwrap();
    settle_with(&h, json!({"no_change": {"reason": "already done"}})).await;
    let summary = succeeded(&h, &item).await;
    assert!(summary.contains("No change was needed"), "{summary}");
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}

/// A claim of no change never bypasses the review of a change that exists.
#[tokio::test]
async fn a_no_change_claim_beside_a_commit_or_a_dirty_tree_fails_the_stage() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    change(&run.workspace);
    settle_with(&h, json!({"no_change": {"reason": "nothing to do"}})).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("reported no change needed"), "{error}");
    assert!(error.contains("commit(s) beyond"), "{error}");

    let (h, gh2) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    std::fs::write(run.workspace.join("README.md"), "edited\n").unwrap();
    settle_with(&h, json!({"no_change": {"reason": "nothing to do"}})).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("uncommitted changes"), "{error}");

    let (h, gh3) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    std::fs::write(run.workspace.join("new_module.rs"), "fn main() {}\n").unwrap();
    settle_with(&h, json!({"no_change": {"reason": "nothing to do"}})).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("staged or untracked"), "{error}");
    assert!(error.contains("new_module.rs"), "{error}");
    assert!(gh.calls().is_empty() && gh2.calls().is_empty() && gh3.calls().is_empty());
}

/// A stage that leaves no usable record is told what is missing before
/// anything is delivered.
#[tokio::test]
async fn a_missing_ship_record_fails_the_stage_that_owes_it() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
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

/// A record naming its commit and tree by the short hashes `git commit` and
/// `git rev-parse --short` print ships that commit, stored by its full id.
#[tokio::test]
async fn a_short_commit_and_tree_ship_as_their_full_ids() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    let mut short = change(&run.workspace);
    let full = short["commit"].as_str().unwrap().to_owned();
    short["commit"] = json!(full[..7].to_uppercase());
    short["tree"] = json!(git(&run.workspace, &["rev-parse", "--short", "HEAD^{tree}"]));
    settle_with(&h, short).await;

    let summary = succeeded(&h, &item).await;
    assert!(summary.contains(&format!("(commit {full})")), "{summary}");
    assert_eq!(pushed(&origin_of(&h), &run.branch), Some(full.clone()));
    assert_eq!(gh.count("pr create"), 1);
    let stages = h.ctx.store.lock().await.stage_runs(&run.id).unwrap();
    let recorded = stages[0].structured.as_ref().unwrap();
    assert_eq!(recorded["commit"], json!(full), "the full id is stored");
}

/// A short hash that names no commit of the worktree, or a commit's hash
/// given for the tree, is refused and says why.
#[tokio::test]
async fn a_short_hash_that_names_no_such_object_is_refused() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
    let mut wrong = change(&run.workspace);
    let commit = wrong["commit"].as_str().unwrap().to_owned();
    wrong["tree"] = json!(commit[..9]);
    settle_with(&h, wrong).await;

    let error = failed(&h, &item).await;
    assert!(
        error.contains("the ship record archond delivers is not on record"),
        "{error}"
    );
    assert!(error.contains("names no single tree"), "{error}");
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

/// The running lead delegates `count` reviewers that pass its change, and
/// settles with it, its ship record and a pass naming `named` agents: those
/// reviewers first, then agents it never delegated to.
async fn settle_reviewed_by(h: &Harness, count: usize, named: usize) -> (StageRun, Vec<String>) {
    let lead = h.running().await.remove(0);
    let agent = lead.aether_agent.clone().unwrap();
    let reviewers: Vec<AetherAgentId> = (0..count)
        .map(|_| h.backend.delegate(&agent, REVIEW_MODEL))
        .collect();
    let mut names = reviewers.clone();
    names.truncate(named);
    for n in count..named {
        names.push(AetherAgentId::parse(&format!("agt_stranger{n}")).unwrap());
    }
    let built = shipped_by(h, &lead, "Built; reviewed", pass_by(&names)).await;
    for reviewer in &reviewers {
        h.backend
            .complete_delegated(reviewer, verdict_on(&built, "pass"));
    }
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    (lead, reviewers.iter().map(ToString::to_string).collect())
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
            "it names reviewer agt_stranger2, which is not an agent the lead delegated to",
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

/// A review vouches for what ships only if it passed the tree the ship record
/// names, which is the tree archond checks the branch holds when it delivers.
#[tokio::test]
async fn a_review_of_another_tree_than_the_ship_record_fails_the_item() {
    let (h, gh) = delivering().await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Standard))
        .await
        .unwrap();
    h.dispatch().await;
    let lead = h.running().await.remove(0);
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let named = pass_by(std::slice::from_ref(&reviewer));
    let recorded = shipped_by(&h, &lead, "Built; reviewed", named).await;
    let run = h.run(&item.id).await;
    let later = StageOutcome::Completed {
        summary: "one more change".into(),
        structured: Some(change(&run.workspace)),
    };
    h.backend
        .complete_delegated(&reviewer, verdict_on(&later, "pass"));
    h.settle_running(|_| recorded.clone()).await;
    h.dispatch().await;

    let error = failed(&h, &item).await;
    assert!(
        error.contains("the review this pipeline declares is not on record"),
        "{error}"
    );
    assert!(error.contains("the ship record names"), "{error}");
    assert!(gh.calls().is_empty(), "nothing is delivered");
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
}

/// A lead that amends a reviewed commit and has the amend reviewed in full
/// ships: the verdict on the replaced commit is superseded, pass or fail.
#[tokio::test]
async fn a_verdict_on_an_amended_commit_is_superseded_by_a_review_of_the_amend() {
    for first_verdict in ["pass", "fail"] {
        let (h, gh) = delivering().await;
        let item = enqueue(&h, "adaptive", asking(ReviewLevel::Standard))
            .await
            .unwrap();
        h.dispatch().await;
        let lead = h.running().await.remove(0);
        let agent = lead.aether_agent.clone().unwrap();
        let first = h.backend.delegate(&agent, REVIEW_MODEL);
        let second = h.backend.delegate(&agent, REVIEW_MODEL);
        let replaced = shipped_by(&h, &lead, "Built", json!({})).await;
        h.backend
            .complete_delegated(&first, verdict_on(&replaced, first_verdict));

        let run = h.run(&item.id).await;
        git(
            &run.workspace,
            &["commit", "--amend", "--quiet", "-m", "Amended"],
        );
        let mut amended = record(&run.workspace);
        let named = pass_by(&[first.clone(), second.clone()]);
        amended["review"] = named["review"].clone();
        let amended = StageOutcome::Completed {
            summary: "Built; amended; reviewed".into(),
            structured: Some(amended),
        };
        h.backend
            .complete_delegated(&second, verdict_on(&amended, "pass"));
        h.settle_running(|_| amended.clone()).await;
        h.dispatch().await;

        let summary = succeeded(&h, &item).await;
        assert!(summary.contains(&format!("SHIPPED: {PR_ONE}")), "{summary}");
        let stored = h.ctx.store.lock().await.stage_run(&lead.id).unwrap();
        let review = &stored.structured.as_ref().unwrap()["review"];
        assert_eq!(
            review["reviewer_agents"],
            json!([second.to_string()]),
            "{first_verdict}: only the review of the amend counts"
        );
        assert_eq!(gh.count("pr create"), 1);
    }
}

/// A pass Archon refused is the lead's failure: learning does not count its
/// stage as a pass for the lead's model.
#[tokio::test]
async fn a_refused_review_is_no_pass_for_the_leads_model() {
    let (h, gh) = delivering().await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Standard))
        .await
        .unwrap();
    h.dispatch().await;
    let (lead, _) = settle_reviewed_by(&h, 0, 1).await;
    let error = failed(&h, &item).await;
    assert!(error.contains("the lead delegated to no agent"), "{error}");
    assert!(gh.calls().is_empty(), "nothing is delivered");

    let run = h.run(&item.id).await;
    let outcome = h.ctx.store.lock().await.outcome(&run.id).unwrap();
    let outcome = outcome.expect("the finished run is learnt from");
    let observed: Vec<_> = outcome
        .stages
        .iter()
        .filter(|stage| stage.stage == lead.stage_id)
        .collect();
    assert_eq!(observed.len(), 1, "{:?}", outcome.stages);
    assert_eq!(observed[0].choice, lead.choice);
    assert_eq!(
        observed[0].passed,
        Some(false),
        "a refused review is no pass"
    );
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
    let (item, _) = started(&h, REVIEWED).await;
    settle_changed(&h, json!({})).await;
    assert_eq!(stage_labels(&h), ["implement", "review"]);
    let advice = "ESCALATE: this rewrites the session token check; run it through \
                  adaptive with review strict";
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

/// An escalated item switched to the recommended pipeline continues on its
/// branch and ships; learning keeps the escalation and the steer.
#[tokio::test]
async fn an_escalated_item_switched_to_another_pipeline_ships_from_its_branch() {
    let (h, gh) = delivering().await;
    let (item, first) = started(&h, "direct").await;
    let begun = change(&first.workspace);
    let advice = "ESCALATE: this rewrites the session check; have it reviewed";
    h.settle_running(|_| StageOutcome::Failed {
        error: advice.into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    failed(&h, &item).await;

    let switch = Steer {
        switch_pipeline: Some(REVIEWED.into()),
        ..Steer::default()
    };
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: switch,
    };
    h.orchestrator(steer).await.expect("revived");
    h.dispatch().await;
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    let second = runs.last().expect("a second run").clone();
    assert_eq!(runs.len(), 2, "a new run");
    assert_eq!(
        (&second.branch, &second.workspace),
        (&first.branch, &first.workspace)
    );
    settle_changed(&h, json!({})).await;
    h.settle_running(|_| verdict(true)).await;
    h.dispatch().await;
    let stages = ["implement", "implement", "review"];
    assert_eq!(stage_labels(&h), stages, "the stages switched to");

    let head = git(&h.repo, &["rev-parse", &first.branch]);
    let summary = succeeded(&h, &item).await;
    let shipped = format!("SHIPPED: {PR_ONE} (commit {head})");
    assert!(summary.ends_with(&shipped), "{summary}");
    let commit = begun["commit"].as_str().unwrap();
    git(&h.repo, &["merge-base", "--is-ancestor", commit, &head]);
    assert_eq!(gh.count("pr create"), 1, "{:?}", gh.calls());
    let store = h.ctx.store.lock().await;
    let escalated = store.outcome(&first.id).unwrap().expect("captured");
    assert_eq!(escalated.failure, Some(FailureClass::Escalated));
    assert!(escalated.corrections.is_empty());
    // The item keeps its pipeline's name, so the run is no evidence for it.
    let delivered = store.outcome(&second.id).unwrap().expect("captured");
    assert_eq!(delivered.pipeline, "direct");
    assert!(delivered.corrections.is_empty());
}

/// A shipped item retried goes on from its branch into the pull request it
/// opened, and the retry counts against the run that had passed.
#[tokio::test]
async fn a_shipped_item_retried_delivers_into_its_pull_request_again() {
    let (h, gh) = delivering().await;
    let (item, first) = started(&h, "direct").await;
    settle_changed(&h, json!({})).await;
    succeeded(&h, &item).await;
    let shipped = git(&h.repo, &["rev-parse", &first.branch]);

    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: Some("The greeting is still wrong on an empty name.".into()),
        stage_timeouts: Default::default(),
    };
    h.orchestrator(retry).await.expect("queued again");
    h.dispatch().await;
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    assert_eq!(runs.len(), 2, "a new run");
    assert_eq!(runs[1].branch, first.branch, "on the branch that shipped");
    settle_changed(&h, json!({})).await;

    let head = git(&h.repo, &["rev-parse", &first.branch]);
    let summary = succeeded(&h, &item).await;
    let again = format!("SHIPPED: {PR_ONE} (commit {head})");
    assert!(summary.ends_with(&again), "{summary}");
    git(&h.repo, &["merge-base", "--is-ancestor", &shipped, &head]);
    assert_ne!(shipped, head, "the retry added to what shipped");
    assert_eq!(pushed(&origin_of(&h), &first.branch), Some(head));
    assert_eq!(gh.count("pr create"), 1, "{:?}", gh.calls());
    let store = h.ctx.store.lock().await;
    let passed = store.outcome(&first.id).unwrap().expect("captured");
    assert_eq!(passed.corrections, [Correction::RetryAfterDone]);
    assert!(!passed.is_win(), "{passed:?}");
    let fixed = store.outcome(&runs[1].id).unwrap().expect("captured");
    assert!(fixed.is_win(), "{fixed:?}");
}

async fn update_repo(h: &Harness, patch: RepoPatch) {
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

/// Let the repository's runs merge by themselves; the autonomy asking for it.
async fn allow_auto_merge(h: &Harness) -> AutonomyPatch {
    let auto = AutonomyPatch {
        merge: Some(MergeMode::Auto),
        ..AutonomyPatch::default()
    };
    let ceiling = RepoPatch {
        autonomy_max: Some(auto),
        ..RepoPatch::default()
    };
    update_repo(h, ceiling).await;
    auto
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
    let (item, run) = started(&h, "direct").await;
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
    let (item, run) = started(&h, "direct").await;
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
    let (item, run) = started(&h, "direct").await;
    gh.open(&run.branch, "some-old-base");
    settle_changed(&h, json!({})).await;

    let summary = succeeded(&h, &item).await;
    let url = "https://github.com/o/r/pull/7";
    assert!(summary.contains(&format!("SHIPPED: {url}")), "{summary}");
    assert_eq!(gh.count("pr create"), 0, "never a second pull request");
    let moved = format!("pr edit {url} --base main");
    assert!(gh.calls().contains(&moved), "{:?}", gh.calls());
}

/// An item that asks for it is owed auto-merge once its pull request is
/// delivered: a maintenance pass enables it, a failure GitHub may not repeat
/// is tried again later, and a refusal is the operator's blocker.
#[tokio::test]
async fn auto_merge_is_owed_until_github_takes_it_and_its_refusal_fails_nothing() {
    let (h, gh) = delivering().await;
    let auto = allow_auto_merge(&h).await;
    let first = enqueue(&h, "direct", auto).await.unwrap();
    h.dispatch().await;
    gh.fail_next("pr merge", "HTTP 502: Bad Gateway");
    settle_changed(&h, json!({})).await;
    succeeded(&h, &first).await;
    let run = h.run(&first.id).await;
    let owed_key = format!("ship_automerge:{}", run.id);
    assert!(h.kv(&owed_key).await.is_some(), "delivered is not merged");
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let head = git(&h.repo, &["rev-parse", &run.branch]);
    let merge = format!("pr merge {PR_ONE} --auto --squash --match-head-commit {head}");
    let tries = |gh: &FakeGh| gh.calls().iter().filter(|c| **c == merge).count();
    assert_eq!(tries(&gh), 1, "{:?}", gh.calls());
    let owed: Value = serde_json::from_str(&h.kv(&owed_key).await.unwrap()).unwrap();
    assert_eq!(owed["tries"], 1, "{owed}");
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert_eq!(tries(&gh), 1, "the next try waits its turn");

    let mut due = owed;
    due["next_at"] = json!("2000-01-01T00:00:00Z");
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&owed_key, &due.to_string())
        .unwrap();
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert_eq!(tries(&gh), 2);
    assert_eq!(h.kv(&owed_key).await, None, "GitHub took it");
    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert!(
        outcomes
            .iter()
            .any(|n| n.contains("Auto-merge is on") && n.contains("not merged yet"))
    );

    let second = enqueue(&h, "direct", auto).await.unwrap();
    h.dispatch().await;
    gh.fail_next("pr merge", "Pull request is in clean status");
    settle_changed(&h, json!({})).await;
    succeeded(&h, &second).await;
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    let blockers = h.notices(NotificationKind::Blocker).await;
    let told =
        |n: &&String| n.contains("Auto-merge could not be enabled") && n.contains("clean status");
    assert_eq!(blockers.iter().filter(told).count(), 1, "{blockers:?}");
    executor::coordination::reconcile(&h.ctx).await.unwrap();
    assert_eq!(
        h.notices(NotificationKind::Blocker).await.len(),
        blockers.len(),
        "told once"
    );
}

/// A commit `origin` holds on the run branch and the run does not is never
/// overwritten: the push is refused as it stands.
#[tokio::test]
async fn a_remote_branch_holding_other_commits_is_never_forced() {
    let (h, gh) = delivering().await;
    let (item, run) = started(&h, "direct").await;
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
    let (item, _) = started(&h, "direct").await;
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
    let (item, _) = started(&h, "direct").await;
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
    let (item, run) = started(&h, REVIEWED).await;
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
    let (item, run) = started(&h, "direct").await;
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
    assert_eq!(
        shipping, 5,
        "adaptive, direct, swarm, refactor and performance ship"
    );
}

/// [`delivering`], with the automatic repair retry on or off.
async fn delivering_repairing(auto_repair: bool) -> (Harness, FakeGh) {
    let mut config = archon_core::config::Config::default();
    config.review.auto_repair = auto_repair;
    let h = Harness::with_config(config).await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

async fn brief_of(h: &Harness, item: &QueueItem) -> String {
    h.ctx.store.lock().await.item(&item.id).unwrap().brief
}

const REPAIR_OPENING: &str = "Automatic repair (archond retries a refused record once).";
const TOO_FEW: &str = "it names 2 reviewer(s), and this item's review level needs 3";

/// A strict item whose lead names 2 of the 3 reviewers it owes is refused.
async fn refused_lead(h: &Harness, strict: &QueueItem) {
    assert_eq!(h.state(&strict.id).await.kind(), "running");
    settle_reviewed_by(h, 2, 2).await;
}

#[tokio::test]
async fn a_refused_review_is_repaired_by_one_automatic_retry_that_passes() {
    let (h, gh) = delivering_repairing(true).await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Strict))
        .await
        .unwrap();
    h.dispatch().await;
    let first = h.run(&item.id).await;
    refused_lead(&h, &item).await;

    assert_eq!(
        h.state(&item.id).await.kind(),
        "running",
        "retried, not failed"
    );
    let brief = brief_of(&h, &item).await;
    assert!(brief.contains(REPAIR_OPENING), "{brief}");
    assert!(brief.contains(TOO_FEW), "{brief}");
    assert!(brief.contains("Repair the record only."), "{brief}");
    let notices = h.notices(NotificationKind::Progress).await;
    let line = notices
        .iter()
        .find(|n| n.starts_with("review gate refused"));
    let line = line.unwrap_or_else(|| panic!("{notices:?}"));
    assert!(
        line.contains("retrying once to repair the record"),
        "{line}"
    );
    assert!(h.notices(NotificationKind::Outcome).await.is_empty());

    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    let second = runs.last().expect("a second run").clone();
    assert_eq!(
        (&second.branch, &second.workspace),
        (&first.branch, &first.workspace)
    );
    settle_reviewed_by(&h, 3, 3).await;
    succeeded(&h, &item).await;
    let runs = h.ctx.store.lock().await.runs_for_item(&item.id).unwrap();
    assert_eq!(runs.len(), 2, "the failed run stays in the history");
    assert!(matches!(
        runs[0].state,
        archon_core::run::RunState::Failed { .. }
    ));
    assert_eq!(gh.count("pr create"), 1, "{:?}", gh.calls());
}

#[tokio::test]
async fn a_second_refusal_fails_the_item_with_both_reasons() {
    let (h, _gh) = delivering_repairing(true).await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Strict))
        .await
        .unwrap();
    h.dispatch().await;
    refused_lead(&h, &item).await;
    refused_lead(&h, &item).await;

    let error = failed(&h, &item).await;
    assert!(
        error.contains("the review this pipeline declares is not on record"),
        "{error}"
    );
    let first = format!(
        "The first refusal was: the review this pipeline declares is not on record: {TOO_FEW}"
    );
    assert!(error.contains(&first), "both reasons: {error}");
    assert!(error.matches(TOO_FEW).count() >= 2, "both reasons: {error}");
    let brief = brief_of(&h, &item).await;
    assert_eq!(brief.matches(REPAIR_OPENING).count(), 1, "repaired once");
    assert_eq!(
        h.notices(NotificationKind::Outcome).await.len(),
        1,
        "told once"
    );
}

#[tokio::test]
async fn a_failure_that_is_not_a_record_refusal_is_not_repaired() {
    let (h, _gh) = delivering_repairing(true).await;
    let (item, _) = started(&h, "direct").await;
    h.settle_running(|_| StageOutcome::Failed {
        error: "the build is broken".into(),
        infra: false,
    })
    .await;
    h.dispatch().await;
    failed(&h, &item).await;
    assert!(!brief_of(&h, &item).await.contains(REPAIR_OPENING));
}

#[tokio::test]
async fn the_config_switch_turns_the_automatic_repair_off() {
    let (h, _gh) = delivering_repairing(false).await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Strict))
        .await
        .unwrap();
    h.dispatch().await;
    refused_lead(&h, &item).await;

    let error = failed(&h, &item).await;
    assert!(error.contains(TOO_FEW), "{error}");
    assert!(!brief_of(&h, &item).await.contains(REPAIR_OPENING));
}

#[tokio::test]
async fn a_cancelled_item_is_not_repaired() {
    let (h, _gh) = delivering_repairing(true).await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Strict))
        .await
        .unwrap();
    h.dispatch().await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancelled");
    h.dispatch().await;

    assert_eq!(h.state(&item.id).await.kind(), "cancelled");
    assert!(!brief_of(&h, &item).await.contains(REPAIR_OPENING));
}

#[tokio::test]
async fn a_steered_run_is_not_repaired() {
    let (h, _gh) = delivering_repairing(true).await;
    let item = enqueue(&h, "adaptive", asking(ReviewLevel::Strict))
        .await
        .unwrap();
    h.dispatch().await;
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: Steer {
            abort: Some(archon_core::steer::Then::Retry),
            ..Steer::default()
        },
    };
    h.orchestrator(steer).await.expect("steered");
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let edits = h.ctx.store.lock().await.run_edits(&run.id).unwrap();
    assert!(!edits.is_empty(), "the steer is on record");
    h.dispatch().await;
    refused_lead(&h, &item).await;

    failed(&h, &item).await;
    assert!(!brief_of(&h, &item).await.contains(REPAIR_OPENING));
}
