//! V24's pass over stored outcomes: each failed run's class is recomputed
//! from its stage records, and each reviewed pull request frozen into its outcome.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::outcome::{ClassSource, FailureClass, UNSCORED_FAILURES};
use archon_core::run::{PipelineRun, RunState, StageRun, UNRESUMABLE_RUN};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params};

use super::super::codec::{SqlContext, from_json, text_column};
use super::class::{classed_without_its_stage, failed_reviews, failure_class, review_on_record};

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
    let rows = failed_outcomes(conn)?;
    let mut counts = Reclassified::default();
    let mut rescored = Vec::new();
    for (run, old) in rows {
        *counts.before.entry(old.clone()).or_default() += 1;
        let (class, source) = match recomputed(conn, &run)? {
            Some(class) => {
                rescored.push((run.clone(), !UNSCORED_FAILURES.contains(&class)));
                (class.as_str().to_owned(), ClassSource::Recomputed)
            }
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
    let lessons = super::super::lessons::rescore_failed_runs(conn, &rescored)?;
    tracing::info!(
        runs = rescored.len(),
        lessons,
        "lesson scores follow the migrated classes"
    );
    counts.log();
    Ok(counts)
}

/// Each failed outcome's run and stored class (`""` for none). A row whose
/// run or class is not text cannot be rewritten by its key: it keeps all it holds.
fn failed_outcomes(conn: &Connection) -> Result<Vec<(String, String)>, ArchonError> {
    let mut stmt = conn
        .prepare("SELECT run, failure_class FROM outcomes WHERE result = 'failed' ORDER BY run")
        .ctx("prepare the failed outcomes")?;
    let class = |r: &rusqlite::Row<'_>| match r.get_ref(1)? {
        ValueRef::Null => Ok(Some(String::new())),
        _ => text_column(r, 1),
    };
    let rows = stmt
        .query_map([], |r| Ok((text_column(r, 0)?, class(r)?)))
        .ctx("query the failed outcomes")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read the failed outcomes")?;
    let stored = rows.len();
    let readable: Vec<(String, String)> = rows
        .into_iter()
        .filter_map(|(run, class)| Some((run?, class?)))
        .collect();
    assert!(readable.len() <= stored, "no row is read twice");
    if readable.len() < stored {
        let skipped = stored - readable.len();
        tracing::warn!(
            skipped,
            "failed outcomes whose run or class is not text are left as they are"
        );
    }
    Ok(readable)
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
    let Some((stored, stages)) = kept_records(conn, &id)? else {
        return Ok(None);
    };
    let RunState::Failed { stage_id, error } = &stored.state else {
        tracing::warn!(
            run,
            "a failed outcome's run did not fail; it keeps its 1.6.3 class"
        );
        return Ok(None);
    };
    assert!(stages.iter().all(|s| s.run == id));
    if error.starts_with(UNRESUMABLE_RUN) {
        tracing::warn!(
            run,
            "recovery failed the run and may have dropped its stage rows; it keeps its 1.6.3 class"
        );
        return Ok(None);
    }
    let Some(missing) = kept(run, missing_stage_rows(conn, run, &stages))? else {
        return Ok(None);
    };
    if let Some(stage) = missing {
        tracing::warn!(
            run,
            stage = stage.as_str(),
            "stage runs the outcome observed are gone; it keeps its 1.6.3 class"
        );
        return Ok(None);
    }
    let recorded = stages.iter().any(|s| &s.stage_id == stage_id);
    if !recorded && !classed_without_its_stage(stage_id, error) {
        tracing::warn!(
            run,
            stage = stage_id.as_str(),
            "the stage that ended the run left no record; it keeps its 1.6.3 class"
        );
        return Ok(None);
    }
    let Some(failed) = kept(run, failed_reviews(conn, &stages))? else {
        return Ok(None);
    };
    Ok(Some(failure_class(stage_id, error, &stages, &failed)))
}

/// The stage whose runs the outcome observed more of than are still stored,
/// if any: each stage run was observed once per model it ran on. An observed
/// stage that holds no id, or no attempts above zero, is corrupt.
fn missing_stage_rows(
    conn: &Connection,
    run: &str,
    stages: &[StageRun],
) -> Result<Option<String>, ArchonError> {
    let mut stored: BTreeMap<&str, i64> = BTreeMap::new();
    for stage in stages {
        *stored.entry(&stage.stage_id).or_default() += 1 + stage.model_segments.len() as i64;
    }
    let mut stmt = conn
        .prepare_cached("SELECT stage_id, attempts FROM outcome_stages WHERE run = ?1")
        .ctx("prepare the observed stage runs")?;
    let count = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Option<i64>> {
        match r.get_ref(1)? {
            ValueRef::Integer(count) if count > 0 => Ok(Some(count)),
            _ => Ok(None),
        }
    };
    let rows = stmt
        .query_map([run], |r| Ok((text_column(r, 0)?, count(r)?)))
        .ctx("query the observed stage runs")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read the observed stage runs")?;
    let mut observed: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        let (Some(stage), Some(count)) = row else {
            let what = format!("an observed stage of run {run} holds no stage id or attempts");
            return Err(ArchonError::Corrupt(what));
        };
        let seen = observed.entry(stage).or_default();
        *seen = seen.saturating_add(count);
    }
    let missing = observed
        .into_iter()
        .find(|(stage, seen)| *seen > stored.get(stage.as_str()).copied().unwrap_or(0));
    Ok(missing.map(|(stage, _)| stage))
}

/// `read`, or `None` for a record of `run` that is gone or cannot be read:
/// the migration leaves that run as it was and goes on.
fn kept<T>(run: &str, read: Result<T, ArchonError>) -> Result<Option<T>, ArchonError> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(ArchonError::NotFound(_)) => Ok(None),
        Err(error @ ArchonError::Corrupt(_)) => {
            tracing::warn!(run, %error, "an unreadable record: the run keeps what 1.6.3 stored");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// The run and its stage runs, when both are still stored and readable.
fn kept_records(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<Option<(PipelineRun, Vec<StageRun>)>, ArchonError> {
    kept(run.as_str(), readable_run(conn, run))
}

/// Store, in each outcome whose item targets a pull request and pushes
/// nothing, that pull request, when a reviewer's verdict is on record for the
/// run: the rule capture applies. Returns how many outcomes gained one.
pub(in crate::store) fn freeze_review_prs(conn: &Connection) -> Result<u64, ArchonError> {
    let mut stmt = conn
        .prepare(
            "SELECT o.run, json_extract(i.data, '$.onto.pr')
             FROM outcomes o JOIN items i ON i.id = o.item
             WHERE json_valid(i.data) AND json_valid(o.data)
               AND CASE WHEN json_valid(i.data)
                   THEN json_extract(i.data, '$.onto.review_only') END
               AND CASE WHEN json_valid(i.data)
                   THEN json_type(i.data, '$.onto.pr') END = 'text'
             ORDER BY o.run",
        )
        .ctx("prepare the review-only outcomes")?;
    let rows = stmt
        .query_map([], |r| Ok((text_column(r, 0)?, text_column(r, 1)?)))
        .ctx("query the review-only outcomes")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read the review-only outcomes")?;
    // A target whose run or pull request is not readable text names none.
    let targets: Vec<(String, String)> = rows
        .into_iter()
        .filter_map(|(run, pr)| Some((run?, pr?)))
        .filter(|(_, pr)| !pr.trim().is_empty())
        .collect();
    let mut frozen = 0;
    for (run, pr) in &targets {
        if !reviewed(conn, run)? {
            continue;
        }
        let changed = conn
            .execute(
                "UPDATE outcomes SET data = json_set(data, '$.review_pr', ?2) WHERE run = ?1",
                params![run, pr],
            )
            .ctx("freeze a reviewed pull request")?;
        assert_eq!(changed, 1, "the outcome row exists");
        frozen += 1;
    }
    tracing::info!(
        outcomes = targets.len(),
        frozen,
        "reviewed pull requests frozen into their outcomes"
    );
    Ok(frozen)
}

/// Whether a reviewer's verdict is on record for a stage run of `run`.
fn reviewed(conn: &Connection, run: &str) -> Result<bool, ArchonError> {
    let Ok(id) = PipelineRunId::parse(run) else {
        return Ok(false);
    };
    let Some((_, stages)) = kept_records(conn, &id)? else {
        return Ok(false);
    };
    let Some(failed) = kept(run, failed_reviews(conn, &stages))? else {
        return Ok(false);
    };
    assert!(stages.iter().all(|s| s.run == id));
    Ok(stages.iter().any(|s| review_on_record(s, &failed)))
}

/// The run and every one of its stage runs, refusing an undecodable row or
/// one that holds another run: a lost slot could hide the stop that ended it.
fn readable_run(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<(PipelineRun, Vec<StageRun>), ArchonError> {
    let data = conn
        .query_row("SELECT data FROM runs WHERE id = ?1", [run.as_str()], |r| {
            text_column(r, 0)
        })
        .optional()
        .ctx("read a migrated outcome's run")?;
    let data = data.ok_or_else(|| ArchonError::NotFound(format!("pipeline run {run}")))?;
    let data = data.ok_or_else(|| ArchonError::Corrupt(format!("run row {run} holds no text")))?;
    let stored: PipelineRun = from_json(&data, "pipeline run")?;
    if &stored.id != run {
        return Err(ArchonError::Corrupt(format!(
            "run row {run} holds {}",
            stored.id
        )));
    }
    Ok((stored, readable_stages(conn, run)?))
}

/// Every stage run of `run` in the order they started. A row that is not
/// text, does not decode, or is no stage run of `run` is corrupt.
fn readable_stages(conn: &Connection, run: &PipelineRunId) -> Result<Vec<StageRun>, ArchonError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT data FROM stage_runs WHERE run = ?1
             ORDER BY started_at, attempt, fan_index, id",
        )
        .ctx("prepare a migrated outcome's stage runs")?;
    let rows = stmt
        .query_map([run.as_str()], |r| text_column(r, 0))
        .ctx("query a migrated outcome's stage runs")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read a migrated outcome's stage runs")?;
    let mut stages = Vec::with_capacity(rows.len());
    for data in rows {
        let unreadable = || ArchonError::Corrupt(format!("a stage row of {run} holds no text"));
        let stage: StageRun = from_json(&data.ok_or_else(unreadable)?, "stage run")?;
        if &stage.run != run || stage.attempt == 0 || stage.stage_id.is_empty() {
            return Err(ArchonError::Corrupt(format!(
                "stage row {} is no stage run of {run}",
                stage.id
            )));
        }
        stages.push(stage);
    }
    Ok(stages)
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
