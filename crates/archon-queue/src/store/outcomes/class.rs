//! A failed run's failure class, read from the records the run left (its
//! settled stages and the stage it failed under), never from its error's words.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::outcome::{ClassSource, FailureClass, Outcome};
use archon_core::run::{
    BUDGET_STAGE, RunState, SHIP_STEP, StageOutcome, StageRun, StageState, Verdict,
};
use rusqlite::{Connection, params};

use super::super::codec::{SqlContext, from_json, to_json};

/// Why the run that failed under `stage_id` with `error` failed. Only the
/// `BLOCKED:` and `ESCALATE:` prefixes a stage writes on purpose are read.
pub(in crate::store) fn failure_class(
    stage_id: &str,
    error: &str,
    stages: &[StageRun],
) -> FailureClass {
    let ending = ending_attempt(stage_id, stages);
    if stage_id == BUDGET_STAGE && ending.is_empty() {
        return FailureClass::Budget;
    }
    if error.starts_with(StageOutcome::ESCALATE_PREFIX) {
        return FailureClass::Escalated;
    }
    if error.starts_with(StageOutcome::BLOCKED_PREFIX) {
        return FailureClass::Blocked;
    }
    if ending.is_empty() {
        return match stage_id {
            SHIP_STEP => FailureClass::Ship,
            _ => FailureClass::Other,
        };
    }
    stopped_class(&ending).unwrap_or_else(|| judged_class(stage_id, &ending))
}

/// The last stage run of each slot of the stage's last attempt: what ended
/// the run. An infrastructure retry replaces a slot within its attempt.
fn ending_attempt<'a>(stage_id: &str, stages: &'a [StageRun]) -> Vec<&'a StageRun> {
    let of_stage = || stages.iter().filter(|s| s.stage_id == stage_id);
    let Some(last) = of_stage().map(|s| s.attempt).max() else {
        return Vec::new();
    };
    let mut slots = BTreeSet::new();
    let ending: Vec<&StageRun> = of_stage()
        .rev()
        .filter(|s| s.attempt == last && slots.insert(s.fan_index))
        .collect();
    assert!(!ending.is_empty(), "the last attempt has a stage run");
    assert!(ending.iter().all(|s| s.attempt == last));
    ending
}

/// A deadline, the agent backend or a cost cap stopped the stage before it
/// judged the work.
fn stopped_class(ending: &[&StageRun]) -> Option<FailureClass> {
    assert!(!ending.is_empty());
    if ending.iter().any(|s| s.state == StageState::TimedOut) {
        return Some(FailureClass::Timeout);
    }
    if ending.iter().any(|s| s.infrastructure_failure) {
        return Some(FailureClass::Infrastructure);
    }
    let cost_stop = ending.iter().any(|s| {
        matches!(&s.state, StageState::Failed { error } if error.contains(StageOutcome::COST_BUDGET_STOP))
    });
    cost_stop.then_some(FailureClass::Budget)
}

/// What the stage that ended the run was for. A review that passed did not
/// fail the run, so it is not counted as a review failure.
fn judged_class(stage_id: &str, ending: &[&StageRun]) -> FailureClass {
    assert!(!ending.is_empty());
    assert!(ending.iter().all(|s| s.stage_id == stage_id));
    let all_passed = ending
        .iter()
        .all(|s| s.state == StageState::Completed && s.verdict == Some(Verdict::Pass));
    let holds_review = |s: &&StageRun| {
        s.kind == Some(StageKind::Review)
            || s.structured
                .as_ref()
                .is_some_and(|v| v.get("review").is_some_and(|r| r.is_object()))
    };
    if ending.iter().any(holds_review) && !all_passed {
        return FailureClass::Review;
    }
    match ending.iter().find_map(|s| s.kind) {
        Some(StageKind::Test) => FailureClass::Verification,
        Some(StageKind::Ship) => FailureClass::Ship,
        Some(_) => FailureClass::Other,
        None => unrecorded_kind(stage_id),
    }
}

/// Stage runs from before Archon kept their stage's kind (1.4.x) name it only
/// by its id.
fn unrecorded_kind(stage_id: &str) -> FailureClass {
    assert!(!stage_id.is_empty(), "a stage run names its stage");
    if stage_id.contains("review") {
        FailureClass::Review
    } else if stage_id.contains("test") {
        FailureClass::Verification
    } else if stage_id.contains("ship") {
        FailureClass::Ship
    } else {
        FailureClass::Other
    }
}

/// Per class, how many failed outcomes held it before and after the
/// migration, and how many keep the class 1.6.3 gave them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::store) struct Reclassified {
    pub before: BTreeMap<&'static str, u64>,
    pub after: BTreeMap<&'static str, u64>,
    pub legacy: BTreeMap<&'static str, u64>,
    pub recomputed: u64,
}

impl Reclassified {
    fn rows(&self) -> u64 {
        self.before.values().sum()
    }

    fn log(&self) {
        let classes: BTreeSet<_> = self.before.keys().chain(self.after.keys()).collect();
        for class in classes {
            let count = |map: &BTreeMap<&str, u64>| map.get(class).copied().unwrap_or(0);
            tracing::info!(
                class,
                old = count(&self.before),
                new = count(&self.after),
                legacy_163 = count(&self.legacy),
                "outcome failure classes migrated"
            );
        }
        let legacy: u64 = self.legacy.values().sum();
        tracing::info!(
            rows = self.rows(),
            recomputed = self.recomputed,
            legacy_163 = legacy,
            "outcome failure classes migrated from stage records"
        );
    }
}

/// Recompute the class of every stored failed outcome whose run is still
/// stored, keeping the old class beside it; a pruned run's stays as it was.
pub(in crate::store) fn reclassify(conn: &Connection) -> Result<Reclassified, ArchonError> {
    let mut stmt = conn
        .prepare("SELECT run, data FROM outcomes WHERE result = 'failed' ORDER BY run")
        .ctx("prepare the failed outcomes")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ctx("query the failed outcomes")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read the failed outcomes")?;
    let mut counts = Reclassified::default();
    for (run, data) in rows {
        let mut outcome: Outcome = from_json(&data, "outcome")?;
        let old_class = outcome
            .failure
            .ok_or_else(|| ArchonError::Corrupt(format!("failed outcome of {run} has no class")))?;
        assert_eq!(
            outcome.run.as_str(),
            run,
            "an outcome row holds its own run"
        );
        *counts.before.entry(old_class.as_str()).or_default() += 1;
        let (class, source) = match recomputed(conn, &outcome.run)? {
            Some(class) => (class, ClassSource::Recomputed),
            None => (old_class, ClassSource::Legacy163),
        };
        match source {
            ClassSource::Legacy163 => *counts.legacy.entry(class.as_str()).or_default() += 1,
            _ => counts.recomputed += 1,
        }
        *counts.after.entry(class.as_str()).or_default() += 1;
        outcome.failure = Some(class);
        outcome.failure_source = Some(source);
        save(conn, &outcome, old_class)?;
    }
    let legacy: u64 = counts.legacy.values().sum();
    assert_eq!(
        counts.recomputed + legacy,
        counts.rows(),
        "each row is recomputed or kept"
    );
    assert_eq!(
        counts.after.values().sum::<u64>(),
        counts.rows(),
        "no row is lost"
    );
    counts.log();
    Ok(counts)
}

/// The run's class from its stage records; `None` when they are gone or
/// cannot be read, so the row keeps its old class.
fn recomputed(conn: &Connection, run: &PipelineRunId) -> Result<Option<FailureClass>, ArchonError> {
    let stored = match super::super::runs::load(conn, run) {
        Ok(stored) => stored,
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error @ ArchonError::Corrupt(_)) => {
            tracing::warn!(%run, %error, "an unreadable run keeps its 1.6.3 failure class");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let RunState::Failed { stage_id, error } = &stored.state else {
        tracing::warn!(%run, "a failed outcome's run did not fail; it keeps its 1.6.3 class");
        return Ok(None);
    };
    let stages = super::super::runs::stages_of(conn, run)?;
    assert!(stages.iter().all(|s| &s.run == run));
    Ok(Some(failure_class(stage_id, error, &stages)))
}

/// Write the new class into the row's columns and record, leaving its
/// `updated_at` alone: no correction or merge happened.
fn save(conn: &Connection, outcome: &Outcome, old: FailureClass) -> Result<(), ArchonError> {
    outcome.check()?;
    let class = outcome.failure.expect("a failed outcome has a class");
    let source = outcome
        .failure_source
        .expect("a migrated class has a source");
    let changed = conn
        .execute(
            "UPDATE outcomes SET failure_class = ?2, class_source = ?3, failure_class_163 = ?4,
             data = ?5 WHERE run = ?1",
            params![
                outcome.run.as_str(),
                class.as_str(),
                source.as_str(),
                old.as_str(),
                to_json(outcome, "outcome")?
            ],
        )
        .ctx("migrate an outcome's failure class")?;
    assert_eq!(changed, 1, "the outcome row exists");
    Ok(())
}
