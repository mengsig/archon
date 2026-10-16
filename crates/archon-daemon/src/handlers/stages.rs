//! One item's stage runs in full: the listing of one of its runs (the
//! latest by default), or a stage's whole summary and structured output,
//! paged (§11).

use std::collections::BTreeSet;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::protocol::{
    Reply, StageDelegation, StageDetail, StageQuery, StageRunView, TextPage,
};
use archon_core::queue::QueueItem;
use archon_core::run::{PipelineRun, StageRun, StageState};
use archon_queue::Store;

use crate::context::DaemonContext;

pub(super) async fn get(ctx: &DaemonContext, query: &StageQuery) -> Result<Reply, ArchonError> {
    query.check().map_err(ArchonError::Invalid)?;
    let estimates = ctx.config.limits.count_estimated_cost;
    let store = ctx.store.lock().await;
    let item = store.item(&query.item)?;
    let run = chosen_run(&store, &item, query)?;
    assert_eq!(run.item, item.id, "a run belongs to its item");
    let all = store.stage_runs(&run.id)?;
    let picked = match &query.stage {
        None => all,
        Some(stage) => pick(all, stage, &run, query)?,
    };
    // Stage costs come from the spend ledger, as in `GetItem`.
    let mut costed = Vec::with_capacity(picked.len());
    for mut stage in picked {
        let delegation = charge(&store, &mut stage, estimates)?;
        costed.push((stage, delegation));
    }
    drop(store);
    assert!(
        costed.iter().all(|(s, _)| s.run == run.id),
        "stage runs belong to their run"
    );
    let page = query.stage.as_ref().map(|_| query.page_chars());
    let stages = costed
        .into_iter()
        .map(|(stage, delegation)| view(stage, delegation, query.offset, page))
        .collect();
    Ok(Reply::StageDetail(Box::new(StageDetail {
        item: item.id,
        run: run.id,
        stage: query.stage.clone(),
        stages,
    })))
}

/// The run asked for, or the item's latest. A run of another item, or none
/// at all, is `NotFound`, naming the item's runs.
fn chosen_run(
    store: &Store,
    item: &QueueItem,
    query: &StageQuery,
) -> Result<PipelineRun, ArchonError> {
    assert_eq!(query.item, item.id, "the item asked for was loaded");
    let mut runs = store.runs_for_item(&item.id)?;
    let Some(wanted) = &query.run else {
        return runs.pop().ok_or_else(|| {
            ArchonError::NotFound(format!(
                "a pipeline run of item {} (it has not started yet)",
                item.id
            ))
        });
    };
    let Some(at) = runs.iter().position(|r| &r.id == wanted) else {
        let known = joined(runs.iter().map(|r| &r.id));
        return Err(ArchonError::NotFound(format!(
            "run {wanted} of item {} (its runs: {})",
            item.id,
            if known.is_empty() { "none yet" } else { &known }
        )));
    };
    Ok(runs.swap_remove(at))
}

/// The runs of `stage` at the attempt asked for (the latest by default),
/// in one slot or all of them. Nothing matching is `NotFound`, naming what
/// does exist.
fn pick(
    all: Vec<StageRun>,
    stage: &str,
    run: &PipelineRun,
    query: &StageQuery,
) -> Result<Vec<StageRun>, ArchonError> {
    let ran: BTreeSet<&str> = all.iter().map(|s| s.stage_id.as_str()).collect();
    if !ran.contains(stage) {
        let ran = match ran.is_empty() {
            true => "no stage has run yet".to_owned(),
            false => format!("stages that ran: {}", joined(ran)),
        };
        return Err(ArchonError::NotFound(format!(
            "stage {stage:?} in run {} of item {} ({ran})",
            run.id, query.item
        )));
    }
    let of_stage: Vec<StageRun> = all.into_iter().filter(|s| s.stage_id == stage).collect();
    let attempts: BTreeSet<u32> = of_stage.iter().map(|s| s.attempt).collect();
    let latest = *attempts.last().expect("the stage ran at least once");
    let attempt = query.attempt.unwrap_or(latest);
    if !attempts.contains(&attempt) {
        return Err(ArchonError::NotFound(format!(
            "attempt {attempt} of stage {stage:?} (attempts: {})",
            joined(attempts)
        )));
    }
    let of_attempt: Vec<StageRun> = of_stage
        .into_iter()
        .filter(|s| s.attempt == attempt)
        .collect();
    let Some(slot) = query.slot else {
        return Ok(of_attempt);
    };
    let slots: BTreeSet<u32> = of_attempt.iter().map(|s| s.fan_index).collect();
    if !slots.contains(&slot) {
        return Err(ArchonError::NotFound(format!(
            "slot {slot} of stage {stage:?} attempt {attempt} (slots: {})",
            joined(slots)
        )));
    }
    let picked: Vec<StageRun> = of_attempt
        .into_iter()
        .filter(|s| s.fan_index == slot)
        .collect();
    assert!(!picked.is_empty(), "the slot was found above");
    Ok(picked)
}

/// Set `stage`'s cost to what the ledger charged its agent and every agent beneath it
/// during this attempt, from hand-off to hand-off of a kept agent; the split, when it delegated.
pub(super) fn charge(
    store: &Store,
    stage: &mut StageRun,
    estimates: bool,
) -> Result<Option<StageDelegation>, ArchonError> {
    let Some(agent) = &stage.aether_agent else {
        return Ok(None);
    };
    let handoffs = store.agent_handoffs(agent, estimates)?;
    let from = handoffs.iter().find(|h| h.attempt == stage.attempt);
    let until = handoffs.iter().find(|h| h.attempt > stage.attempt);
    let span = |agent: &AetherAgentId, now: f64| {
        let start = from.map_or(0.0, |h| h.spent_by(agent));
        let end = until.map_or(now, |h| h.spent_by(agent));
        // A provider's settled total may land below an interim charge.
        (end - start).max(0.0)
    };
    let direct_usd = span(agent, store.spend_for_agent(agent, estimates)?);
    let mut agents = store.delegated_beneath(agent, estimates)?;
    assert!(
        agents.iter().all(|delegated| delegated.agent != *agent),
        "a stage agent is not beneath itself"
    );
    agents.retain_mut(|delegated| {
        let earlier = from.is_some_and(|h| h.spent.contains_key(&delegated.agent));
        delegated.cost_usd = span(&delegated.agent, delegated.cost_usd);
        delegated.cost_usd > 0.0 || (until.is_none() && !earlier)
    });
    if agents.is_empty() {
        stage.cost_usd = direct_usd;
        return Ok(None);
    }
    let delegation = StageDelegation::of(direct_usd, agents);
    stage.cost_usd = delegation.total_usd();
    debug_assert!(stage.cost_usd.is_finite() && stage.cost_usd >= 0.0);
    Ok(Some(delegation))
}

/// A stage run for the reply; `page` characters of its output when a stage
/// was asked for.
fn view(
    stage: StageRun,
    delegation: Option<StageDelegation>,
    offset: usize,
    page: Option<usize>,
) -> StageRunView {
    let output = page.map(|limit| TextPage::of(&output_text(&stage), offset, limit));
    let choice = stage.current_choice().clone();
    StageRunView {
        stage_id: stage.stage_id,
        attempt: stage.attempt,
        slot: stage.fan_index,
        choice,
        state: stage.state,
        verdict: stage.verdict,
        cost_usd: stage.cost_usd,
        delegation,
        aether_agent: stage.aether_agent,
        output,
    }
}

/// Everything a stage reported: its error, summary and pretty-printed
/// structured result, as one text that pages consistently.
fn output_text(stage: &StageRun) -> String {
    let mut text = String::new();
    if let StageState::Failed { error } = &stage.state {
        text.push_str(&format!("error:\n{error}\n\n"));
    }
    let summary = stage.summary.as_deref().unwrap_or("(none)");
    let structured = match &stage.structured {
        None => "(none)".to_owned(),
        Some(value) => serde_json::to_string_pretty(value).expect("a JSON value always serializes"),
    };
    text.push_str(&format!("summary:\n{summary}\n\nstructured:\n{structured}"));
    text
}

fn joined<T: ToString>(values: impl IntoIterator<Item = T>) -> String {
    let values: Vec<String> = values.into_iter().map(|v| v.to_string()).collect();
    values.join(", ")
}

#[cfg(test)]
mod tests {
    use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
    use archon_core::queue::QueueState;
    use archon_core::run::RunState;
    use archon_queue::store::SpendRow;

    use super::*;

    fn agent(name: &str) -> AetherAgentId {
        AetherAgentId::parse(name).unwrap()
    }

    /// An item with a live run, in a fresh store.
    fn running_item() -> (Store, QueueItem, PipelineRun) {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let item = QueueItem {
            id: QueueItemId::new(),
            repo: None,
            pipeline: "ask".into(),
            title: "repaired".into(),
            brief: "brief".into(),
            priority: Default::default(),
            dependencies: vec![],
            tags: vec![],
            model_plan: Default::default(),
            model_reasons: None,
            budget_usd: None,
            autonomy: Default::default(),
            origin: None,
            stage_timeouts: Default::default(),
            fresh_retries: 0,
            routed_provider: None,
            initiative: None,
            state: QueueState::Queued,
            created_at: now,
            updated_at: now,
        };
        let item = store.insert_item(&item, None).unwrap();
        store
            .transition_item(&item.id, QueueState::Dispatching, now)
            .unwrap();
        let run = PipelineRun {
            id: PipelineRunId::new(),
            item: item.id.clone(),
            pipeline: item.pipeline.clone(),
            shipped: false,
            aether_run: None,
            workspace: "/wt/x".into(),
            team_root: None,
            branch: "archon/x".into(),
            state: RunState::Running {
                stage_id: "implement".into(),
                attempt: 1,
            },
            cost_usd: 0.0,
            started_at: now,
            finished_at: None,
        };
        store.insert_run(&run).unwrap();
        let running = QueueState::Running {
            run: run.id.clone(),
        };
        let item = store.transition_item(&item.id, running, now).unwrap();
        (store, item, run)
    }

    fn attempt(run: &PipelineRun, attempt: u32, agent: &AetherAgentId) -> StageRun {
        StageRun {
            id: StageRunId::new(),
            run: run.id.clone(),
            stage_id: "implement".into(),
            kind: None,
            attempt,
            fan_index: 0,
            choice: archon_core::model::ModelChoice {
                model: "claude-code/opus".parse().unwrap(),
                effort: archon_core::model::Effort::High,
                tier: None,
            },
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            aether_agent: Some(agent.clone()),
            aether_task: None,
            state: StageState::Running,
            verdict: None,
            summary: None,
            structured: None,
            cost_usd: 0.0,
            started_at: archon_core::now(),
            finished_at: None,
        }
    }

    fn spent(store: &mut Store, item: &QueueItem, agent: &AetherAgentId, total: f64) {
        store
            .record_spend(&SpendRow {
                agent: agent.clone(),
                item: Some(item.id.clone()),
                cost_usd: total,
                cost_basis: archon_core::cost::CostBasis::ProviderReported,
                cost_complete: None,
                estimated_cost_usd: 0.0,
                input_tokens: 1,
                output_tokens: 1,
                at: archon_core::now(),
            })
            .unwrap();
    }

    /// A kept agent resumed for a repair attempt: each attempt is charged what
    /// was spent during it, so the attempts add up to what the agents cost.
    #[test]
    fn attempts_sharing_a_kept_agent_are_charged_once_between_them() {
        let (mut store, item, run) = running_item();
        let (lead, early, late) = (agent("agt_lead"), agent("agt_early"), agent("agt_late"));
        let model = "claude-code/opus".parse().unwrap();
        let mut first = attempt(&run, 1, &lead);
        first.aether_task = Some("tsk_first".to_owned().try_into().unwrap());
        store.upsert_stage_run(&first).unwrap();
        store.record_delegated(&early, &lead, &model, None).unwrap();
        spent(&mut store, &item, &lead, 1.0);
        spent(&mut store, &item, &early, 0.5);
        first.state = StageState::Failed {
            error: "review failed".into(),
        };
        first.finished_at = Some(archon_core::now());
        store.upsert_stage_run(&first).unwrap();

        let mut second = attempt(&run, 2, &lead);
        store.upsert_stage_run(&second).unwrap();
        store.record_delegated(&late, &lead, &model, None).unwrap();
        spent(&mut store, &item, &lead, 3.0);
        spent(&mut store, &item, &early, 0.75);
        spent(&mut store, &item, &late, 0.25);

        let split = charge(&store, &mut first, false).unwrap().unwrap();
        assert_eq!((first.cost_usd, split.direct_usd), (1.5, 1.0));
        let named: Vec<_> = split
            .agents
            .iter()
            .map(|a| (&a.agent, a.cost_usd))
            .collect();
        assert_eq!(named, [(&early, 0.5)], "the late agent is the repair's");
        let split = charge(&store, &mut second, false).unwrap().unwrap();
        assert_eq!((second.cost_usd, split.direct_usd), (2.5, 2.0));
        let named: Vec<_> = split
            .agents
            .iter()
            .map(|a| (&a.agent, a.cost_usd))
            .collect();
        assert_eq!(named, [(&early, 0.25), (&late, 0.25)]);
        let item_total = store.spend_for_item(&item.id, false).unwrap();
        assert_eq!(item_total, 4.0);
        assert_eq!(first.cost_usd + second.cost_usd, item_total);
    }
}
