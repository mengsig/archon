//! Retry controls: a fresh retry starts again from the base branch with
//! the old worktree set aside, a plain retry keeps the item's worktree and
//! branch, and a brief amendment reaches the next run's stage prompts.

use crate::common;

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan};
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ItemPatch, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::PipelineRun;
use archon_daemon::worktree::WorktreeManager;
use common::{Harness, MODEL, REPO_NAME, choice, git};

fn retry(item: &QueueItem, fresh: bool, brief_append: Option<&str>) -> Request {
    Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh,
        brief_append: brief_append.map(str::to_owned),
        stage_timeouts: Default::default(),
    }
}

fn amend(item: &QueueItem, text: &str) -> Request {
    Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            brief_append: Some(text.into()),
            ..ItemPatch::default()
        },
    }
}

async fn cancel(h: &Harness, item: &QueueItem) {
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
}

/// Enqueue `title`, start its run and return it with the item.
async fn started(h: &Harness, title: &str) -> (QueueItem, PipelineRun) {
    let item = h.enqueue(title).await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "the implementer runs");
    let run = latest_run(h, &item).await;
    (item, run)
}

async fn latest_run(h: &Harness, item: &QueueItem) -> PipelineRun {
    let runs = h
        .ctx
        .store
        .lock()
        .await
        .runs_for_item(&item.id)
        .expect("runs");
    runs.into_iter()
        .max_by_key(|r| r.started_at)
        .expect("a run")
}

/// Commit `file` on whatever `tree` has checked out.
fn commit_file(tree: &Path, file: &str, text: &str) -> String {
    std::fs::write(tree.join(file), text).expect("write");
    git(tree, &["add", file]);
    git(tree, &["commit", "--quiet", "-m", file]);
    git(tree, &["rev-parse", "HEAD"])
}

fn set_aside_dir(h: &Harness) -> PathBuf {
    h.ctx.paths.worktrees.join(REPO_NAME).join(".set-aside")
}

#[tokio::test]
async fn a_fresh_retry_forks_from_the_base_head_and_keeps_the_old_work_aside() {
    let h = Harness::new().await;
    let (item, first) = started(&h, "start over").await;
    let unpushed = commit_file(&first.workspace, "first.txt", "first try\n");
    std::fs::write(first.workspace.join("draft.txt"), "uncommitted\n").expect("draft");
    let base = commit_file(&h.repo, "base.txt", "main moved on\n");
    cancel(&h, &item).await;

    h.cli(retry(&item, true, None)).await.expect("fresh retry");
    let queued = h.item(&item.id).await;
    assert_eq!(
        (queued.state, queued.fresh_retries),
        (QueueState::Queued, 1)
    );
    let kept = set_aside_dir(&h).join(format!("{}-1", item.id));
    assert_eq!(
        std::fs::read_to_string(kept.join("draft.txt")).expect("kept"),
        "uncommitted\n",
        "uncommitted work is moved aside, not lost"
    );
    assert!(kept.join("first.txt").exists());
    assert_eq!(
        git(
            &h.repo,
            &["rev-parse", &format!("refs/heads/{}", first.branch)]
        ),
        unpushed,
        "the old branch keeps its commits"
    );

    h.dispatch().await;
    let second = latest_run(&h, &item).await;
    assert_ne!(second.id, first.id);
    assert_eq!(second.workspace, first.workspace, "the item's path is kept");
    assert_eq!(second.branch, format!("{}-fresh1", first.branch));
    assert_eq!(git(&second.workspace, &["rev-parse", "HEAD"]), base);
    assert!(!second.workspace.join("draft.txt").exists());
    assert!(!second.workspace.join("first.txt").exists());
    assert_eq!(
        h.backend.requests().last().expect("spawn").workspace,
        second.workspace
    );
    let notes = h.notices(NotificationKind::Progress).await;
    let note = notes.iter().find(|n| n.contains("fresh")).expect("a note");
    assert!(
        note.contains(&second.branch) && note.contains(&kept.display().to_string()),
        "{note}"
    );
    let expiry = format!("`worktrees.gc_after` ({})", h.ctx.config.worktrees.gc_after);
    assert!(
        note.contains("Worktree gc deletes the set-aside worktree") && note.contains(&expiry),
        "the note says when the set-aside work goes: {note}"
    );
}

/// After a fresh retry the item's runs stay on its new branch: a plain
/// retry reuses that worktree and branch with the work left in it.
#[tokio::test]
async fn a_plain_retry_after_a_fresh_one_keeps_the_new_branch_and_its_work() {
    let h = Harness::new().await;
    let (item, first) = started(&h, "twice").await;
    cancel(&h, &item).await;
    h.cli(retry(&item, true, None)).await.expect("fresh retry");
    h.dispatch().await;
    let second = latest_run(&h, &item).await;
    std::fs::write(second.workspace.join("draft.txt"), "second try\n").expect("draft");
    cancel(&h, &item).await;

    h.cli(retry(&item, false, None)).await.expect("plain retry");
    assert_eq!(h.item(&item.id).await.fresh_retries, 1, "unchanged");
    h.dispatch().await;
    let third = latest_run(&h, &item).await;
    assert_ne!(third.id, second.id);
    assert_eq!(
        (&third.workspace, &third.branch),
        (&second.workspace, &second.branch)
    );
    assert_ne!(third.branch, first.branch);
    let draft = std::fs::read_to_string(third.workspace.join("draft.txt")).expect("kept");
    assert_eq!(draft, "second try\n");
    assert!(!set_aside_dir(&h).join(format!("{}-2", item.id)).exists());
}

/// A worktree `archon gc` removed leaves nothing to set aside; the branch
/// the fresh run would fork must not exist yet, or nothing moves at all.
#[tokio::test]
async fn a_fresh_retry_needs_a_new_branch_but_no_worktree() {
    let h = Harness::new().await;
    let (item, first) = started(&h, "gc'd").await;
    cancel(&h, &item).await;
    let taken = format!("{}-fresh1", first.branch);
    git(&h.repo, &["branch", &taken, "main"]);
    let refused = h.cli(retry(&item, true, None)).await.expect_err("taken");
    assert!(
        matches!(&refused, ArchonError::Invalid(why) if why.contains(&taken)),
        "{refused:?}"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    assert!(first.workspace.join(".git").exists(), "nothing was moved");

    git(&h.repo, &["branch", "-D", &taken]);
    let manager = WorktreeManager::new(
        h.ctx.paths.worktrees.clone(),
        h.ctx.config.worktrees.branch_prefix.clone(),
    );
    manager
        .remove(&h.ctx.paths.data_dir, &h.repo, &first.workspace)
        .expect("gc");
    h.cli(retry(&item, true, None)).await.expect("fresh retry");
    assert!(!set_aside_dir(&h).exists(), "nothing to set aside");
    let notes = h.notices(NotificationKind::Progress).await;
    let note = notes.iter().find(|n| n.contains("fresh")).expect("a note");
    assert!(
        !note.contains("gc_after"),
        "nothing set aside to expire: {note}"
    );
    h.dispatch().await;
    let second = latest_run(&h, &item).await;
    assert_eq!(second.branch, taken);
    assert_eq!(
        git(&second.workspace, &["rev-parse", "HEAD"]),
        git(&h.repo, &["rev-parse", "main"])
    );
}

#[tokio::test]
async fn a_fresh_retry_is_refused_for_a_pipeline_without_a_worktree() {
    let h = Harness::new().await;
    h.add_pipeline(
        "scratch-note",
        r#"[pipeline]
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
"#,
    );
    let mut plan = ModelPlan::default();
    plan.0
        .insert("implement".into(), vec![choice(MODEL, Effort::Medium)]);
    let item = h
        .enqueue_in("scratch-note", "no branch", plan)
        .await
        .expect("enqueue");
    cancel(&h, &item).await;
    let refused = h.cli(retry(&item, true, None)).await.expect_err("scratch");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert_eq!(h.item(&item.id).await.fresh_retries, 0);
    h.cli(retry(&item, false, None))
        .await
        .expect("a plain retry");
}

#[tokio::test]
async fn an_amendment_on_retry_reaches_the_next_runs_stage_prompt() {
    let h = Harness::new().await;
    let (item, _) = started(&h, "amended").await;
    let first = h.backend.requests().pop().expect("implementer");
    cancel(&h, &item).await;

    let guidance = "The reviewer found the greeting untested; add a test for it.";
    h.cli(retry(&item, false, Some(guidance)))
        .await
        .expect("retry with an amendment");
    let brief = h.item(&item.id).await.brief;
    assert!(brief.starts_with(&item.brief), "{brief}");
    assert!(brief.contains("## Amendment ("), "{brief}");
    h.dispatch().await;
    let next = h.backend.requests().pop().expect("a new implementer");
    assert!(!first.objective.contains(guidance));
    assert!(next.objective.contains(guidance), "{}", next.objective);
    assert!(next.objective.contains("## Amendment ("));
    let notes = h.notices(NotificationKind::Progress).await;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("retry_item") && n.contains(guidance)),
        "recorded in the item's history: {notes:?}"
    );

    cancel(&h, &item).await;
    let blank = h.cli(retry(&item, false, Some("  \n"))).await;
    assert!(matches!(blank, Err(ArchonError::Invalid(_))), "{blank:?}");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
}

#[tokio::test]
async fn update_item_amends_the_brief_of_a_running_item_without_a_cancel() {
    let h = Harness::new().await;
    let item = h.enqueue("amend me").await;
    h.cli(amend(&item, "Keep the public API as it is."))
        .await
        .expect("a queued item takes an amendment");
    h.dispatch().await;
    let request = h.backend.requests().pop().expect("implementer");
    assert!(request.objective.contains("Keep the public API as it is."));

    h.cli(amend(&item, "Mid-run change."))
        .await
        .expect("a running item is steered");
    let agent = h.running().await[0].aether_agent.clone().expect("agent");
    let told = h.backend.messages();
    assert!(
        told.iter()
            .any(|(to, text)| to == &agent && text.contains("Mid-run change.")),
        "the running agent is told: {told:?}"
    );
    assert!(h.backend.cancelled().is_empty(), "and it goes on");
    assert!(h.item(&item.id).await.brief.contains("Mid-run change."));

    cancel(&h, &item).await;
    h.cli(amend(&item, "Also update the docs."))
        .await
        .expect("a cancelled item takes an amendment");
    let blank = h.cli(amend(&item, " ")).await;
    assert!(matches!(blank, Err(ArchonError::Invalid(_))), "{blank:?}");
    h.cli(retry(&item, false, None)).await.expect("retry");
    h.dispatch().await;
    let next = h.backend.requests().pop().expect("a new implementer");
    let amendments = [
        "Keep the public API as it is.",
        "Mid-run change.",
        "Also update the docs.",
    ];
    for text in amendments {
        assert!(next.objective.contains(text), "{text}");
    }
}

#[tokio::test]
async fn a_retry_sets_stage_timeouts_within_the_limit() {
    let h = Harness::new().await;
    let (item, _) = started(&h, "slow stage").await;
    cancel(&h, &item).await;
    let with = |stage: &str, minutes: u32| Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: [(stage.to_owned(), minutes)].into(),
    };

    for (stage, minutes, said) in [
        ("implement", 100_000, "max_stage_timeout_minutes"),
        ("no-such-stage", 30, "does not have"),
    ] {
        let refused = h.cli(with(stage, minutes)).await;
        assert!(
            matches!(&refused, Err(ArchonError::Invalid(m)) if m.contains(said)),
            "{refused:?}"
        );
        assert!(matches!(
            h.state(&item.id).await,
            QueueState::Cancelled { .. }
        ));
    }
    h.cli(with("implement", 45)).await.expect("retry");
    let queued = h.item(&item.id).await;
    assert_eq!(queued.state, QueueState::Queued);
    assert_eq!(queued.stage_timeouts["implement"], 45);
}

#[tokio::test]
async fn retry_checks_a_kept_pin_against_the_current_catalog() {
    let h = Harness::new().await;
    let item = h.enqueue("removed pin").await;
    cancel(&h, &item).await;
    h.set_learning_models(vec![common::REVIEW_MODEL.parse().unwrap()])
        .await;
    let error = h
        .cli(retry(&item, false, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(MODEL), "{error}");
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
}

#[tokio::test]
async fn retry_of_a_model_pin_block_keeps_independently_blocked_dependents() {
    let h = Harness::new().await;
    let item = h.enqueue("replan").await;
    let mut dependent = h.enqueue("independent invalid pin").await;
    let blocked = QueueState::model_pin_block("re-plan the missing model");
    {
        let mut store = h.ctx.store.lock().await;
        dependent.dependencies.push(item.id.clone());
        store.update_item(&dependent).unwrap();
        for id in [&item.id, &dependent.id] {
            store
                .transition_item(id, blocked.clone(), archon_core::now())
                .unwrap();
        }
    }
    let mut request = retry(&item, false, None);
    let Request::RetryItem { model_plan, .. } = &mut request else {
        unreachable!()
    };
    *model_plan = Some(common::plan().into());
    h.cli(request).await.expect("retry with a checked plan");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    assert_eq!(h.state(&dependent.id).await, blocked);
}

#[tokio::test]
async fn a_plain_retry_repicks_a_removed_daemon_choice() {
    use archon_core::plan_origin::{PlanSource, PlanWriter};
    let h = Harness::new().await;
    let mut item = h.enqueue("retry fallback").await;
    let mut writer = PlanWriter::new(&item.id, archon_core::now());
    writer.filled(
        item.model_plan.plan().clone(),
        (PlanSource::Fallback, PlanSource::Fallback),
    );
    item.model_plan = writer.finish(archon_core::plan_origin::Sizing::Off { policy_cap: None });
    h.ctx.store.lock().await.update_item(&item).unwrap();
    cancel(&h, &item).await;
    h.set_learning_models(vec![common::REVIEW_MODEL.parse().unwrap()])
        .await;
    h.cli(retry(&item, false, None))
        .await
        .expect("repick removed fallback");
    let queued = h.item(&item.id).await;
    assert_eq!(queued.state, QueueState::Queued);
    assert_eq!(
        queued.model_plan.0["implement"][0].model.to_string(),
        common::REVIEW_MODEL
    );
}

#[tokio::test]
async fn steers_repair_multiple_removed_pins_before_a_plain_retry() {
    let h = Harness::new().await;
    let mut plan = common::plan();
    plan.0.get_mut("review").unwrap().truncate(1);
    let item = h.enqueue_with("repair two pins", plan).await.unwrap();
    h.set_learning_models(vec![common::REVIEW_MODEL.parse().unwrap()])
        .await;
    h.dispatch().await;
    assert!(h.state(&item.id).await.is_model_pin_block());
    for stage in ["implement", "ship"] {
        h.cli(Request::SteerItem {
            item: item.id.clone(),
            steer: archon_core::steer::Steer {
                stage: Some(stage.into()),
                model: Some(common::REVIEW_MODEL.parse().unwrap()),
                ..Default::default()
            },
        })
        .await
        .expect("change one pin independently");
    }
    h.cli(retry(&item, false, None))
        .await
        .expect("retry repaired pins");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    h.dispatch().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

/// A failure only advises: the reported failure suggests a step up, and a
/// retry without a `model_plan` runs the stored plan's model and effort.
#[tokio::test]
async fn a_retry_without_a_model_plan_keeps_the_stored_plan_after_a_failure() {
    use archon_core::run::StageOutcome;
    use archon_daemon::triggers::{self, TriggerEvent};

    let h = Harness::new().await;
    let (item, _) = started(&h, "checks fail").await;
    let ran = h.backend.requests()[0].choice.clone();
    assert_eq!(ran, choice(MODEL, Effort::Medium), "the stored plan ran");
    h.settle_running(|_| StageOutcome::Failed {
        error: "make ci failed: 2 tests failed".into(),
        infra: false,
    })
    .await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let held = triggers::held(&h.ctx).await.unwrap();
    assert!(
        held.iter().any(|event| matches!(event,
            TriggerEvent::ItemFinished { item: id, summary, .. } if id == &item.id
                && summary.contains("Failure class: structural")
                && summary.contains("consider one effort step up from medium"))),
        "{held:?}"
    );

    h.cli(retry(&item, false, None)).await.expect("retry");
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 2, "the retry spawned once");
    assert_eq!(requests[1].choice, ran, "nothing escalates");
    let stored = h.ctx.store.lock().await.item(&item.id).unwrap().model_plan;
    assert_eq!(
        stored.plan().0["implement"],
        vec![ran],
        "the stored plan is unchanged"
    );
}
