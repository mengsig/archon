//! `submit_verdict`: a stage agent checks its verdict and records while it still runs.
//! Aether cannot ask a finished agent again, so a verdict the gate would read
//! as neither pass nor fail is answered here with what to restate, and the
//! one sent back as the restatement is final. The ask is kept by stage
//! attempt, so neither a restarted archond nor a replacement of the attempt's
//! stage run asks for a second one. A stage that never calls it settles as before.

mod records;

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::protocol::Reply;
use archon_core::run::{
    PipelineRun, Restatement, StageOutcome, StageRun, StageState, Submission, Verdict,
    VerdictCheck, VerdictReading, names_environment,
};
use archon_pipeline::spec::Owed;
use archon_queue::Store;

use super::RunTable;
use super::spawn::{self, ParentBinding};
use crate::context::DaemonContext;

/// The store key prefix of a live stage attempt's restatement.
const KEY_PREFIX: &str = "verdict_restatement:";

/// How long a caller whose spawn or wake may not be on record yet waits for it.
const BINDING_GRACE: Duration = Duration::from_secs(5);
/// How often that wait re-reads the store, should a record's signal be missed.
const BINDING_RECHECK: Duration = Duration::from_secs(1);
const _: () = assert!(BINDING_RECHECK.as_millis() <= BINDING_GRACE.as_millis());

/// The result a stage agent submits before it finishes with it.
pub(crate) struct Submitted<'a> {
    pub summary: &'a str,
    pub structured: Option<&'a serde_json::Value>,
    /// Sent as the restatement an earlier answer asked for.
    pub restating: bool,
}

/// Check what `caller`'s stage is about to finish with against its contract.
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
    let deadline = tokio::time::Instant::now() + BINDING_GRACE;
    loop {
        // Listening before the store is read: a binding recorded between the
        // read and the wait still ends the wait. No lock is held while waiting.
        let recorded = ctx.spawn_recorded.notified();
        tokio::pin!(recorded);
        recorded.as_mut().enable();
        // The clock is read before the look, so the look that refuses began past
        // the deadline: it sees every binding recorded within the wait.
        let began = tokio::time::Instant::now();
        let standing = {
            let table = ctx.runs.lock().await;
            let mut store = ctx.store.lock().await;
            standing(&table, &mut store, agent, &sent)?
        };
        match standing {
            Standing::Checked(check) => return Ok(check.map_or(Reply::Ok, Reply::VerdictCheck)),
            Standing::Records(records) => return records.check(ctx, agent, &sent).await,
            Standing::Unbound if began < deadline => {}
            Standing::Unbound | Standing::Refused => {
                return Err(ArchonError::Forbidden(format!(
                    "submit_verdict is for a stage agent while its stage runs, and Archon has \
                     none on record for {agent}. Finish as your instructions say"
                )));
            }
        }
        tokio::select! {
            () = &mut recorded => {}
            () = tokio::time::sleep_until(deadline.min(began + BINDING_RECHECK)) => {}
        }
    }
}

/// Where a caller of `submit_verdict` stands.
enum Standing {
    Checked(Option<VerdictCheck>),
    /// Record verification needs Git and Aether, with no daemon locks held.
    Records(Box<RecordCheck>),
    /// A spawn or wake not on record yet may make it a stage's agent.
    Unbound,
    Refused,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Contract {
    verdict: bool,
    owed: Owed,
}

struct RecordCheck {
    run: PipelineRun,
    stage: StageRun,
    contract: Contract,
}

/// Where `agent` stands, from one reading of the store: a binding recorded
/// between a lookup and a later look for one pending would be seen by neither.
fn standing(
    table: &RunTable,
    store: &mut Store,
    agent: &AetherAgentId,
    sent: &Submitted<'_>,
) -> Result<Standing, ArchonError> {
    let Some(stage) = held_stage(store, agent)? else {
        let pending = binding_pending(store, agent)?;
        return Ok(if pending {
            Standing::Unbound
        } else {
            Standing::Refused
        });
    };
    let contract = contract(table, &stage)?;
    if contract.owed.requires_records() && records_can_pass(store, &stage, sent, contract.verdict)?
    {
        let run = store.run(&stage.run)?;
        if run.state.is_terminal() {
            return Ok(Standing::Refused);
        }
        return Ok(Standing::Records(Box::new(RecordCheck {
            run,
            stage,
            contract,
        })));
    }
    let verdict = contract
        .verdict
        .then(|| check(store, &stage, sent))
        .transpose()?;
    Ok(Standing::Checked(verdict))
}

fn records_can_pass(
    store: &Store,
    stage: &StageRun,
    sent: &Submitted<'_>,
    verdict: bool,
) -> Result<bool, ArchonError> {
    assert!(!stage.state.is_terminal(), "only a running stage submits");
    if StageOutcome::states_failure(sent.summary, sent.structured) {
        return Ok(false);
    }
    if !verdict {
        return Ok(true);
    }
    let reading = read(stage, sent.summary, sent.structured);
    if reading.verdict != Verdict::Pass {
        return Ok(false);
    }
    let state = load(store, &key(stage))?;
    Ok(state.is_none_or(|restatement| restatement.bind(reading).verdict == Verdict::Pass))
}

/// The check of `sent` for running `stage`, recorded with its attempt.
fn check(
    store: &mut Store,
    stage: &StageRun,
    sent: &Submitted<'_>,
) -> Result<VerdictCheck, ArchonError> {
    assert!(!stage.state.is_terminal(), "only a running stage submits");
    let submission = Submission {
        reading: read(stage, sent.summary, sent.structured),
        restating: sent.restating,
        environment: sent.structured.is_some_and(names_environment),
        said: archon_pipeline::submission_text(sent.summary, sent.structured),
    };
    let key = key(stage);
    let state = load(store, &key)?;
    let (next, check) = Restatement::submit(state.as_ref(), submission);
    if next != state {
        let next = next.expect("a submission never forgets an ask");
        store.kv_set(&key, &serde_json::to_string(&next)?)?;
    }
    tracing::info!(
        stage = %stage.id, agent = ?stage.aether_agent, decision = ?check.decision,
        verdict = ?check.reading.verdict, note = ?check.reading.note,
        "verdict submitted before completion"
    );
    Ok(check)
}

/// The running stage run `agent` holds, when it is its attempt's current one: after a
/// crash mid-replacement the superseded one is still live, and its agent is not answered.
fn held_stage(store: &Store, agent: &AetherAgentId) -> Result<Option<StageRun>, ArchonError> {
    let Some(stage) = store.stage_run_by_agent(agent)? else {
        return Ok(None);
    };
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(agent),
        "found by its agent"
    );
    if stage.state.is_terminal() {
        return Ok(None);
    }
    let attempt = Attempt::of(&stage);
    let stages = store.stage_runs(&stage.run)?;
    // Newest as the engine picks a slot's stage run: by start time, then id.
    let newest = stages
        .iter()
        .filter(|other| attempt.holds(other))
        .max_by_key(|other| (other.started_at, &other.id))
        .expect("the attempt holds the stage run found");
    if newest.id != stage.id {
        tracing::warn!(
            stage = %stage.id, replacement = %newest.id, %agent,
            "a superseded stage run's agent submitted a verdict"
        );
        return Ok(None);
    }
    Ok(Some(stage))
}

/// Whether a `spawning` stage may yet be bound to `agent`: one that resumes
/// it, or, when no stage run holds it, one whose create may have reached Aether.
fn binding_pending(store: &Store, agent: &AetherAgentId) -> Result<bool, ArchonError> {
    let unheld = store.stage_run_by_agent(agent)?.is_none();
    for stage in store.live_stage_runs()? {
        if stage.state != StageState::Spawning {
            continue;
        }
        let stored = match spawn::load_spawn(store, &stage.id) {
            Ok(stored) => stored,
            // `claim` fails such a stage before anything is sent.
            Err(ArchonError::NotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        let resumes = stored.resume.is_some_and(|kept| kept.agent == *agent);
        if resumes || (unheld && stored.parent != ParentBinding::Pending) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The verdict and records a live stage can submit under its current contract.
fn contract(table: &RunTable, stage: &StageRun) -> Result<Contract, ArchonError> {
    assert!(!stage.state.is_terminal(), "only a running stage submits");
    let driver = table.drivers.get(&stage.run).ok_or_else(|| {
        ArchonError::Unavailable(format!(
            "Archon is not driving stage `{}`'s run right now. Finish as your instructions say",
            stage.stage_id
        ))
    })?;
    let spec = driver.spec().stage(&stage.stage_id).ok_or_else(|| {
        ArchonError::Unavailable(format!(
            "Archon has no contract for stage `{}`",
            stage.stage_id
        ))
    })?;
    let owed = driver.spec().owed(spec);
    let verdict = archon_pipeline::contract::holds_verdict(spec, owed);
    if !verdict && !owed.requires_records() {
        return Err(ArchonError::Forbidden(format!(
            "stage `{}` has no verdict or required records to check. Finish as your instructions say",
            stage.stage_id
        )));
    }
    Ok(Contract { verdict, owed })
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
    if settled.state != StageState::Completed {
        settled.restatement = Some(restatement);
        return Ok(settled);
    }
    let verdict = settled.verdict.expect("a completed stage has a verdict");
    let terminal = VerdictReading::new((verdict, settled.verdict_note));
    let stands = restatement.bind(terminal);
    settled.restatement = Some(restatement);
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
/// on that stage run. An attempt another stage run still holds keeps it: a
/// replacement, or after a crash mid-replacement the stage run it superseded.
pub(super) fn forget(store: &mut Store, settled: &StageRun) -> Result<(), ArchonError> {
    assert!(
        settled.state.is_terminal(),
        "a running attempt keeps its ask"
    );
    let key = key(settled);
    if store.kv_get(&key)?.is_none() {
        return Ok(());
    }
    if Attempt::of(settled).lives_beside(store, Some(settled))? {
        return Ok(());
    }
    store.kv_delete(&key)
}

/// Delete the restatements of stage attempts none of whose stage runs is
/// live, and those whose run history pruning has since deleted.
pub(super) fn forget_ended(store: &mut Store) -> Result<(), ArchonError> {
    for (key, _) in store.kv_prefix(KEY_PREFIX)? {
        let live = match Attempt::of_key(&key)?.lives_beside(store, None) {
            Ok(live) => live,
            Err(ArchonError::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
        if !live {
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
    fn of(stage: &StageRun) -> Self {
        Self {
            run: stage.run.clone(),
            attempt: stage.attempt,
            fan_index: stage.fan_index,
            stage_id: stage.stage_id.clone(),
        }
    }

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

    /// Whether a stage run of this attempt other than `ended` is still live.
    fn lives_beside(&self, store: &Store, ended: Option<&StageRun>) -> Result<bool, ArchonError> {
        assert!(
            ended.is_none_or(|ended| self.holds(ended)),
            "one of its own"
        );
        let ended = ended.map(|ended| &ended.id);
        let stages = store.stage_runs(&self.run)?;
        let mut others = stages
            .iter()
            .filter(|stage| self.holds(stage) && Some(&stage.id) != ended);
        Ok(others.any(|stage| !stage.state.is_terminal()))
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
