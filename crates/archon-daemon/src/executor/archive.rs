//! Clearing stage agents the pipeline moved past out of Aether's live view
//! (`archive`), so it shows only what needs the operator. An attempt that
//! failed, was cancelled or timed out is archived with every agent it
//! delegated once a fresh agent took its place in a live run, or once its
//! run finished. An `ESCALATE:` or `BLOCKED:` failure that ended the run
//! stays until its item moves on: retried, steered, cancelled or finished.
//!
//! A `kv` mark per stage run, `archive:<stage run id>`, makes this durable:
//! `owed` until Aether confirmed the archive, then `done` while the run is
//! live, so each tree is archived once. An owed mark is written in the same
//! transaction as the spawn or node retirement that owes it, so a restart
//! never loses one, and retention keeps its item while it stands.
//!
//! Dispatch, the sweep and the event pump only write marks. One worker
//! ([`run`]) asks Aether: at start, when a mark is written and every
//! [`RETRY_EVERY`]. A pass reads the marks from the store and may take
//! [`PASS_BUDGET`] in all and [`CALL_LIMIT`] a call, so a slow or silent
//! Aether holds up nothing else; what a pass did not reach stays owed.
//! So does a tree Aether refuses to archive, asked of again only every
//! [`REFUSED_RETRY`] and after a restart. Archiving is best-effort and
//! never fails a run.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::ports::DescendantState;
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_queue::Store;
use tokio::time::Instant;

use super::run_node;
use super::sweep::is_passing;
use crate::context::DaemonContext;

const MARK_PREFIX: &str = "archive:";
const OWED: &str = "owed";
const DONE: &str = "done";

/// How long one `archive` or `descendants` call may take before it counts
/// as unanswered and its mark stays owed.
pub const CALL_LIMIT: Duration = Duration::from_secs(5);
/// How long one pass may spend asking Aether; what it did not reach stays owed.
pub const PASS_BUDGET: Duration = Duration::from_secs(10);
/// How long the worker waits for a new mark before it asks of the owed ones again.
pub const RETRY_EVERY: Duration = Duration::from_secs(15);
/// How long a tree Aether refused to archive waits before it is asked of again.
pub const REFUSED_RETRY: Duration = Duration::from_secs(3600);
// A pass outlasts one unanswered call, so one silent agent never takes it whole.
const _: () = assert!(CALL_LIMIT.as_secs() < PASS_BUDGET.as_secs());

/// The delegates Aether confirmed archived, by the stage run whose tree they are of.
type Delegates = BTreeMap<StageRunId, BTreeSet<AetherAgentId>>;

/// What the worker carries from one pass to the next. None of it is durable:
/// after a restart Aether is asked again, and archiving twice does no harm.
#[derive(Debug, Default)]
pub struct Progress {
    /// The mark a pass that ran out of time had reached; the next starts there.
    resume: Option<StageRunId>,
    /// What is archived already of each tree whose mark is still owed.
    archived: Delegates,
    /// When each tree Aether refused to archive is asked of again.
    refused: BTreeMap<StageRunId, Instant>,
}

/// Wake the worker for the `owed` marks just written; none wakes nobody.
pub(super) fn announce(ctx: &DaemonContext, owed: &[StageRunId]) {
    if !owed.is_empty() {
        ctx.coordination.archive_owed.notify_one();
    }
}

/// Archive what the marks owe at start, whenever one is written and every
/// [`RETRY_EVERY`], until the daemon stops; only a store failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    run_every(ctx, RETRY_EVERY).await
}

/// [`run`] with the wait between passes no mark announced, for the
/// deterministic harness. Dropped at any point, it leaves every mark it did
/// not settle owed.
pub async fn run_every(ctx: Arc<DaemonContext>, retry_every: Duration) -> Result<(), ArchonError> {
    assert!(!retry_every.is_zero(), "a worker that never waits spins");
    let mut progress = Progress::default();
    loop {
        if ctx.aether.is_connected() {
            pass(&ctx, &mut progress).await?;
        }
        tokio::select! {
            () = ctx.coordination.archive_owed.notified() => {}
            () = tokio::time::sleep(retry_every) => {}
        }
    }
}

/// One pass of the worker: ask Aether of every tree a mark owes, oldest
/// stage run first from where the last pass ran out of time, until
/// [`PASS_BUDGET`] is spent. Only a store failure is returned.
pub async fn pass(ctx: &DaemonContext, progress: &mut Progress) -> Result<(), ArchonError> {
    let mut stages = marked(&mut *ctx.store.lock().await)?;
    stages.sort();
    let marked = |stage: &StageRunId| stages.binary_search(stage).is_ok();
    progress.archived.retain(|stage, _| marked(stage));
    progress.refused.retain(|stage, _| marked(stage));
    let resume = progress.resume.take();
    let start = resume.map_or(0, |from| stages.partition_point(|stage| *stage < from));
    let (earlier, rest) = stages.split_at(start);
    let deadline = Instant::now() + PASS_BUDGET;
    for stage in rest.iter().chain(earlier) {
        if Instant::now() >= deadline {
            tracing::info!(
                marks = stages.len(),
                "archive pass out of time; the marks not reached wait for the next"
            );
            progress.resume = Some(stage.clone());
            break;
        }
        match release(ctx, stage, deadline, progress).await {
            Ok(()) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::error!(%stage, %error, "archiving a moved-past stage agent failed; its mark is retried");
            }
        }
    }
    assert!(
        progress.archived.keys().all(marked) && progress.refused.keys().all(marked),
        "progress is kept only of marked trees"
    );
    assert!(
        progress.resume.as_ref().is_none_or(marked),
        "the next pass resumes at a mark this one read"
    );
    Ok(())
}

fn mark_key(stage: &StageRunId) -> String {
    format!("{MARK_PREFIX}{stage}")
}

/// Every stage run with an archive mark: what a pass looks at. A mark
/// whose key or value does not parse is logged and dropped, never fatal.
fn marked(store: &mut Store) -> Result<Vec<StageRunId>, ArchonError> {
    let mut stages = Vec::new();
    for (key, value) in store.kv_prefix(MARK_PREFIX)? {
        let raw = key.strip_prefix(MARK_PREFIX).expect("ranged by prefix");
        let parsed = StageRunId::parse(raw)
            .map_err(|e| e.to_string())
            .and_then(|id| {
                if value == OWED || value == DONE {
                    Ok(id)
                } else {
                    Err(format!("value {value:?} is neither {OWED} nor {DONE}"))
                }
            });
        match parsed {
            Ok(id) => stages.push(id),
            Err(error) => {
                tracing::warn!(%key, %error, "corrupt archive mark dropped");
                store.kv_delete(&key)?;
            }
        }
    }
    assert!(
        stages.len() == stages.iter().collect::<BTreeSet<_>>().len(),
        "one mark per stage run"
    );
    Ok(stages)
}

/// The stage run each agent last served, for agents whose last attempt
/// ended without completing: an agent woken or resumed by a later attempt
/// belongs to that attempt.
fn ended_agents(stages: &[StageRun]) -> Vec<(&StageRun, &AetherAgentId)> {
    let mut last: BTreeMap<&AetherAgentId, &StageRun> = BTreeMap::new();
    for stage in stages {
        let Some(agent) = &stage.aether_agent else {
            continue;
        };
        let later =
            |seen: &StageRun| (stage.started_at, stage.attempt) > (seen.started_at, seen.attempt);
        if last.get(agent).is_none_or(|seen| later(seen)) {
            last.insert(agent, stage);
        }
    }
    let ended: Vec<_> = last
        .into_iter()
        .filter(|(_, stage)| stage.state.is_terminal() && stage.state != StageState::Completed)
        .map(|(agent, stage)| (stage, agent))
        .collect();
    assert!(
        ended
            .iter()
            .all(|(s, a)| s.aether_agent.as_ref() == Some(*a)),
        "each ended agent is its stage run's own"
    );
    ended
}

/// The ended agents of a live run that no attempt of their slot can still
/// wake: a replacement not yet bound to an agent may take its slot's failed
/// one back (ARCHITECTURE §6.3); one bound to a fresh agent moved past it.
fn superseded_in(stages: &[StageRun]) -> Vec<StageRunId> {
    let live_slots: BTreeSet<(&str, u32)> = stages
        .iter()
        .filter(|s| !s.state.is_terminal() && s.aether_agent.is_none())
        .map(|s| (s.stage_id.as_str(), s.fan_index))
        .collect();
    ended_agents(stages)
        .into_iter()
        .filter(|(stage, _)| !live_slots.contains(&(stage.stage_id.as_str(), stage.fan_index)))
        .map(|(stage, _)| stage.id.clone())
        .collect()
}

/// Save `spawned`, just bound to a fresh agent in a live run, with an owed
/// mark for each ended attempt of its run it moved past, in one transaction.
/// Called under the run table, so no settle is half applied.
pub(super) fn save_spawned(
    store: &mut Store,
    spawned: &StageRun,
) -> Result<Vec<StageRunId>, ArchonError> {
    assert!(
        spawned.state == StageState::Running && spawned.aether_agent.is_some(),
        "only a stage bound to its agent moves past others"
    );
    let mut stages = store.stage_runs(&spawned.run)?;
    assert!(
        stages.iter().all(|s| s.run == spawned.run),
        "the run's own stages"
    );
    let saved = stages.iter_mut().find(|s| s.id == spawned.id);
    *saved.expect("the spawned stage is its run's") = spawned.clone();
    let mut owed = Vec::new();
    for stage in superseded_in(&stages) {
        if store.kv_get(&mark_key(&stage))?.is_none() {
            owed.push(stage);
        }
    }
    let keys: Vec<String> = owed.iter().map(mark_key).collect();
    let marks: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), OWED)).collect();
    store.upsert_stage_run_with_kv(spawned, &marks)?;
    Ok(owed)
}

/// The finished `run`'s node is retiring, every agent of it stopped: owe an
/// archive of each ended attempt not archived yet, drop the marks of those
/// that were, and forget the node at `node_key`, all in one transaction.
/// Called under the store lock.
pub(super) fn owe_finished(
    store: &mut Store,
    run: &PipelineRunId,
    node_key: &str,
) -> Result<Vec<StageRunId>, ArchonError> {
    assert!(
        store.run(run)?.state.is_terminal(),
        "only a finished run's attempts are all moved past"
    );
    let stages = store.stage_runs(run)?;
    assert!(
        stages.iter().all(|s| s.state.is_terminal()),
        "no stage is left live"
    );
    let (mut owed, mut done) = (Vec::new(), Vec::new());
    for (stage, _) in ended_agents(&stages) {
        let key = mark_key(&stage.id);
        if store.kv_get(&key)?.as_deref() == Some(DONE) {
            done.push(key);
        } else {
            owed.push(stage.id.clone());
        }
    }
    let keys: Vec<String> = owed.iter().map(mark_key).collect();
    let marks: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), OWED)).collect();
    let mut forget: Vec<&str> = done.iter().map(String::as_str).collect();
    forget.push(node_key);
    store.kv_update(&marks, &forget)?;
    Ok(owed)
}

/// Whether `stage`'s failure still waits for the operator: it escalated or
/// blocked, ending its run, and the item still stands failed by that run.
fn needs_operator(stage: &StageRun, item: Option<&QueueState>) -> bool {
    let StageState::Failed { error } = &stage.state else {
        return false;
    };
    let failed_by_run =
        matches!(item, Some(QueueState::Failed { run: Some(run), .. }) if *run == stage.run);
    failed_by_run && StageOutcome::ends_run(error)
}

/// The agent whose tree `stage`'s mark owes an archive now; `None` when it
/// owes none yet. Drops marks with nothing left to do.
fn due(store: &mut Store, stage: &StageRunId) -> Result<Option<AetherAgentId>, ArchonError> {
    let key = mark_key(stage);
    let Some(mark) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let found = match store.stage_run(stage) {
        Ok(found) => found,
        Err(ArchonError::NotFound(error)) => {
            // Retention keeps a marked stage run; one gone anyway leaves no agent to name.
            tracing::warn!(%stage, %error, "archive mark of a stage run no longer stored dropped");
            store.kv_delete(&key)?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let run = store.run(&found.run)?;
    if mark == DONE {
        // The node's retirement drops it; one that raced the retirement ends here.
        if run.state.is_terminal() && !run_node::has_node(store, &run.id)? {
            store.kv_delete(&key)?;
        }
        return Ok(None);
    }
    assert_eq!(mark, OWED, "an archive mark is owed or done");
    let item = match store.item(&run.item) {
        Ok(item) => Some(item.state),
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    if needs_operator(&found, item.as_ref()) {
        return Ok(None);
    }
    let agent = found.aether_agent;
    Ok(Some(
        agent.expect("only a stage run with an agent is marked"),
    ))
}

/// Archive `stage`'s tree if its mark owes it now, and record that it is
/// settled: a finished run's mark is dropped, a live one's kept as done.
/// A tree not archived whole keeps its mark owed and, in `progress`, the
/// delegates it need not ask of again; one Aether refused waits there for
/// [`REFUSED_RETRY`] first.
async fn release(
    ctx: &DaemonContext,
    stage: &StageRunId,
    deadline: Instant,
    progress: &mut Progress,
) -> Result<(), ArchonError> {
    let Some(agent) = due(&mut *ctx.store.lock().await, stage)? else {
        return Ok(());
    };
    let now = Instant::now();
    if progress
        .refused
        .get(stage)
        .is_some_and(|again| now < *again)
    {
        return Ok(());
    }
    let delegates = progress.archived.entry(stage.clone()).or_default();
    match archive_tree(ctx, stage, &agent, deadline, delegates).await {
        Archived::Done => {}
        Archived::Later => return Ok(()),
        Archived::Refused => {
            let again = Instant::now() + REFUSED_RETRY;
            progress.refused.insert(stage.clone(), again);
            return Ok(());
        }
    }
    progress.archived.remove(stage);
    progress.refused.remove(stage);
    settle(&mut *ctx.store.lock().await, stage)
}

/// `stage`'s tree is archived: drop its mark once its run finished, and
/// keep it as done while the run is live, so the tree is archived once.
fn settle(store: &mut Store, stage: &StageRunId) -> Result<(), ArchonError> {
    let key = mark_key(stage);
    let Some(mark) = store.kv_get(&key)? else {
        return Ok(());
    };
    assert!(
        mark == OWED || mark == DONE,
        "an archive mark is owed or done"
    );
    let run = store.stage_run(stage)?.run;
    if store.run(&run)?.state.is_terminal() {
        store.kv_delete(&key)?;
    } else {
        store.kv_set(&key, DONE)?;
    }
    assert_ne!(
        store.kv_get(&key)?.as_deref(),
        Some(OWED),
        "an archived tree owes nothing"
    );
    Ok(())
}

/// What became of an archive of one agent or a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archived {
    /// Archived, or Aether no longer knows it: nothing is left to show.
    Done,
    /// Not yet: Aether did not answer or holds it live; the mark stays owed.
    Later,
    /// Refused by Aether: it stays in view, its mark owed, to be asked of
    /// again much later.
    Refused,
}

/// `call` to Aether, counted unanswered once it takes over [`CALL_LIMIT`]
/// or runs past the pass's `deadline`, whichever comes first.
async fn bounded<T>(
    call: impl Future<Output = Result<T, ArchonError>>,
    what: &str,
    deadline: Instant,
) -> Result<T, ArchonError> {
    let limit = CALL_LIMIT.min(deadline.saturating_duration_since(Instant::now()));
    match tokio::time::timeout(limit, call).await {
        Ok(answer) => answer,
        Err(_) => Err(ArchonError::Unavailable(format!(
            "aether did not answer {what} within {:.1}s",
            limit.as_secs_f32()
        ))),
    }
}

/// Archive every agent beneath `agent`, then `agent` itself, only once the
/// whole tree is confirmed stopped and every delegate archived or gone. A
/// tree it cannot read, for any reason, or with an agent still live, is
/// left for later; a stage agent Aether no longer knows lists no delegates,
/// and its own `archive` then finds it gone. Delegates in `archived` were
/// archived by an earlier pass and are not asked of again; each one this
/// pass archives joins them, so a tree too slow for one pass ends in a later.
async fn archive_tree(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    deadline: Instant,
    archived: &mut BTreeSet<AetherAgentId>,
) -> Archived {
    let listing = bounded(ctx.backend.descendants(agent), "descendants", deadline);
    let descendants = match listing.await {
        Ok(descendants) => descendants,
        Err(error) if is_passing(&error) => {
            tracing::info!(%stage, %agent, %error, "stage agent tree unanswered; archive retried later");
            return Archived::Later;
        }
        Err(error) => {
            tracing::warn!(%stage, %agent, %error, "stage agent tree unreadable; archive retried later");
            return Archived::Later;
        }
    };
    if let Some(live) = descendants
        .iter()
        .find(|d| d.state == DescendantState::Live)
    {
        tracing::info!(%stage, %agent, live = %live.agent, "an agent beneath the stage agent still runs; archive retried later");
        return Archived::Later;
    }
    assert!(
        descendants.iter().all(|d| d.agent != *agent),
        "an agent is not its own descendant"
    );
    assert!(
        !archived.contains(agent),
        "the stage agent goes last, so no pass remembers it"
    );
    let mut refused = false;
    for member in &descendants {
        if archived.contains(&member.agent) {
            continue;
        }
        match archive_one(ctx, stage, &member.agent, deadline).await {
            Archived::Done => {
                archived.insert(member.agent.clone());
            }
            // The rest would likely wait as long; the next pass asks again.
            Archived::Later => return Archived::Later,
            Archived::Refused => refused = true,
        }
    }
    if refused {
        tracing::warn!(%stage, %agent, "Aether refused to archive a delegate of the stage agent; the tree stays in its view, owed");
        return Archived::Refused;
    }
    archive_one(ctx, stage, agent, deadline).await
}

/// One `archive` call; none once the pass is out of time.
async fn archive_one(
    ctx: &DaemonContext,
    stage: &StageRunId,
    agent: &AetherAgentId,
    deadline: Instant,
) -> Archived {
    if Instant::now() >= deadline {
        tracing::info!(%stage, %agent, "archive pass out of time; this agent waits for the next");
        return Archived::Later;
    }
    match bounded(ctx.backend.archive(agent), "archive", deadline).await {
        Ok(()) => Archived::Done,
        // Retention may have removed it: nothing is left to show.
        Err(ArchonError::NotFound(_)) => Archived::Done,
        Err(error @ (ArchonError::Invalid(_) | ArchonError::InvalidTransition(_))) => {
            tracing::info!(%stage, %agent, %error, "Aether refused to archive a stage agent it holds live; retried later");
            Archived::Later
        }
        Err(error) if is_passing(&error) => {
            tracing::warn!(%stage, %agent, %error, "archiving a stage agent went unanswered or failed inside Aether; retried later");
            Archived::Later
        }
        Err(error) => {
            tracing::warn!(%stage, %agent, %error, "Aether refused to archive a stage agent; it stays in its view, owed");
            Archived::Refused
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::model::{Effort, ModelChoice};

    fn stage(id: &str, attempt: u32, agent: Option<&str>, state: StageState) -> StageRun {
        let started_at = archon_core::now() + chrono::Duration::seconds(i64::from(attempt));
        StageRun {
            id: StageRunId::new(),
            run: PipelineRunId::new(),
            stage_id: id.into(),
            kind: None,
            attempt,
            fan_index: 0,
            choice: ModelChoice::pinned("claude-code/opus".parse().unwrap(), Effort::High),
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            aether_agent: agent.map(|a| AetherAgentId::parse(a).unwrap()),
            aether_task: None,
            finished_at: state.is_terminal().then_some(started_at),
            state,
            verdict: None,
            summary: None,
            structured: None,
            cost_usd: 0.0,
            started_at,
        }
    }

    fn failed() -> StageState {
        StageState::Failed {
            error: "tests failed".into(),
        }
    }

    #[test]
    fn a_failed_attempt_is_superseded_once_its_slot_has_no_live_attempt() {
        let first = stage("build", 1, Some("agt_1"), failed());
        let retry = stage("build", 2, None, StageState::Spawning);
        let other = stage("review", 1, Some("agt_3"), StageState::Running);
        let stages = vec![first.clone(), retry.clone(), other.clone()];
        assert!(
            superseded_in(&stages).is_empty(),
            "the replacement may wake it"
        );
        let mut running = retry.clone();
        running.aether_agent = Some(AetherAgentId::parse("agt_2").unwrap());
        running.state = StageState::Running;
        let stages = vec![first.clone(), running.clone(), other.clone()];
        assert_eq!(
            superseded_in(&stages),
            std::slice::from_ref(&first.id),
            "a fresh agent took over"
        );
        running.state = failed();
        let stages = vec![first.clone(), running, other];
        let ids = superseded_in(&stages);
        assert_eq!(ids.len(), 2, "both ended attempts: {ids:?}");
        assert!(ids.contains(&first.id));
    }

    #[test]
    fn an_agent_woken_by_a_later_attempt_belongs_to_that_attempt() {
        let first = stage("build", 1, Some("agt_1"), failed());
        let woken = stage("build", 2, Some("agt_1"), StageState::Completed);
        let stages = vec![first.clone(), woken];
        assert!(
            ended_agents(&stages).is_empty(),
            "its last attempt completed"
        );
        let later_failed = stage("build", 2, Some("agt_1"), failed());
        let stages = vec![first, later_failed.clone()];
        let ended = ended_agents(&stages);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].0.id, later_failed.id);
    }

    #[test]
    fn only_an_escalation_its_item_still_fails_on_waits_for_the_operator() {
        let mut escalated = stage("build", 1, Some("agt_1"), failed());
        escalated.state = StageState::Failed {
            error: "ESCALATE: needs a migration".into(),
        };
        let failed_by = |run: &PipelineRunId| QueueState::Failed {
            run: Some(run.clone()),
            error: "stage build: ESCALATE: needs a migration".into(),
        };
        assert!(needs_operator(&escalated, Some(&failed_by(&escalated.run))));
        let other_run = PipelineRunId::new();
        assert!(!needs_operator(&escalated, Some(&failed_by(&other_run))));
        assert!(!needs_operator(&escalated, Some(&QueueState::Queued)));
        assert!(
            !needs_operator(&escalated, None),
            "a deleted item needs no one"
        );
        let routine = stage("build", 1, Some("agt_2"), failed());
        assert!(!needs_operator(&routine, Some(&failed_by(&routine.run))));
    }
}
