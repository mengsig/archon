//! Finishing runs and items: the atomic store write, the operator's feed
//! entries, the orchestrator trigger, and worktree cleanup.

use std::collections::BTreeSet;

use archon_core::catalog::{StageKind, is_snapshot_of};
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, PipelineRunId, QueueItemId};
use archon_core::notify::{FeedEntry, Notification, NotificationKind};
use archon_core::protocol::ItemQuery;
use archon_core::protocol::gib;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::{PipelineRun, RunState, StageOutcome, StageRun, StageState, Verdict};
use archon_queue::Store;
use chrono::TimeDelta;

use super::repair::{self, Decision, Repair};
use super::{Cleanup, FollowUp, ItemBuild, RunTable, Teardown, clip, frozen};
use crate::context::DaemonContext;
use crate::resolve;
use crate::triggers::{self, TriggerEvent};
use crate::worktree::WorktreeManager;
use crate::worktree::users::{self, LiveWorkspaces};

/// Feed text is clipped to this; the item keeps the full text.
const MAX_NOTICE_CHARS: usize = 2_000;
/// An `item_finished` summary is clipped to this. A batch holds at most
/// `compact::MAX_BATCH_EVENTS` news events, besides pinned ones.
const MAX_FINISHED_CHARS: usize = 4_000;
/// Longest a routine success waits for the rest of the work in flight before
/// it wakes the orchestrator; the feed shows the outcome at once.
const MAX_SUCCESS_DEFER: TimeDelta = TimeDelta::minutes(5);

/// Finish a live run and settle its item in one store transaction, close
/// the reopen questions of its withdrawn gates' pages, then announce it.
/// Live stage agents are stopped by the returned follow-up.
pub(super) async fn finish_run(
    ctx: &DaemonContext,
    table: &mut RunTable,
    mut run: PipelineRun,
    mut state: RunState,
    blocker: bool,
) -> Result<FollowUp, ArchonError> {
    assert!(state.is_terminal(), "finish_run needs a terminal state");
    let mut repair = None;
    match repair::decide(ctx, &run, &state).await? {
        Decision::Finish => {}
        Decision::Repair(planned) => repair = Some(planned),
        Decision::Spent { first } => state = repair::with_first_refusal(state, &first),
    }
    let proof = match &state {
        RunState::Succeeded { .. } => shipping_proof(ctx, &run).await,
        _ => ShippingProof::None,
    };
    Box::pin(delete_role_branches(ctx, &run)).await?;
    let now = archon_core::now();
    let finished = {
        let mut store = ctx.store.lock().await;
        run.cost_usd = store.spend_for_run(&run.id, archon_core::cost::API_EQUIVALENT)?;
        run.shipped = match &proof {
            ShippingProof::Daemon => true,
            ShippingProof::Stage(id) => store
                .stage_runs(&run.id)?
                .iter()
                .any(|s| &s.stage_id == id && s.state == StageState::Completed),
            ShippingProof::None => false,
        };
        run.finish(state, now)?;
        let mut finished = store.finish_run(&run, item_state(&run), now)?;
        if let Some(planned) = &repair {
            match planned.requeue(&mut store) {
                Ok(queued) => finished.item = queued,
                Err(error) => {
                    tracing::error!(item = %run.item, %error, "automatic repair not queued; the item stays failed");
                    repair = None;
                }
            }
        }
        finished
    };
    crate::learned::finished(ctx, &run).await;
    let item = finished.item;
    frozen::discard(&ctx.paths.data_dir, &run.id);
    table.drivers.remove(&run.id);
    table.pending.remove(&run.id);
    table.merges.remove(&run.id);
    table.ships.remove(&run.id);
    table.ship_retries.remove(&run.id);
    table.gates.forget(&run.id);
    crate::feed::publish(ctx, finished.answers);
    crate::reports::close_stale_questions(ctx).await?;
    if let Some(repaired) = repair {
        let mut follow = repaired_follow_up(ctx, &run, &repaired).await?;
        follow.stacks.extend(stacks_of(ctx, &run).await?);
        return Ok(follow);
    }
    assert!(item.state.is_terminal(), "finish_run settles the item");
    let pr_url = pr_url(ctx, &run).await?;
    announce(ctx, &item, Some(&run), blocker, pr_url).await?;
    let mut follow = FollowUp::poke();
    follow.stop.push(run.id.clone());
    follow.stacks.extend(stacks_of(ctx, &run).await?);
    follow.cleanup.extend(cleanup_for(ctx, &item, &run).await?);
    follow.release_builds.extend(item_build(ctx, &item).await?);
    follow.discard.push(run.id.clone());
    Ok(follow)
}

/// The run of an item queued for its one repair retry has ended: say so in
/// the feed instead of announcing a failure. The worktree and the item's
/// build output stay for the retry.
async fn repaired_follow_up(
    ctx: &DaemonContext,
    run: &PipelineRun,
    repair: &Repair,
) -> Result<FollowUp, ArchonError> {
    notify(
        ctx,
        NotificationKind::Progress,
        &repair.feed_text(),
        Some(&repair.item),
        Some(&run.id),
    )
    .await?;
    let mut follow = FollowUp::poke();
    follow.stop.push(run.id.clone());
    follow.discard.push(run.id.clone());
    Ok(follow)
}

/// The teardown of the Compose stacks a finished run's stages may have left
/// in its worktree (a team run's whole directory) when Archon made it and no
/// other live item uses it. Its mark keeps a requeued or retried run of the
/// worktree from starting until the teardown is done; the caller holds the
/// run table, as dispatch does.
async fn stacks_of(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<Option<Teardown>, ArchonError> {
    assert!(run.state.is_terminal(), "only a finished run's stacks go");
    let tree = run
        .team_root
        .clone()
        .unwrap_or_else(|| run.workspace.clone());
    if !tree.starts_with(&ctx.paths.worktrees) || tree == ctx.paths.worktrees {
        return Ok(None);
    }
    let (owner, sharer) = {
        let store = ctx.store.lock().await;
        let live = LiveWorkspaces::read(&store, &ctx.paths.worktrees)?;
        let sharer = live.user_besides(&tree, Some(&run.item)).cloned();
        (users::owner(&store, &run.item)?, sharer)
    };
    // A restack fix or refresh may run there already; its stacks are its own.
    if let Some(sharer) = sharer {
        tracing::warn!(item = %run.item, %sharer, tree = %tree.display(), "another item works in the worktree; its compose stacks are left running");
        return Ok(None);
    }
    let Some(mark) = ctx.collecting.mark(owner.clone()) else {
        // Its holder works in the worktree; `archon doctor` names what it leaves.
        tracing::warn!(item = %run.item, %owner, tree = %tree.display(), "the worktree is busy; its compose stacks are left running");
        return Ok(None);
    };
    assert!(
        ctx.collecting.contains(&owner),
        "the teardown holds the owner"
    );
    Ok(Some(Teardown {
        item: run.item.clone(),
        tree,
        mark,
    }))
}

/// A team run's role branches that reached its run branch go with the run,
/// whether or not the cleanup after their merge ran. This is before the run
/// is persisted as finished, so a crash in between repeats it.
async fn delete_role_branches(ctx: &DaemonContext, run: &PipelineRun) -> Result<(), ArchonError> {
    assert!(
        run.team_root.is_none() || !run.branch.is_empty(),
        "team runs have a branch"
    );
    if run.team_root.is_none() {
        return Ok(());
    }
    let (item, repo) = {
        let store = ctx.store.lock().await;
        let item = store.item(&run.item)?;
        let repo = match &item.repo {
            Some(id) => Some(store.repo(&resolve::repo_ref(id))?),
            None => None,
        };
        (item, repo)
    };
    if let Some(repo) = repo {
        super::team::delete_branches(ctx, &item.id, &run.id, repo.path, run.branch.clone()).await;
    }
    Ok(())
}

/// The build directory a settled item leaves behind, if it has a repository,
/// owed its release so that gc leaves it to the item's end.
async fn item_build(
    ctx: &DaemonContext,
    item: &QueueItem,
) -> Result<Option<ItemBuild>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let build = build_of(&store, item)?;
    if let Some(build) = &build {
        crate::build_cache::owe_release(&mut store, &build.item, &build.repo_name)?;
    }
    Ok(build)
}

/// The build directory of `run`'s item, if it has a repository.
pub(super) fn run_build(
    store: &Store,
    run: &PipelineRunId,
) -> Result<Option<ItemBuild>, ArchonError> {
    build_of(store, &store.item(&store.run(run)?.item)?)
}

fn build_of(store: &Store, item: &QueueItem) -> Result<Option<ItemBuild>, ArchonError> {
    let Some(repo_id) = &item.repo else {
        return Ok(None);
    };
    let repo = store.repo(&resolve::repo_ref(repo_id))?;
    Ok(Some(ItemBuild {
        item: item.id.clone(),
        repo_name: repo.name,
    }))
}

/// What shows that a succeeded run shipped.
enum ShippingProof {
    /// archond delivered it: such a run succeeds only once it has.
    Daemon,
    /// Its pipeline's ship agent, whose stage must have completed.
    Stage(String),
    /// The pipeline ships nothing, or no longer loads.
    None,
}

async fn shipping_proof(ctx: &DaemonContext, run: &PipelineRun) -> ShippingProof {
    let spec = match frozen::thaw(ctx, &run.id, &run.pipeline).await {
        Ok(spec) => spec,
        Err(error) => {
            tracing::warn!(%error, run = %run.id, "shipping proof unavailable; retaining workspace");
            return ShippingProof::None;
        }
    };
    if spec.ship_record_stage().is_some() {
        return ShippingProof::Daemon;
    }
    let agent = spec.file.stages.iter().find(|s| s.kind == StageKind::Ship);
    agent.map_or(ShippingProof::None, |s| ShippingProof::Stage(s.id.clone()))
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
    let stages = match ctx.store.lock().await.stage_runs(&run.id) {
        Ok(stages) => stages,
        Err(error) if !error.is_store() => return Ok(None),
        Err(error) => return Err(error),
    };
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
        item: ctx
            .store
            .lock()
            .await
            .stack_fix(&item.id)?
            .map_or_else(|| item.id.clone(), |fix| fix.owner),
        repo_path: repo.path,
        worktree: run.workspace.clone(),
        snapshot,
    }))
}

/// A failed removal is reported, not fatal: the run already succeeded (or,
/// for a snapshot, ended), or its start was abandoned (`when` says which).
/// The worktree's build caches go first, so a stray untracked file that
/// keeps the worktree (a change is never discarded) keeps only its source.
/// A snapshot is removed with `--force` instead. Its stacks are the caller's
/// to tear down first, off the run table.
pub(super) async fn remove_worktree(
    ctx: &DaemonContext,
    cleanup: Cleanup,
    when: &str,
    mark: Option<crate::gc::Mark>,
) -> Result<(), ArchonError> {
    let cache_dirs = ctx.config.worktrees.cache_dirs.clone();
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    )
    .with_cache_dirs(cache_dirs.clone());
    let data_dir = ctx.paths.data_dir.clone();
    let target = cleanup.worktree.clone();
    let (reclaimed, removed) = tokio::task::spawn_blocking(move || {
        let _mark = mark;
        // A snapshot goes whole with `--force`, so its caches need no cleaning.
        if cleanup.snapshot {
            let removed = manager.remove_snapshot(&cleanup.repo_path, &cleanup.worktree);
            return (Ok(None), removed);
        }
        let reclaimed = crate::gc::cache::reclaim_worktree(&cleanup.worktree, &cache_dirs);
        (
            reclaimed,
            manager.remove(&data_dir, &cleanup.repo_path, &cleanup.worktree),
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
/// orchestrator's `item_finished` news: a trigger when it has something to
/// do about it, otherwise a line of its next trigger (ARCHITECTURE §5.4).
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
            Some(run) => match ctx.store.lock().await.stage_runs(&run.id) {
                Err(error) if !error.is_store() => {
                    tracing::warn!(run = %run.id, %error, "stage rows unreadable; notice without them");
                    Vec::new()
                }
                other => other?,
            },
            None => Vec::new(),
        };
        failure_notice(item, run, &stages)
    } else {
        let clipped = clip(&summary, MAX_NOTICE_CHARS);
        match local_notice(ctx, item).await {
            Some(extra) => format!("{clipped}\n{extra}"),
            None => clipped,
        }
    };
    let needs_operator = blocker || failed_blocked(item);
    let run = run.map(|r| &r.id);
    let kind = if needs_operator {
        NotificationKind::Blocker
    } else {
        NotificationKind::Outcome
    };
    notify(ctx, kind, &notice, Some(&item.id), run).await?;
    crate::push::finished(ctx, item, needs_operator);
    ctx.coordination.changed.notify_one();
    if item.initiative.is_some() {
        ctx.dispatch.notify_one();
        return Ok(());
    }
    let cost_usd = ctx
        .store
        .lock()
        .await
        .spend_for_item(&item.id, archon_core::cost::API_EQUIVALENT)?;
    let finished = TriggerEvent::ItemFinished {
        item: item.id.clone(),
        title: item.title.clone(),
        state: item.state.kind().to_owned(),
        summary: finished_summary(&summary, item),
        pr_url,
        cost_usd,
        origin: item.origin.clone(),
        lesson_hint: crate::learned::lesson_hint(&*ctx.store.lock().await, item)?,
    };
    // Its base branch may have moved, and the operator may ask about it next.
    if let Some(repo) = &item.repo {
        ctx.mirrors.request_repo(repo);
    }
    tell_orchestrator(ctx, item, finished, needs_operator).await
}

/// Hand `finished` to the orchestrator; a routine success waits up to [`MAX_SUCCESS_DEFER`]
/// for other running work so one turn reports both, unless it is the last item.
async fn tell_orchestrator(
    ctx: &DaemonContext,
    item: &QueueItem,
    finished: TriggerEvent,
    needs_operator: bool,
) -> Result<(), ArchonError> {
    assert!(
        matches!(&finished, TriggerEvent::ItemFinished { item: id, .. } if *id == item.id),
        "the news is of this item"
    );
    if !needs_operator && !wakes_orchestrator(item, ctx.config.orchestrator.wake_on_success) {
        return triggers::push_quiet(ctx, finished).await;
    }
    let mut store = ctx.store.lock().await;
    // Checked under the lock that settles items, so the last to finish wakes it.
    if !others_unfinished(&store)? {
        triggers::hold_at_once(&mut store, finished)?;
    } else if needs_operator || !is_routine_success(item) {
        triggers::hold(&mut store, finished)?;
    } else {
        let until = archon_core::now() + MAX_SUCCESS_DEFER;
        triggers::hold_deferred(&mut store, finished, until)?;
    }
    drop(store);
    triggers::wake(ctx);
    Ok(())
}

/// A success no one asked to hear of at once: no GitHub thread to answer and
/// no `notify` tag.
fn is_routine_success(item: &QueueItem) -> bool {
    matches!(item.state, QueueState::Succeeded { .. })
        && item.origin.is_none()
        && !item.wants_notice()
}

/// Whether any item is still queued or running; blocked ones wait for the
/// operator, not for this news.
fn others_unfinished(store: &Store) -> Result<bool, ArchonError> {
    let query = ItemQuery {
        states: ["queued", "dispatching", "running"]
            .map(String::from)
            .to_vec(),
        limit: Some(1),
        ..ItemQuery::default()
    };
    let unfinished = store.items(&query)?;
    assert!(unfinished.len() <= 1, "the query is limited to one");
    assert!(
        unfinished.iter().all(|item| !item.state.is_terminal()),
        "a finished item is no other work"
    );
    Ok(!unfinished.is_empty())
}

/// Whether the finished `item` wakes the orchestrator: a failure, a GitHub
/// thread to answer, a `notify` tag, or a success with `wake_on_success`.
fn wakes_orchestrator(item: &QueueItem, wake_on_success: bool) -> bool {
    assert!(item.state.is_terminal(), "only a finished item is news");
    let failed = matches!(item.state, QueueState::Failed { .. });
    let succeeded = matches!(item.state, QueueState::Succeeded { .. });
    failed || (succeeded && wake_on_success) || item.origin.is_some() || item.wants_notice()
}

/// Keep the notification small; the queue, stages and orchestrator retain the
/// full error. Read the failing member instead of the engine's nested report.
fn failure_notice(item: &QueueItem, run: Option<&PipelineRun>, stages: &[StageRun]) -> String {
    let QueueState::Failed { error, .. } = &item.state else {
        return format!(
            "{} did not fail; `archon queue show {}` says where it stands",
            item.title, item.id
        );
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

/// Whether the item failed on a stage that only the operator can unblock. A
/// success never does, whatever words its summary holds.
fn failed_blocked(item: &QueueItem) -> bool {
    matches!(&item.state, QueueState::Failed { error, .. } if error.contains(StageOutcome::BLOCKED_PREFIX))
}

/// Where a succeeded `ship = local` item's change is, what it changed, and
/// the commands to review and land it; `None` for any other item.
async fn local_notice(ctx: &DaemonContext, item: &QueueItem) -> Option<String> {
    if !matches!(item.state, QueueState::Succeeded { .. }) {
        return None;
    }
    let review = match super::operator_merge::local_review(ctx, &item.id, false).await {
        Ok(review) => review,
        // Not a local delivery, or nothing left to land: no extra line.
        Err(error) => {
            tracing::debug!(item = %item.id, %error, "no local review for the done notice");
            return None;
        }
    };
    Some(format!(
        "Branch `{}` in {} holds it ({}). Review: `archon diff {}`; land it in `{}`: `archon merge {}`.",
        review.branch,
        review.repo_path.display(),
        review.diffstat.trim(),
        item.id,
        review.base,
        item.id
    ))
}

pub(super) fn outcome_text(item: &QueueItem) -> String {
    let text = match &item.state {
        QueueState::Succeeded { summary, .. } => format!("Done: {}. {summary}", item.title),
        QueueState::Failed { error, .. } => format!("Failed: {}. {error}", item.title),
        QueueState::Cancelled { reason: Some(r) } => format!("Cancelled: {}. {r}", item.title),
        QueueState::Cancelled { reason: None } => format!("Cancelled: {}.", item.title),
        other => format!("{}: {} is {}.", item.id, item.title, other.kind()),
    };
    text.trim().to_owned()
}

/// `text` for the orchestrator; when clipped, it says where the rest is.
pub(super) fn finished_summary(text: &str, item: &QueueItem) -> String {
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
