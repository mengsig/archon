//! A retry keeps the proposal the operator approved (ARCHITECTURE §18.3): a
//! new run of the item on the same branch, for the same brief, starts past it.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::queue::QueueItem;
use archon_core::run::{GateDecision, GateSubject};
use archon_pipeline::{ApprovedProposal, RunDriver};
use archon_queue::Store;

/// The approved proposal a run started past, read again to rebuild its driver.
fn proposal_key(run: &PipelineRunId) -> String {
    format!("run_proposal:{run}")
}

/// The brief a gated run started with, fingerprinted; it outlives the run's
/// pruned variables, as a retry compares it with the brief it starts with.
fn brief_key(run: &PipelineRunId) -> String {
    format!("run_brief:{run}")
}

/// 64-bit FNV-1a in hex: stable across builds and toolchains, unlike std's
/// hasher, and only ever compared for equality.
fn fingerprint(text: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = text.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    format!("{hash:016x}")
}

/// Record what a later retry and a rebuild of a run with a proposal gate
/// read: the brief it started with, and the proposal it started past.
pub(super) fn save_start(
    store: &mut Store,
    run: &PipelineRunId,
    driver: &RunDriver,
    brief: &str,
) -> Result<(), ArchonError> {
    let Some(gate) = driver.gates().proposal.as_deref() else {
        assert!(
            driver.approved_proposal().is_none(),
            "only a proposal gate is started past"
        );
        return Ok(());
    };
    store.kv_set(&brief_key(run), &fingerprint(brief))?;
    let Some(proposal) = driver.approved_proposal() else {
        return Ok(());
    };
    assert_eq!(proposal.stage_id, gate, "started past its own gate");
    store.kv_set(&proposal_key(run), &serde_json::to_string(proposal)?)
}

/// The approved proposal `run` started past, if it did.
pub(super) fn started_past(
    store: &Store,
    run: &PipelineRunId,
) -> Result<Option<ApprovedProposal>, ArchonError> {
    let Some(raw) = store.kv_get(&proposal_key(run))? else {
        return Ok(None);
    };
    let proposal: ApprovedProposal = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("approved proposal of run {run}: {e}")))?;
    if proposal.stage_id.is_empty() {
        return Err(ArchonError::Corrupt(format!(
            "approved proposal of run {run} names no stage"
        )));
    }
    Ok(Some(proposal))
}

/// The proposal a new run of `item` on `branch` starts past: the latest
/// earlier run's, if its last proposal gate was approved (or it started past
/// one itself), on the same branch and for the brief the item has now. A
/// fresh retry's new branch, an amended brief, or a run that left no brief
/// fingerprint proposes again.
pub(super) fn approved_before(
    store: &Store,
    item: &QueueItem,
    branch: &str,
) -> Result<Option<ApprovedProposal>, ArchonError> {
    let runs = store.runs_for_item(&item.id)?;
    let Some(latest) = runs.last() else {
        return Ok(None);
    };
    assert_eq!(
        latest.item, item.id,
        "runs_for_item returns the item's runs"
    );
    let same_work = latest.pipeline == item.pipeline && latest.branch == branch;
    if !latest.state.is_terminal() || !same_work {
        return Ok(None);
    }
    let brief = store.kv_get(&brief_key(&latest.id))?;
    if brief.as_deref() != Some(fingerprint(&item.brief).as_str()) {
        return Ok(None);
    }
    let gates = store.gates_for_run(&latest.id)?;
    assert!(gates.iter().all(|g| g.run == latest.id), "the run's gates");
    let last = gates
        .into_iter()
        .filter(|g| g.subject == GateSubject::Proposal)
        .max_by_key(|g| g.attempt);
    let Some(gate) = last else {
        return started_past(store, &latest.id);
    };
    match gate.decision {
        Some(GateDecision::Approve { note }) => Ok(Some(ApprovedProposal {
            stage_id: gate.stage_id,
            summary: gate.summary,
            note,
        })),
        _ => Ok(None),
    }
}

/// `driver` started past `approved` while it still gates that proposal: a
/// pipeline or knob that no longer asks for it runs as it is.
pub(super) fn start_past(
    driver: RunDriver,
    approved: Option<ApprovedProposal>,
) -> Result<RunDriver, ArchonError> {
    let gate = driver.gates().proposal.as_deref();
    let approved = approved.filter(|a| gate == Some(a.stage_id.as_str()));
    match approved {
        Some(approved) => driver.with_approved_proposal(approved),
        None => Ok(driver),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_and_tell_briefs_apart() {
        assert_eq!(fingerprint(""), "cbf29ce484222325");
        assert_eq!(fingerprint("a"), "af63dc4c8601ec8c");
        assert_ne!(fingerprint("brief"), fingerprint("brief\n\n## Amendment"));
    }
}
