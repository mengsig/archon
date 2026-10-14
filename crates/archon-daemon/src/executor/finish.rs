//! Finishing runs and items: the atomic store write, the operator's feed
//! entries, the orchestrator trigger, and worktree cleanup.

use std::collections::BTreeSet;

use archon_core::catalog::{StageKind, is_snapshot_of};
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, PipelineRunId, QueueItemId};
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use archon_core::protocol::gib;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::{PipelineRun, RunState, StageOutcome, StageRun, StageState, Verdict};

use super::{Cleanup, FollowUp, RunTable, clip, frozen};
use crate::context::DaemonContext;
use crate::resolve;
use crate::triggers::{self, TriggerEvent};
use crate::worktree::WorktreeManager;

/// Feed text is clipped to this; the item keeps the full text.
const MAX_NOTICE_CHARS: usize = 2_000;
/// An `item_finished` summary is clipped to this. A batch holds at most
/// `compact::MAX_BATCH_EVENTS` news events, besides pinned ones.
const MAX_FINISHED_CHARS: usize = 4_000;

/// Finish a live run and settle its item in one store transaction, close
/// the reopen questions of its withdrawn gates' pages, then announce it.
/// Live stage agents are stopped by the returned follow-up.
pub(super) async fn finish_run(
    ctx: &DaemonContext,
    table: &mut RunTable,
    mut run: PipelineRun,
    state: RunState,
    blocker: bool,
) -> Result<FollowUp, ArchonError> {
    assert!(state.is_terminal(), "finish_run needs a terminal state");
    let ship_stage = if matches!(state, RunState::Succeeded { .. }) {
        match frozen::thaw(ctx, &run.id, &run.pipeline).await {
            Ok(spec) => spec
                .file
                .stages
                .iter()
                .find(|s| s.kind == StageKind::Ship)
                .map(|s| s.id.clone()),
            Err(error) => {
                tracing::warn!(%error, run = %run.id, "shipping proof unavailable; retaining workspace");
                None
            }
        }
    } else {
        None
    };
    let now = archon_core::now();
    let finished = {
        let mut store = ctx.store.lock().await;
        let estimates = ctx.config.limits.count_estimated_cost;
        let stages = store.stage_runs(&run.id)?;
        run.cost_usd = store.spend_for_run(&run.id, estimates)?;
        run.shipped = ship_stage.is_some_and(|id| {
            stages
                .iter()
                .any(|s| s.stage_id == id && s.state == StageState::Completed)
        });
        run.finish(state, now)?;
        store.finish_run(&run, item_state(&run), now)?
    };
    let item = finished.item;
    assert!(item.state.is_terminal(), "finish_run settles the item");
    frozen::discard(&ctx.paths.data_dir, &run.id);
    table.drivers.remove(&run.id);
    table.pending.remove(&run.id);
    table.merges.remove(&run.id);
    crate::feed::publish(ctx, finished.answers);
    crate::reports::close_stale_questions(ctx).await?;
    let pr_url = pr_url(ctx, &run).await?;
    announce(ctx, &item, Some(&run), blocker, pr_url).await?;
    let mut follow = FollowUp::poke();
    follow.stop.push(run.id.clone());
    follow.cleanup.extend(cleanup_for(ctx, &item, &run).await?);
    Ok(follow)
}

/// Fail a live run outright (policy refusal, exhausted budget, a pipeline
/// that no longer loads).
pub(super) async fn fail_run(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    stage_id: String,
    error: String,
    blocker: bool,
) -> Result<FollowUp, ArchonError> {
    if run.state.is_terminal() {
        return Ok(FollowUp::poke());
    }
    let state = RunState::Failed { stage_id, error };
    finish_run(ctx, table, run, state, blocker).await
}

fn item_state(run: &PipelineRun) -> QueueState {
    match &run.state {
        RunState::Succeeded { summary } => QueueState::Succeeded {
            run: run.id.clone(),
            summary: summary.clone(),
        },
        RunState::Failed { stage_id, error } => QueueState::Failed {
            run: Some(run.id.clone()),
            error: format!("stage {stage_id}: {error}"),
        },
        RunState::Cancelled { reason } => QueueState::Cancelled {
            reason: reason.clone(),
        },
        RunState::Running { .. } | RunState::AwaitingOperator { .. } => {
            unreachable!("item_state of a live run")
        }
    }
}

/// The newest `pr_url` a completed stage reported (the ship stage's).
async fn pr_url(ctx: &DaemonContext, run: &PipelineRun) -> Result<Option<String>, ArchonError> {
    let stages = ctx.store.lock().await.stage_runs(&run.id)?;
    Ok(archon_core::run::pr_url(stages.iter()).map(str::to_owned))
}

/// Worktree removal after a shipped success in `pr` mode, unless
/// `keep_on_success` (ARCHITECTURE §13). The branch is always kept. A
/// snapshot is removed however its run ended.
async fn cleanup_for(
    ctx: &DaemonContext,
    item: &QueueItem,
    run: &PipelineRun,
) -> Result<Option<Cleanup>, ArchonError> {
    assert!(run.state.is_terminal(), "only a finished run is cleaned up");
    let succeeded = matches!(run.state, RunState::Succeeded { .. });
    let in_worktrees = run.workspace.starts_with(&ctx.paths.worktrees);
    let snapshot = in_worktrees && is_snapshot_of(&run.workspace, &item.id);
    let kept = !succeeded || !run.shipped || !in_worktrees || ctx.config.worktrees.keep_on_success;
    if kept && !snapshot {
        return Ok(None);
    }
    let Some(repo_id) = &item.repo else {
        return Ok(None);
    };
    let repo = ctx.store.lock().await.repo(&resolve::repo_ref(repo_id))?;
    if !snapshot && repo.ship != ShipMode::Pr {
        return Ok(None);
    }
    Ok(Some(Cleanup {
        repo_path: repo.path,
        worktree: run.workspace.clone(),
        snapshot,
    }))
}

/// A failed removal is reported, not fatal: the run already succeeded (or,
/// for a snapshot, ended), or its start was abandoned (`when` says which).
/// The worktree's build caches go first, so a stray untracked file that
/// keeps the worktree (`git worktree remove` is never forced) keeps only
/// its source. A snapshot is removed with `--force` instead.
pub(super) async fn remove_worktree(
    ctx: &DaemonContext,
    cleanup: Cleanup,
    when: &str,
) -> Result<(), ArchonError> {
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    );
    let cache_dirs = ctx.config.worktrees.cache_dirs.clone();
    let target = cleanup.worktree.clone();
    let (reclaimed, removed) = tokio::task::spawn_blocking(move || {
        // A snapshot goes whole with `--force`, so its caches need no cleaning.
        if cleanup.snapshot {
            let removed = manager.remove_snapshot(&cleanup.repo_path, &cleanup.worktree);
            return (Ok(None), removed);
        }
        let reclaimed = crate::gc::cache::reclaim_worktree(&cleanup.worktree, &cache_dirs);
        (
            reclaimed,
            manager.remove(&cleanup.repo_path, &cleanup.worktree),
        )
    })
    .await
    .map_err(|e| ArchonError::io("worktree removal task", std::io::Error::other(e)))?;
    let Err(error) = removed else {
        // The caches went with the worktree whether or not cleaning them did.
        if let Err(error) = reclaimed {
            tracing::debug!(worktree = %target.display(), %error, "caches not cleaned first");
        }
        return Ok(());
    };
    tracing::warn!(worktree = %target.display(), %error, "worktree not removed");
    let caches = match reclaimed {
        Ok(Some(bytes)) => format!(" Its build caches were deleted ({}).", gib(bytes)),
        Ok(None) => String::new(),
        Err(e) => format!(" Deleting its build caches failed too: {e}"),
    };
    let text = format!(
        "Could not remove the worktree {} {when}: {error}.{caches}",
        target.display()
    );
    crate::feed::append(ctx, FeedEntry::System { text }).await?;
    Ok(())
}

/// One `Blocker` when the operator must act, otherwise an `Outcome`, and the
/// orchestrator's `item_finished` trigger.
pub(super) async fn announce(
    ctx: &DaemonContext,
    item: &QueueItem,
    run: Option<&PipelineRun>,
    blocker: bool,
    pr_url: Option<String>,
) -> Result<(), ArchonError> {
    assert!(
        item.state.is_terminal(),
        "only finished items are announced"
    );
    let summary = outcome_text(item);
    let notice = if matches!(item.state, QueueState::Failed { .. }) {
        let stages = match run {
            Some(run) => ctx.store.lock().await.stage_runs(&run.id)?,
            None => Vec::new(),
        };
        failure_notice(item, run, &stages)
    } else {
        clip(&summary, MAX_NOTICE_CHARS)
    };
    let needs_operator = blocker || summary.contains(StageOutcome::BLOCKED_PREFIX);
    let run = run.map(|r| &r.id);
    let kind = if needs_operator {
        NotificationKind::Blocker
    } else {
        NotificationKind::Outcome
    };
    notify(ctx, kind, &notice, Some(&item.id), run).await?;
    let cost_usd = ctx
        .store
        .lock()
        .await
        .spend_for_item(&item.id, ctx.config.limits.count_estimated_cost)?;
    let finished = TriggerEvent::ItemFinished {
        item: item.id.clone(),
        title: item.title.clone(),
        state: item.state.kind().to_owned(),
        summary: finished_summary(&summary, item),
        pr_url,
        cost_usd,
        origin: item.origin.clone(),
    };
    triggers::push(ctx, finished).await
}

/// Keep the notification small; the queue, stages and orchestrator retain the
/// full error. Read the failing member instead of the engine's nested report.
fn failure_notice(item: &QueueItem, run: Option<&PipelineRun>, stages: &[StageRun]) -> String {
    let QueueState::Failed { error, .. } = &item.state else {
        unreachable!("failure notice requires a failed item");
    };
    let (stage, fallback) = match run.map(|r| &r.state) {
        Some(RunState::Failed { stage_id, error }) => (stage_id.as_str(), error.as_str()),
        _ => ("startup", error.as_str()),
    };
    let attempt = stages
        .iter()
        .filter(|s| s.stage_id == stage)
        .map(|s| s.attempt)
        .max();
    let mut slots = BTreeSet::new();
    let cause = stages
        .iter()
        .rev()
        .filter(|s| s.stage_id == stage && Some(s.attempt) == attempt)
        // Infrastructure retries replace a slot within the same attempt.
        .filter(|s| slots.insert(s.fan_index))
        .find_map(|s| match &s.state {
            StageState::Failed { error } => Some(error.as_str()),
            StageState::TimedOut => Some("The stage timed out."),
            StageState::Completed if s.verdict == Some(Verdict::Fail) => s
                .structured
                .as_ref()
                .and_then(|v| v.get("findings"))
                .and_then(|v| v.as_array())
                .and_then(|v| v.first())
                .and_then(|v| v.get("title"))
                .and_then(|v| v.as_str())
                .or(s.summary.as_deref()),
            _ => None,
        })
        .unwrap_or(fallback);
    let title = item.title.split_whitespace().collect::<Vec<_>>().join(" ");
    let cause = notice_sentence(cause);
    let punctuation = if cause.ends_with(['.', '!', '?', '…']) {
        ""
    } else {
        "."
    };
    format!(
        "Failed: {} — {stage}: {cause}{punctuation} Full detail: archon queue show {} --full",
        clip(&title, 120),
        item.id
    )
}

fn notice_sentence(text: &str) -> String {
    // Preserve the daemon's distinction between its disk and Aether's,
    // which otherwise sits after the first sentence of this diagnostic.
    if text.starts_with("create_agent failed on a full disk ")
        && text.contains("full disk is likely Aether's")
    {
        return "Aether's disk appears full after repeated create_agent failures (Archon's disk had room).".into();
    }
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("**Stage "))
        .unwrap_or("No failure detail was recorded.");
    let plain: String = line
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '`' | '*'))
        .collect();
    let sentence = plain
        .split_once(". ")
        .map_or(plain.as_str(), |(first, _)| first);
    clip(sentence, 240)
}

fn outcome_text(item: &QueueItem) -> String {
    let text = match &item.state {
        QueueState::Succeeded { summary, .. } => format!("Done: {}. {summary}", item.title),
        QueueState::Failed { error, .. } => format!("Failed: {}. {error}", item.title),
        QueueState::Cancelled { reason: Some(r) } => format!("Cancelled: {}. {r}", item.title),
        QueueState::Cancelled { reason: None } => format!("Cancelled: {}.", item.title),
        other => unreachable!("outcome of a {} item", other.kind()),
    };
    text.trim().to_owned()
}

/// `text` for the orchestrator; when clipped, it says where the rest is.
fn finished_summary(text: &str, item: &QueueItem) -> String {
    let total = text.chars().count();
    if total <= MAX_FINISHED_CHARS {
        return text.to_owned();
    }
    let kept = clip(text, MAX_FINISHED_CHARS);
    assert!(kept.chars().count() < total, "a clip shortens the text");
    match stage_hint(item) {
        Some(hint) => format!("{kept} [clipped: {total} chars in full; {hint}]"),
        None => format!("{kept} [clipped: {total} chars in full]"),
    }
}

/// Where a finished run's stages tell the whole story. A cancelled item's
/// text is its cancel reason, and it may have no run, so it gets none.
fn stage_hint(item: &QueueItem) -> Option<String> {
    assert!(
        item.state.is_terminal(),
        "only finished items are summarized"
    );
    let run = match &item.state {
        QueueState::Succeeded { run, .. } | QueueState::Failed { run: Some(run), .. } => run,
        _ => return None,
    };
    Some(format!(
        "get_stage with item {}, run {run} lists its stages, and with a stage reads that \
         stage's full summary",
        item.id
    ))
}

/// A daemon-authored notification, optionally about an item and run.
pub(crate) async fn notify(
    ctx: &DaemonContext,
    kind: NotificationKind,
    text: &str,
    item: Option<&QueueItemId>,
    run: Option<&PipelineRunId>,
) -> Result<(), ArchonError> {
    assert!(!text.trim().is_empty(), "notifications say something");
    let entry = FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind,
        text: clip(text, MAX_NOTICE_CHARS),
        item: item.cloned(),
        run: run.cloned(),
        at: archon_core::now(),
    });
    crate::feed::append(ctx, entry).await.map(|_| ())
}

/// Cancel an item: a live run is finished `cancelled` (its agents are then
/// stopped); an item without one is cancelled directly.
pub(super) async fn cancel_item(
    ctx: &DaemonContext,
    table: &mut RunTable,
    id: &QueueItemId,
    reason: &str,
) -> Result<FollowUp, ArchonError> {
    let reason = Some(reason.trim().to_owned()).filter(|r| !r.is_empty());
    let (item, run) = {
        let store = ctx.store.lock().await;
        let item = store.item(id)?;
        let run = match &item.state {
            QueueState::Running { run } => Some(store.run(run)?),
            _ => None,
        };
        (item, run)
    };
    if let Some(run) = run {
        let state = RunState::Cancelled { reason };
        return finish_run(ctx, table, run, state, false).await;
    }
    if item.state.is_terminal() {
        return Err(ArchonError::InvalidTransition(format!(
            "item {id} is already {}",
            item.state.kind()
        )));
    }
    let next = QueueState::Cancelled { reason };
    let item = ctx
        .store
        .lock()
        .await
        .transition_item(id, next, archon_core::now())?;
    announce(ctx, &item, None, false, None).await?;
    Ok(FollowUp::poke())
}
