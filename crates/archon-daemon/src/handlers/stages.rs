//! One item's stage runs in full: the listing of one of its runs (the
//! latest by default), or a stage's whole summary and structured output,
//! paged (§11).

use std::collections::BTreeSet;

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

/// Set `stage`'s cost to what the ledger charged its agent and every agent beneath
/// it; the split, when it delegated. No agent is beneath two stages' agents.
pub(super) fn charge(
    store: &Store,
    stage: &mut StageRun,
    estimates: bool,
) -> Result<Option<StageDelegation>, ArchonError> {
    let Some(agent) = &stage.aether_agent else {
        return Ok(None);
    };
    let direct_usd = store.spend_for_agent(agent, estimates)?;
    let agents = store.delegated_beneath(agent, estimates)?;
    assert!(
        agents.iter().all(|delegated| delegated.agent != *agent),
        "a stage agent is not beneath itself"
    );
    if agents.is_empty() {
        stage.cost_usd = direct_usd;
        return Ok(None);
    }
    let delegation = StageDelegation::of(direct_usd, agents);
    stage.cost_usd = delegation.total_usd();
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
