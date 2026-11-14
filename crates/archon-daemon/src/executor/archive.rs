//! Clearing stage agents the pipeline moved past out of Aether's live view
//! (`archive`), so it shows only what needs the operator. An attempt that
//! failed, was cancelled or timed out is archived with every agent it
//! delegated once a fresh agent took its place in a live run, or once its
//! run finished. An `ESCALATE:` or `BLOCKED:` failure that ended the run
//! stays until its item moves on: retried, steered, cancelled or finished.
//!
//! A `kv` mark per stage run, `archive:<stage run id>`, makes this durable:
//! `owed` until Aether confirmed the archive, then `done` while the run is
//! live, so each tree is archived once. The sweep retries owed marks after
//! a restart or an unanswered call; archiving is best-effort and never
//! fails a run.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::ports::DescendantState;
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageRun, StageState};
use archon_queue::Store;

use super::{FollowUp, run_node};
use crate::context::DaemonContext;

const MARK_PREFIX: &str = "archive:";
const OWED: &str = "owed";
const DONE: &str = "done";

fn mark_key(stage: &StageRunId) -> String {
    format!("{MARK_PREFIX}{stage}")
}

/// Every stage run with an archive mark: what the sweep looks at. A mark
/// whose key or value does not parse is logged and dropped, never fatal.
pub(super) fn marked(store: &mut Store) -> Result<Vec<StageRunId>, ArchonError> {
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

/// A fresh agent took a stage of live `run`: owe an archive of each ended
/// attempt it moved past. The run table is held while the stages are read,
/// so no settle is half applied.
pub(super) async fn superseded(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    let _table = ctx.runs.lock().await;
    let mut store = ctx.store.lock().await;
    if store.run(run)?.state.is_terminal() {
        // Its node's retirement owes the archives once every agent stopped.
        return Ok(FollowUp::default());
    }
    let stages = store.stage_runs(run)?;
    assert!(stages.iter().all(|s| s.run == *run), "the run's own stages");
    let mut owed = Vec::new();
    for stage in superseded_in(&stages) {
        let key = mark_key(&stage);
        if store.kv_get(&key)?.is_none() {
            store.kv_set(&key, OWED)?;
            owed.push(stage);
        }
    }
    Ok(FollowUp {
        archive: owed,
        ..FollowUp::default()
    })
}

/// The finished `run`'s node is retiring, every agent of it stopped: owe an
/// archive of each ended attempt not archived yet, and drop the marks of
/// those that were. Called under the store lock that forgets the node.
pub(super) fn owe_finished(
    store: &mut Store,
    run: &PipelineRunId,
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
    let mut owed = Vec::new();
    for (stage, _) in ended_agents(&stages) {
        let key = mark_key(&stage.id);
        if store.kv_get(&key)?.as_deref() == Some(DONE) {
            store.kv_delete(&key)?;
            continue;
        }
        store.kv_set(&key, OWED)?;
        owed.push(stage.id.clone());
    }
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
        Err(ArchonError::NotFound(_)) => {
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

/// Archive `stage`'s tree if its mark owes it now, and record that it did:
/// a finished run's mark is dropped, a live one's kept as done.
pub(super) async fn release(
    ctx: &DaemonContext,
    stage: &StageRunId,
) -> Result<FollowUp, ArchonError> {
    let Some(agent) = due(&mut *ctx.store.lock().await, stage)? else {
        return Ok(FollowUp::default());
    };
    if !archive_tree(ctx, stage, &agent).await {
        return Ok(FollowUp::default());
    }
    let mut store = ctx.store.lock().await;
    let key = mark_key(stage);
    if store.kv_get(&key)?.is_none() {
        return Ok(FollowUp::default());
    }
    let run = store.stage_run(stage)?.run;
    if store.run(&run)?.state.is_terminal() {
        store.kv_delete(&key)?;
    } else {
        store.kv_set(&key, DONE)?;
    }
    Ok(FollowUp::default())
}

/// Whether Aether gave no answer, so the same call is worth repeating.
fn unanswered(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Unavailable(_) | ArchonError::Protocol(_) | ArchonError::Io { .. }
    )
}

/// Archive `agent` and every ended agent beneath it; `false` when a later
/// sweep should try again: Aether did not answer, or some agent still runs.
/// A refusal is logged and given up on, as the archive is only cleanup.
async fn archive_tree(ctx: &DaemonContext, stage: &StageRunId, agent: &AetherAgentId) -> bool {
    let descendants = match ctx.backend.descendants(agent).await {
        Ok(descendants) => descendants,
        Err(error) if unanswered(&error) => {
            tracing::info!(%stage, %agent, %error, "stage agent tree unreadable; archive retried later");
            return false;
        }
        Err(error) => {
            tracing::warn!(%stage, %agent, %error, "stage agent tree unreadable; archiving the agent alone");
            Vec::new()
        }
    };
    if let Some(live) = descendants
        .iter()
        .find(|d| d.state == DescendantState::Live)
    {
        tracing::info!(%stage, %agent, live = %live.agent, "an agent beneath the stage agent still runs; archive retried later");
        return false;
    }
    let mut complete = true;
    let tree = descendants.iter().map(|d| &d.agent).chain([agent]);
    for member in tree {
        complete &= archive_one(ctx, stage, member).await;
    }
    complete
}

/// One `archive` call; `false` when it is worth repeating.
async fn archive_one(ctx: &DaemonContext, stage: &StageRunId, agent: &AetherAgentId) -> bool {
    match ctx.backend.archive(agent).await {
        Ok(()) => true,
        // Retention may have removed it: nothing is left to show.
        Err(ArchonError::NotFound(_)) => true,
        Err(error @ (ArchonError::Invalid(_) | ArchonError::InvalidTransition(_))) => {
            tracing::info!(%stage, %agent, %error, "Aether refused to archive a stage agent it holds live; retried later");
            false
        }
        Err(error) if unanswered(&error) => {
            tracing::warn!(%stage, %agent, %error, "archiving a stage agent went unanswered; retried later");
            false
        }
        Err(error) => {
            tracing::warn!(%stage, %agent, %error, "archiving a stage agent failed; it stays in Aether's view");
            true
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
