//! A failed run's failure class, read from the records the run left (its
//! settled stages and the stage it failed under), never from its error's words.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::catalog::StageKind;
use archon_core::cost::{BUDGET_UNENFORCEABLE, BUDGET_USED_UP};
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::outcome::{ClassSource, FailureClass};
use archon_core::run::{
    PipelineRun, RunState, SHIP_STEP, StageOutcome, StageRun, StageState, Verdict,
};
use rusqlite::{Connection, params};

use super::super::codec::SqlContext;

/// Why the run that failed under `stage_id` with `error` failed. Only the
/// prefixes a stage or the daemon writes on purpose are read from `error`.
pub(in crate::store) fn failure_class(
    stage_id: &str,
    error: &str,
    stages: &[StageRun],
) -> FailureClass {
    let ending = ending_attempt(stage_id, stages);
    if error.starts_with(BUDGET_USED_UP) || error.starts_with(BUDGET_UNENFORCEABLE) {
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

/// A deadline, a cost cap or the agent backend stopped the stage before it
/// judged the work. Aether's cost stop also marks the agent failed, so it
/// is read before the generic infrastructure flag.
fn stopped_class(ending: &[&StageRun]) -> Option<FailureClass> {
    assert!(!ending.is_empty());
    let failed_with = |marker: &str, at_start: bool| {
        ending.iter().any(|s| match &s.state {
            StageState::Failed { error } if at_start => error.starts_with(marker),
            StageState::Failed { error } => error.contains(marker),
            _ => false,
        })
    };
    if ending.iter().any(|s| s.state == StageState::TimedOut) {
        return Some(FailureClass::Timeout);
    }
    if failed_with(StageOutcome::COST_BUDGET_STOP, false) {
        return Some(FailureClass::Budget);
    }
    let infra = ending.iter().any(|s| s.infrastructure_failure);
    (infra || failed_with(StageOutcome::DISK_FULL_SPAWN, true))
        .then_some(FailureClass::Infrastructure)
}

/// What the stage that ended the run was for. A review that passed did not
/// fail the run, so it is not counted as a review failure.
fn judged_class(stage_id: &str, ending: &[&StageRun]) -> FailureClass {
    assert!(!ending.is_empty());
    assert!(ending.iter().all(|s| s.stage_id == stage_id));
    let all_passed = ending.iter().all(|s| passed(s));
    let kind = ending.iter().find_map(|s| s.kind);
    let reviews =
        ending.iter().any(|s| holds_review(s)) || (kind.is_none() && stage_id.contains("review"));
    if reviews && !all_passed {
        return FailureClass::Review;
    }
    match kind {
        Some(StageKind::Test) => FailureClass::Verification,
        Some(StageKind::Ship) => FailureClass::Ship,
        Some(_) => FailureClass::Other,
        None => unrecorded_kind(stage_id),
    }
}

/// Whether the stage ran a review: a review stage, or a stage whose result
/// carries the structured review the gate reads.
pub(in crate::store) fn holds_review(stage: &StageRun) -> bool {
    stage.kind == Some(StageKind::Review) || nested_review(stage).is_some_and(|r| r.is_object())
}

fn nested_review(stage: &StageRun) -> Option<&serde_json::Value> {
    stage.structured.as_ref()?.get("review")
}

/// Whether the stage passed as the gate reads it: its own verdict, and the
/// verdict of the review it carries unless Archon refused that review.
fn passed(stage: &StageRun) -> bool {
    if stage.state != StageState::Completed || stage.verdict != Some(Verdict::Pass) {
        return false;
    }
    let Some(review) = nested_review(stage).filter(|r| r.is_object()) else {
        return true;
    };
    let verdict = review.get("verdict").and_then(serde_json::Value::as_str);
    let review_passed = verdict.is_some_and(|v| {
        v.trim().eq_ignore_ascii_case("skipped") || Verdict::of_phrase(v) == Some(Verdict::Pass)
    });
    review_passed && !stage.review_refused()
}

/// Stage runs from before Archon kept their stage's kind (1.4.x) name it only
/// by its id; a review among them is read by [`judged_class`].
fn unrecorded_kind(stage_id: &str) -> FailureClass {
    assert!(!stage_id.is_empty(), "a stage run names its stage");
    if stage_id.contains("test") {
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
    pub before: BTreeMap<String, u64>,
    pub after: BTreeMap<String, u64>,
    pub legacy: BTreeMap<String, u64>,
    pub recomputed: u64,
}

impl Reclassified {
    fn rows(&self) -> u64 {
        self.before.values().sum()
    }

    fn log(&self) {
        let classes: BTreeSet<_> = self.before.keys().chain(self.after.keys()).collect();
        for class in classes {
            let count = |map: &BTreeMap<String, u64>| map.get(class).copied().unwrap_or(0);
            tracing::info!(
                class = class.as_str(),
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
/// stored and readable, keeping the old class beside it; any other row keeps
/// its old class. Only the class fields change: no record is decoded whole.
pub(in crate::store) fn reclassify(conn: &Connection) -> Result<Reclassified, ArchonError> {
    let mut stmt = conn
        .prepare("SELECT run, COALESCE(failure_class, '') FROM outcomes WHERE result = 'failed' ORDER BY run")
        .ctx("prepare the failed outcomes")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ctx("query the failed outcomes")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read the failed outcomes")?;
    let mut counts = Reclassified::default();
    for (run, old) in rows {
        *counts.before.entry(old.clone()).or_default() += 1;
        let (class, source) = match recomputed(conn, &run)? {
            Some(class) => (class.as_str().to_owned(), ClassSource::Recomputed),
            None => (old.clone(), ClassSource::Legacy163),
        };
        match source {
            ClassSource::Legacy163 => *counts.legacy.entry(class.clone()).or_default() += 1,
            _ => counts.recomputed += 1,
        }
        save(conn, &run, &class, source, &old)?;
        *counts.after.entry(class).or_default() += 1;
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

/// The run's class from its stage records; `None` when any of them is gone
/// or cannot be read, so the row keeps its old class.
fn recomputed(conn: &Connection, run: &str) -> Result<Option<FailureClass>, ArchonError> {
    let Ok(id) = PipelineRunId::parse(run) else {
        tracing::warn!(
            run,
            "an outcome with an unreadable run id keeps its 1.6.3 class"
        );
        return Ok(None);
    };
    let (stored, stages) = match readable_run(conn, &id) {
        Ok(read) => read,
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error @ ArchonError::Corrupt(_)) => {
            tracing::warn!(run, %error, "an unreadable run keeps its 1.6.3 failure class");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let RunState::Failed { stage_id, error } = &stored.state else {
        tracing::warn!(
            run,
            "a failed outcome's run did not fail; it keeps its 1.6.3 class"
        );
        return Ok(None);
    };
    assert!(stages.iter().all(|s| s.run == id));
    Ok(Some(failure_class(stage_id, error, &stages)))
}

/// The run and every one of its stage runs, refusing an undecodable row: a
/// lost slot could hide the stop that ended the run.
fn readable_run(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<(PipelineRun, Vec<StageRun>), ArchonError> {
    let stored = super::super::runs::load(conn, run)?;
    if &stored.id != run {
        return Err(ArchonError::Corrupt(format!(
            "run row {run} holds {}",
            stored.id
        )));
    }
    let stages = super::super::runs::stages_of_strict(conn, run)?;
    Ok((stored, stages))
}

/// Write the new class into the row's columns and, when its record is JSON,
/// into the record; `updated_at` stays, as no correction or merge happened.
fn save(
    conn: &Connection,
    run: &str,
    class: &str,
    source: ClassSource,
    old: &str,
) -> Result<(), ArchonError> {
    assert!(!run.is_empty(), "an outcome row names its run");
    let changed = conn
        .execute(
            "UPDATE outcomes SET failure_class = NULLIF(?2, ''), class_source = ?3,
                 failure_class_163 = NULLIF(?4, ''),
                 data = CASE WHEN json_valid(data)
                        THEN json_set(data, '$.failure', NULLIF(?2, ''), '$.failure_source', ?3)
                        ELSE data END
             WHERE run = ?1",
            params![run, class, source.as_str(), old],
        )
        .ctx("migrate an outcome's failure class")?;
    assert_eq!(changed, 1, "the outcome row exists");
    Ok(())
}
