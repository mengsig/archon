//! `submit_verdict`: a stage agent has its verdict read while it still runs.
//! Aether cannot ask a finished agent again, so a verdict the gate would read
//! as neither pass nor fail is answered here with what to restate, and the
//! one sent back as the restatement is final. The ask is kept by stage
//! attempt, so neither a restarted archond nor a replacement of the attempt's
//! stage run asks for a second one. A stage that never calls it settles as before.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::protocol::Reply;
use archon_core::run::{
    Restatement, StageRun, StageState, Submission, Verdict, VerdictReading, names_environment,
};
use archon_queue::Store;

use super::RunTable;
use crate::context::DaemonContext;

/// The store key prefix of a live stage attempt's restatement.
const KEY_PREFIX: &str = "verdict_restatement:";

/// The result a stage agent submits before it finishes with it.
pub(crate) struct Submitted<'a> {
    pub summary: &'a str,
    pub structured: Option<&'a serde_json::Value>,
    /// Sent as the restatement an earlier answer asked for.
    pub restating: bool,
}

/// Read the verdict `caller`'s stage is about to finish with, as the gate
/// will, and say whether it stands, is to be restated, or is final.
pub(crate) async fn submit(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
    sent: Submitted<'_>,
) -> Result<Reply, ArchonError> {
    let agent = caller.ok_or_else(|| {
        ArchonError::Forbidden(
            "submit_verdict requires Aether caller metadata (send_caller_meta = true)".into(),
        )
    })?;
    if sent
        .structured
        .is_some_and(|structured| !structured.is_object())
    {
        return Err(ArchonError::Invalid(
            "submit_verdict: `structured` must be an object, as `complete_task` takes it".into(),
        ));
    }
    let table = ctx.runs.lock().await;
    let mut store = ctx.store.lock().await;
    let stage = submitting_stage(&table, &store, agent)?;
    let submission = Submission {
        reading: read(&stage, sent.summary, sent.structured),
        restating: sent.restating,
        environment: sent.structured.is_some_and(names_environment),
    };
    let key = key(&stage);
    let state = load(&store, &key)?;
    let (next, check) = Restatement::submit(state, submission);
    if next != state {
        let next = next.expect("a submission never forgets an ask");
        store.kv_set(&key, &serde_json::to_string(&next)?)?;
    }
    tracing::info!(
        stage = %stage.id, %agent, decision = ?check.decision, verdict = ?check.reading.verdict,
        note = ?check.reading.note, "verdict submitted before completion"
    );
    Ok(Reply::VerdictCheck(check))
}

/// The running stage `agent` submits for, which holds its run's verdict.
fn submitting_stage(
    table: &RunTable,
    store: &Store,
    agent: &AetherAgentId,
) -> Result<StageRun, ArchonError> {
    let stage = store
        .stage_run_by_agent(agent)?
        .filter(|stage| !stage.state.is_terminal())
        .ok_or_else(|| {
            ArchonError::Forbidden(format!(
                "submit_verdict is for a stage agent while its stage runs, and Archon has none \
                 on record for {agent}. Finish as your instructions say"
            ))
        })?;
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(agent),
        "found by its agent"
    );
    let driver = table.drivers.get(&stage.run).ok_or_else(|| {
        ArchonError::Unavailable(format!(
            "Archon is not driving stage `{}`'s run right now, so its verdict is read when you \
             finish. Finish as your instructions say",
            stage.stage_id
        ))
    })?;
    if !driver.holds_verdict(&stage.stage_id) {
        return Err(ArchonError::Forbidden(format!(
            "submit_verdict is for a stage whose contract has a Verdict section, which stage \
             `{}` has not. Finish as your instructions say",
            stage.stage_id
        )));
    }
    Ok(stage)
}

/// A result's verdict as settlement reads it for `stage`.
fn read(stage: &StageRun, summary: &str, structured: Option<&serde_json::Value>) -> VerdictReading {
    VerdictReading::new(match stage.only_reviews {
        Some(true) => Verdict::read_review_lead(summary, structured),
        _ => Verdict::read(summary, structured),
    })
}

/// `settled` with the restatement its attempt was asked for on record, and a
/// completed one's verdict bound by it: a final restatement that is no pass
/// stands over the result the stage finished with.
pub(super) fn bound(store: &Store, mut settled: StageRun) -> Result<StageRun, ArchonError> {
    assert!(settled.state.is_terminal(), "only an outcome is bound");
    let Some(restatement) = load(store, &key(&settled))? else {
        return Ok(settled);
    };
    settled.restatement = Some(restatement);
    if settled.state != StageState::Completed {
        return Ok(settled);
    }
    let verdict = settled.verdict.expect("a completed stage has a verdict");
    let terminal = VerdictReading::new((verdict, settled.verdict_note));
    let stands = restatement.bind(terminal);
    if stands != terminal {
        tracing::info!(
            stage = %settled.id, result = ?terminal.verdict, restated = ?stands.verdict,
            "the final restated verdict stands over the completed result"
        );
    }
    settled.verdict = Some(stands.verdict);
    settled.verdict_note = stands.note;
    Ok(settled)
}

/// Forget the restatement of the attempt `settled` ended, once it is stored
/// on that stage run. A stage run a replacement follows keeps it instead.
pub(super) fn forget(store: &mut Store, settled: &StageRun) -> Result<(), ArchonError> {
    assert!(
        settled.state.is_terminal(),
        "a running attempt keeps its ask"
    );
    let key = key(settled);
    if store.kv_get(&key)?.is_some() {
        store.kv_delete(&key)?;
    }
    Ok(())
}

/// Delete the restatements of stage attempts none of whose stage runs is live.
pub(super) fn forget_ended(store: &mut Store) -> Result<(), ArchonError> {
    for (key, _) in store.kv_prefix(KEY_PREFIX)? {
        let attempt = Attempt::of_key(&key)?;
        let stages = store.stage_runs(&attempt.run)?;
        let mut held = stages.iter().filter(|stage| attempt.holds(stage));
        if held.all(|stage| stage.state.is_terminal()) {
            store.kv_delete(&key)?;
        }
    }
    Ok(())
}

/// A stage attempt: the slot every replacement of its stage run shares.
#[derive(Debug, PartialEq, Eq)]
struct Attempt {
    run: PipelineRunId,
    attempt: u32,
    fan_index: u32,
    stage_id: String,
}

impl Attempt {
    /// The attempt a [`key`] names; the stage id comes last, as it may hold a `:`.
    fn of_key(key: &str) -> Result<Self, ArchonError> {
        let corrupt = || ArchonError::Corrupt(format!("restatement key {key}"));
        let raw = key.strip_prefix(KEY_PREFIX).ok_or_else(corrupt)?;
        let mut parts = raw.splitn(4, ':');
        let mut part = || parts.next().ok_or_else(corrupt);
        let run = PipelineRunId::parse(part()?).map_err(|_| corrupt())?;
        let attempt = part()?.parse().map_err(|_| corrupt())?;
        let fan_index = part()?.parse().map_err(|_| corrupt())?;
        let stage_id = part()?.to_owned();
        assert!(
            !stage_id.is_empty() || key.ends_with(':'),
            "the rest is the stage id"
        );
        Ok(Self {
            run,
            attempt,
            fan_index,
            stage_id,
        })
    }

    fn holds(&self, stage: &StageRun) -> bool {
        stage.run == self.run
            && stage.attempt == self.attempt
            && stage.fan_index == self.fan_index
            && stage.stage_id == self.stage_id
    }
}

/// The key of the stage attempt `stage` is a run of.
fn key(stage: &StageRun) -> String {
    assert!(!stage.stage_id.is_empty(), "a stage run names its stage");
    let (run, attempt, fan) = (&stage.run, stage.attempt, stage.fan_index);
    format!("{KEY_PREFIX}{run}:{attempt}:{fan}:{}", stage.stage_id)
}

fn load(store: &Store, key: &str) -> Result<Option<Restatement>, ArchonError> {
    assert!(key.starts_with(KEY_PREFIX), "a restatement key");
    let Some(raw) = store.kv_get(key)? else {
        return Ok(None);
    };
    let state = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_names_its_attempt_whatever_the_stage_is_called() {
        let run = PipelineRunId::new();
        for stage_id in ["e2e", "odd:1:2"] {
            let key = format!("{KEY_PREFIX}{run}:2:3:{stage_id}");
            let attempt = Attempt::of_key(&key).expect("a key Archon wrote");
            let named = Attempt {
                run: run.clone(),
                attempt: 2,
                fan_index: 3,
                stage_id: stage_id.to_owned(),
            };
            assert_eq!(attempt, named);
        }
        for corrupt in ["verdict_restatement:run_x:1:0:e2e", "verdict_restatement:"] {
            let refused = Attempt::of_key(corrupt);
            assert!(matches!(refused, Err(ArchonError::Corrupt(_))), "{corrupt}");
        }
        let short = format!("{KEY_PREFIX}{run}:2");
        assert!(matches!(
            Attempt::of_key(&short),
            Err(ArchonError::Corrupt(_))
        ));
    }
}
