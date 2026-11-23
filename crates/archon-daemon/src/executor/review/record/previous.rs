use std::collections::BTreeMap;

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::run::{PipelineRun, StageRun};
use archon_queue::Store;
use serde_json::Value;

use super::{Finding, RoundId, Rounds};

pub(in crate::executor::review) fn prior_findings(
    store: &Store,
    stage: &StageRun,
) -> Result<Vec<Finding>, ArchonError> {
    Ok(previous(store, stage)?.map_or_else(Vec::new, |p| p.open))
}

pub(super) struct Prior {
    stage: StageRunId,
    head: Option<String>,
    pub(super) open: Vec<Finding>,
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
            && (!same_run || candidate.attempt < stage.attempt)
            && candidate.state.is_terminal()
    });
    for candidate in earlier {
        assert_ne!(stage.id, candidate.id);
        assert_eq!(stage.run == candidate.run, same_run);
        let records = candidate
            .structured
            .as_ref()
            .and_then(|s| s.get("review_rounds"));
        if let Some(record) = records.filter(|r| r.get("findings").is_some()) {
            let found: BTreeMap<String, Finding> =
                serde_json::from_value(record["findings"].clone())?;
            let head = record
                .get("rounds")
                .and_then(Value::as_array)
                .and_then(|rounds| rounds.last())
                .and_then(|r| r.get("head"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let open = found
                .into_values()
                .filter(|f| f.resolved_in.is_none())
                .collect();
            return Ok(Some(Prior {
                stage: candidate.id.clone(),
                head,
                open,
                record: record.clone(),
            }));
        }
    }
    Ok(None)
}

pub(in crate::executor) fn append_prior(
    store: &Store,
    stage: &StageRun,
    objective: &mut String,
    delegates_review: bool,
) -> Result<(), ArchonError> {
    if stage.only_reviews == Some(true)
        || (!delegates_review && stage.kind != Some(StageKind::Review))
    {
        return Ok(());
    }
    let Some(prior) = previous(store, stage)?.filter(|p| !p.open.is_empty()) else {
        return Ok(());
    };
    assert!(!objective.is_empty());
    assert!(prior.open.iter().all(|f| f.resolved_in.is_none()));
    objective.push_str("\n\n## Open findings from the previous review\n\n");
    if let Some(head) = prior.head {
        objective.push_str(&format!(
            "The last review read `{head}`. Review the new delta if that head is still an ancestor.\n"
        ));
    }
    let rule = "Return confirmed fixes as IDs in `resolved_findings`. Keep the `id` on each finding still open. Any real blocker found anywhere fails.";
    if delegates_review {
        objective.push_str(
            "Include these findings and the following instructions in the next review prompt.\n",
        );
    }
    objective.push_str(rule);
    for finding in prior.open {
        objective.push_str(&format!("\n- {}: {}", finding.id, finding.description));
    }
    Ok(())
}

pub(super) fn update_previous(
    mut previous: Prior,
    stage: &StageRun,
    rounds: &Rounds<'_>,
) -> Option<(StageRunId, Value)> {
    assert_ne!(stage.id, previous.stage);
    let first = rounds.rounds.first().expect("a settled review has rounds");
    let at = RoundId {
        stage: stage.id.to_string(),
        number: first.number,
    };
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
        let resolved = ids.iter().all(|id| {
            id.as_str()
                .and_then(|id| rounds.findings.get(id))
                .is_some_and(|finding| finding.resolved_in.as_ref() == Some(&at))
        });
        review["next_round_resolved"] = Value::Bool(resolved);
        changed = true;
    }
    changed.then_some((previous.stage, previous.record))
}
