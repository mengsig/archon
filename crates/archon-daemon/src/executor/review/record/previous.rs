use std::collections::BTreeMap;

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::run::{PipelineRun, StageRun};
use archon_queue::Store;
use serde_json::Value;

use super::{Finding, Rounds};
use crate::executor::review::handoff;

pub(in crate::executor::review) fn prior_findings(
    store: &Store,
    stage: &StageRun,
) -> Result<Vec<Finding>, ArchonError> {
    Ok(previous(store, stage)?.map_or_else(Vec::new, |p| p.findings))
}

pub(super) struct Prior {
    stage: StageRunId,
    head: Option<String>,
    pub(super) findings: Vec<Finding>,
    record: Value,
}

pub(super) fn previous(store: &Store, stage: &StageRun) -> Result<Option<Prior>, ArchonError> {
    let run = store.run(&stage.run)?;
    let current = store.stage_runs(&run.id)?;
    if let Some(prior) = findings_before(&current, stage, true)? {
        return Ok(Some(prior));
    }
    let runs = store.runs_for_item(&run.item)?;
    let earlier = runs
        .iter()
        .take_while(|r| r.id != run.id)
        .collect::<Vec<_>>();
    let same_branch = |r: &&PipelineRun| r.branch == run.branch && r.workspace == run.workspace;
    for earlier in earlier.into_iter().rev().filter(same_branch) {
        let stages = store.stage_runs(&earlier.id)?;
        if let Some(prior) = findings_before(&stages, stage, false)? {
            return Ok(Some(prior));
        }
    }
    Ok(None)
}

fn findings_before(
    stages: &[StageRun],
    stage: &StageRun,
    same_run: bool,
) -> Result<Option<Prior>, ArchonError> {
    let earlier = stages.iter().rev().filter(|candidate| {
        candidate.stage_id == stage.stage_id
            && candidate.fan_index == stage.fan_index
            && (!same_run || (candidate.started_at, &candidate.id) < (stage.started_at, &stage.id))
            && candidate.state.is_terminal()
    });
    for candidate in earlier {
        assert_ne!(stage.id, candidate.id);
        assert_eq!(stage.run == candidate.run, same_run);
        if let Some(prior) = read_prior(candidate)? {
            return Ok(Some(prior));
        }
    }
    Ok(None)
}

fn read_prior(stage: &StageRun) -> Result<Option<Prior>, ArchonError> {
    let Some(record) = stage
        .structured
        .as_ref()
        .and_then(|s| s.get("review_rounds"))
        .filter(|r| r.get("findings").is_some())
    else {
        return Ok(None);
    };
    assert!(stage.state.is_terminal());
    let found: BTreeMap<String, Finding> = serde_json::from_value(record["findings"].clone())?;
    let head = record
        .get("rounds")
        .and_then(Value::as_array)
        .and_then(|rounds| rounds.last())
        .and_then(|r| r.get("head"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Some(Prior {
        stage: stage.id.clone(),
        head,
        findings: found.into_values().collect(),
        record: record.clone(),
    }))
}

pub(in crate::executor) struct PriorHandoff {
    pub text: String,
    pub file: Option<handoff::FindingsFile>,
}

pub(in crate::executor) fn prior_handoff(
    store: &Store,
    stage: &StageRun,
    delegates_review: bool,
    replacing: Option<&StageRun>,
) -> Result<Option<PriorHandoff>, ArchonError> {
    if stage.only_reviews == Some(true)
        || (!delegates_review && stage.kind != Some(StageKind::Review))
    {
        return Ok(None);
    }
    let prior = match replacing.map(read_prior).transpose()?.flatten() {
        Some(prior) => Some(prior),
        None => previous(store, stage)?,
    };
    let Some(prior) = prior else {
        return Ok(None);
    };
    assert_ne!(stage.id, prior.stage);
    let findings: Vec<_> = prior
        .findings
        .iter()
        .filter(|finding| finding.resolved_in.is_none())
        .map(|finding| (finding.id.as_str(), finding.description.as_str()))
        .collect();
    if findings.is_empty() {
        return Ok(None);
    }
    let mut objective = "\n\n## Open findings from the previous review\n\n".to_owned();
    if let Some(head) = prior.head {
        objective.push_str(&format!(
            "The last review read `{head}`. Review the new delta if that head is still an ancestor.\n"
        ));
    }
    let rule = "Return confirmed fixes as IDs in `resolved_findings`. Keep the `id` on each finding still open. A reviewer fails only on a defect in the commits it reads, or an open finding left unfixed: a new finding in code a pass already read is `\"blocking\": false`.";
    if delegates_review {
        objective.push_str(
            "Give the next reviewer this list or its file path and the instructions below.\n",
        );
    }
    objective.push_str(rule);
    let (list, file) = handoff::prepare(&findings);
    objective.push_str("\n\n");
    objective.push_str(&list);
    Ok(Some(PriorHandoff {
        text: objective,
        file,
    }))
}

pub(super) fn update_previous(
    mut previous: Prior,
    stage: &StageRun,
    rounds: &Rounds<'_>,
) -> Option<(StageRunId, Value)> {
    assert_ne!(stage.id, previous.stage);
    let first = rounds.rounds.first().expect("a settled review has rounds");
    let last = previous
        .record
        .get_mut("rounds")?
        .as_array_mut()?
        .last_mut()?;
    let reviews = last.get_mut("reviews")?.as_array_mut()?;
    let mut changed = false;
    for review in reviews {
        let ids = review.get("finding_ids").and_then(Value::as_array);
        let Some(ids) = ids.filter(|ids| !ids.is_empty()) else {
            continue;
        };
        let resolved = ids
            .iter()
            .all(|id| id.as_str().is_some_and(|id| first.resolved.contains(id)));
        review["next_round_resolved"] = Value::Bool(resolved);
        changed = true;
    }
    changed.then_some((previous.stage, previous.record))
}
