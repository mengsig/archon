//! `submit_verdict`: a stage agent has its verdict read while it still runs.
//! Aether cannot ask a finished agent again, so a verdict the gate would read
//! as neither pass nor fail is answered once, here, with what to restate, and
//! the restated one is final. The ask is kept by stage run, so a restarted
//! archond does not ask twice. A stage that never calls it settles as before.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::protocol::Reply;
use archon_core::run::{Restatement, StageRun, StageState, Verdict, VerdictReading};
use archon_queue::Store;

use crate::context::DaemonContext;

/// The store key prefix of a live stage run's restatement.
pub(super) const KEY_PREFIX: &str = "verdict_restatement:";

/// Read the verdict `caller`'s stage is about to complete with, as the gate
/// will, and say whether it stands, is to be restated once, or is final.
pub(crate) async fn submit(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
    summary: &str,
    structured: Option<&serde_json::Value>,
) -> Result<Reply, ArchonError> {
    let agent = caller.ok_or_else(|| {
        ArchonError::Forbidden(
            "submit_verdict requires Aether caller metadata (send_caller_meta = true)".into(),
        )
    })?;
    let table = ctx.runs.lock().await;
    let mut store = ctx.store.lock().await;
    let stage = store
        .stage_run_by_agent(agent)?
        .filter(|stage| !stage.state.is_terminal())
        .ok_or_else(|| {
            ArchonError::Forbidden(format!(
                "submit_verdict is for a stage agent while its stage runs, and Archon has none \
                 on record for {agent}. Complete your task as your instructions say"
            ))
        })?;
    let driver = table.drivers.get(&stage.run).ok_or_else(|| {
        ArchonError::Unavailable(format!(
            "Archon is not driving stage `{}`'s run right now, so its verdict is read when you \
             complete. Complete your task as your contract says",
            stage.stage_id
        ))
    })?;
    if !driver.holds_verdict(&stage.stage_id) {
        return Err(ArchonError::Forbidden(format!(
            "submit_verdict is for a stage whose contract has a Verdict section, which stage \
             `{}` has not. Complete with `complete_task` as your contract says",
            stage.stage_id
        )));
    }
    let reading = read(&stage, summary, structured);
    let state = load(&store, &stage.id)?;
    let (next, check) = Restatement::submit(state, reading);
    if next != state {
        let next = next.expect("a submission never forgets an ask");
        store.kv_set(&key(&stage.id), &serde_json::to_string(&next)?)?;
    }
    tracing::info!(
        stage = %stage.id, %agent, decision = ?check.decision, verdict = ?check.reading.verdict,
        note = ?check.reading.note, "verdict submitted before completion"
    );
    Ok(Reply::VerdictCheck(check))
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
/// stands over the result the stage completed with.
pub(super) fn bound(store: &Store, mut settled: StageRun) -> Result<StageRun, ArchonError> {
    assert!(settled.state.is_terminal(), "only an outcome is bound");
    let Some(restatement) = load(store, &settled.id)? else {
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

fn key(stage: &StageRunId) -> String {
    format!("{KEY_PREFIX}{stage}")
}

fn load(store: &Store, stage: &StageRunId) -> Result<Option<Restatement>, ArchonError> {
    let key = key(stage);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let state = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
    Ok(Some(state))
}
