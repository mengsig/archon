//! Which model choice a correction may penalize. Never guessed: anything
//! ambiguous records the correction with no model credit and says why.

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::feedback::{
    Attribution, CorrectionRequest, Credit, CurationState, MemoryCuration, OperatorFeedback,
};
use archon_core::id::FeedbackId;
use archon_core::outcome::Outcome;
use archon_core::run::{PipelineRun, StageRun, StageState};

use super::CorrectionFacts;
use crate::store::Store;

/// The correction as it will be recorded, its attribution frozen now.
pub(super) fn feedback(
    store: &Store,
    request: &CorrectionRequest,
    facts: &CorrectionFacts,
) -> Result<OperatorFeedback, ArchonError> {
    let run = store.run(&request.run)?;
    if run.item != request.item {
        return Err(ArchonError::Invalid(format!(
            "run {} belongs to item {}, not {}",
            run.id, run.item, request.item
        )));
    }
    let (credit, outcome) = credit(store, &run, &request.stage)?;
    let curation = pending_curation(request, facts)?;
    let attribution = Attribution {
        repo: store.item(&request.item)?.repo,
        pipeline: run.pipeline.clone(),
        context: outcome.and_then(|o| o.context),
        delivery_commit: facts.delivery_commit.clone(),
        credit,
    };
    let feedback = OperatorFeedback {
        id: FeedbackId::new(),
        request_key: request.request_key(),
        item: request.item.clone(),
        run: run.id.clone(),
        stage: request.stage.clone(),
        cause: request.cause,
        reason: request.reason().to_owned(),
        attribution,
        observed_at: facts.observed_at,
        revoked_at: None,
        curation,
        memory_feedback: facts.memory_feedback.clone(),
    };
    feedback.check()?;
    Ok(feedback)
}

/// The forget the request asks for, bound to its Aether instance now.
fn pending_curation(
    request: &CorrectionRequest,
    facts: &CorrectionFacts,
) -> Result<Option<MemoryCuration>, ArchonError> {
    let Some(memory) = &request.forget_memory else {
        return Ok(None);
    };
    let Some(instance) = facts.aether_instance.as_ref().filter(|id| !id.is_empty()) else {
        return Err(ArchonError::Invalid(format!(
            "a forget of memory {memory} needs the Aether instance it is bound to"
        )));
    };
    let curation = MemoryCuration {
        memory: memory.clone(),
        instance_id: instance.clone(),
        state: CurationState::Pending,
        before: None,
        forgotten_at: None,
        restored_at: None,
        error: None,
        updated_at: facts.observed_at,
    };
    curation.check()?;
    Ok(Some(curation))
}

/// The model credit for `stage` of `run`, with the run's outcome.
fn credit(
    store: &Store,
    run: &PipelineRun,
    stage: &str,
) -> Result<(Credit, Option<Outcome>), ArchonError> {
    let mut stages = store.stage_runs(&run.id)?;
    stages.retain(|s| s.stage_id == stage);
    if stages.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "run {} ran no stage {stage:?}",
            run.id
        )));
    }
    assert!(stages.iter().all(|s| s.run == run.id), "read by their run");
    let outcome = store.outcome(&run.id)?;
    let reshaped = !store.run_edits(&run.id)?.is_empty()
        || store
            .kv_get(&format!("learning/reshaped/{}", run.id))?
            .is_some();
    let evidence = Evidence {
        run,
        stages: &stages,
        reshaped,
        outcome: outcome.as_ref(),
    };
    let credit = match model_credit(&evidence) {
        Ok(stage) => Credit::Model {
            stage_run: stage.id.clone(),
            choice: stage.current_choice().clone(),
        },
        Err(why) => Credit::Unavailable { why: why.into() },
    };
    Ok((credit, outcome))
}

/// What decides a credit: the run, its attempts of the stage, and its outcome.
struct Evidence<'a> {
    run: &'a PipelineRun,
    stages: &'a [StageRun],
    reshaped: bool,
    outcome: Option<&'a Outcome>,
}

/// The stage run whose one completed choice produced the stage, or why
/// there is none.
fn model_credit<'a>(evidence: &Evidence<'a>) -> Result<&'a StageRun, &'static str> {
    let stages = evidence.stages;
    assert!(!stages.is_empty(), "the stage ran in the run");
    if !evidence.run.state.is_terminal() {
        return Err("the run has not finished");
    }
    if evidence.reshaped {
        return Err("the run was reshaped while it ran");
    }
    if recorded_kind(stages)? != StageKind::Work {
        return Err("only a work stage's model is credited");
    }
    let last = stages
        .iter()
        .max_by_key(|s| s.attempt)
        .expect("checked non-empty");
    let mut attempts: Vec<u32> = stages.iter().map(|s| s.attempt).collect();
    attempts.sort_unstable();
    attempts.dedup();
    if attempts.len() != stages.len() || stages.iter().any(|s| s.fan_index != 0) {
        return Err("the stage ran several agents at once");
    }
    let choice = last.current_choice();
    if stages
        .iter()
        .any(|s| !s.model_segments.is_empty() || s.current_choice() != choice)
    {
        return Err("the stage's attempts used more than one model");
    }
    if last.state != StageState::Completed {
        return Err("the stage's last attempt did not complete");
    }
    sampled(evidence.outcome, last)?;
    Ok(last)
}

/// The kind every attempt of the stage ran as, recorded when it ran. The
/// current catalog may have changed the stage since, so it is never asked.
fn recorded_kind(stages: &[StageRun]) -> Result<StageKind, &'static str> {
    let first = stages.first().expect("the stage ran in the run");
    let Some(kind) = first.kind else {
        return Err("the stage's kind was not recorded when it ran");
    };
    if stages.iter().any(|s| s.kind != Some(kind)) {
        return Err("the stage's attempts ran as different kinds");
    }
    Ok(kind)
}

/// Whether `stage` is a successful routing sample of the run's outcome.
fn sampled(outcome: Option<&Outcome>, stage: &StageRun) -> Result<(), &'static str> {
    let Some(outcome) = outcome else {
        return Err("the run has no recorded outcome");
    };
    if outcome.repo.is_none() || outcome.context.is_none() {
        return Err("the run has no repository routing context");
    }
    if !crate::store::model_evidence::counts_for_routing(outcome) {
        return Err("the run's outcome does not count for routing");
    }
    let passed = outcome.stages.iter().any(|s| {
        s.stage == stage.stage_id && &s.choice == stage.current_choice() && s.passed == Some(true)
    });
    if !passed {
        return Err("the stage has no successful routing sample to correct");
    }
    Ok(())
}
