//! Run-level observations made when a failed item is queued again.

use archon_core::Timestamp;
use archon_core::cost::DayRange;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::model_evidence::RoutingEvidenceRow;
use archon_core::outcome::FailureClass;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
use archon_core::steer::RunEdit;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::Store;
use super::codec::{SqlContext, from_json, to_json, to_u64, write_tx};
use super::outcomes::class;
use super::{items, runs, settings};

const PREFIX: &str = "routing-observation:";
const PREFIX_END: &str = "routing-observation;";

#[derive(Serialize)]
struct Observation<'a> {
    repo: Option<&'a RepoId>,
    pipeline: &'a str,
    finished_at: Timestamp,
    missed: Option<bool>,
}

impl Store {
    pub fn steered_pipeline(&self, item: &QueueItemId) -> Result<Option<String>, ArchonError> {
        steered_pipeline(&self.conn, item)
    }

    /// Store the shape, item, and audit history together. For a failed item,
    /// also queue it again and observe the routing decision of its failed run.
    pub fn apply_shape_steer(
        &mut self,
        item: &QueueItem,
        shape: Option<&RunEdit>,
        history: (&str, &str),
        at: Timestamp,
    ) -> Result<QueueItem, ArchonError> {
        if !matches!(
            item.state,
            QueueState::Queued | QueueState::Blocked { .. } | QueueState::Failed { .. }
        ) {
            return Err(ArchonError::InvalidTransition(format!(
                "item {} is {} and cannot take a waiting shape steer",
                item.id,
                item.state.kind()
            )));
        }
        let min_uses = self.learning_config()?.retire_min_uses;
        let tx = write_tx(&mut self.conn)?;
        items::update_in(&tx, item)?;
        let next = shape
            .map(|edit| shape_pipeline(&item.id, edit))
            .transpose()?
            .unwrap_or(&item.pipeline);
        let revive = matches!(item.state, QueueState::Failed { .. });
        if revive {
            observe_requeue(&tx, item, Some(next))?;
        }
        let key = super::keys::item_shape(&item.id);
        match shape {
            Some(edit) => settings::kv_set(&tx, &key, &to_json(edit, "steered stages")?)?,
            None => settings::kv_delete(&tx, &key)?,
        }
        settings::kv_set(&tx, history.0, history.1)?;
        let stored = if revive {
            items::requeue_in(&tx, &item.id, at, min_uses)?
        } else {
            item.clone()
        };
        assert_eq!(stored.id, item.id);
        assert!(!revive || stored.state == QueueState::Queued);
        tx.commit().ctx("commit shape steer")?;
        Ok(stored)
    }

    /// Apply a retry's item changes and queue it without changing its route.
    pub fn requeue_item(
        &mut self,
        item: &QueueItem,
        at: Timestamp,
    ) -> Result<QueueItem, ArchonError> {
        let min_uses = self.learning_config()?.retire_min_uses;
        let tx = write_tx(&mut self.conn)?;
        items::update_in(&tx, item)?;
        observe_requeue(&tx, item, None)?;
        let queued = items::requeue_in(&tx, &item.id, at, min_uses)?;
        assert_eq!(queued.id, item.id);
        assert_eq!(queued.state, QueueState::Queued);
        tx.commit().ctx("commit item retry")?;
        Ok(queued)
    }
}

/// Observed misses and unclassified failures by the failed run's finish
/// day. Historical runs without a recorded route use their stored name.
pub(super) fn read(
    conn: &Connection,
    repo: Option<&RepoId>,
    pipeline: Option<&str>,
    range: &DayRange,
) -> Result<Vec<RoutingEvidenceRow>, ArchonError> {
    assert!(range.since <= range.until, "the evidence range is checked");
    let mut stmt = conn
        .prepare(ROUTING_EVIDENCE)
        .ctx("prepare routing evidence")?;
    let groups = stmt
        .query_map(
            params![
                PREFIX,
                PREFIX_END,
                repo.map(RepoId::as_str),
                pipeline,
                range.since.to_string(),
                range.until.to_string()
            ],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .ctx("query routing evidence")?;
    let mut rows = Vec::new();
    for group in groups {
        let (repo, pipeline, misses, unclassified) = group.ctx("read routing evidence")?;
        let repo = repo.map(|id| RepoId::parse(&id)).transpose()?;
        rows.push(RoutingEvidenceRow {
            repo,
            pipeline,
            misses: misses.map(|n| to_u64(n, "routing misses")).transpose()?,
            unclassified: to_u64(unclassified, "unclassified routing decisions")?,
        });
    }
    assert!(rows.iter().all(|row| !row.pipeline.is_empty()));
    assert!(
        rows.windows(2)
            .all(|p| (&p[0].repo, &p[0].pipeline) < (&p[1].repo, &p[1].pipeline))
    );
    Ok(rows)
}

const ROUTING_EVIDENCE: &str = "
WITH evidence(repo, pipeline, finished_at, missed, failed) AS (
    SELECT json_extract(value, '$.repo'), json_extract(value, '$.pipeline'),
           json_extract(value, '$.finished_at'), json_extract(value, '$.missed'), 1
    FROM kv WHERE key >= ?1 AND key < ?2
    UNION ALL
    SELECT i.repo, r.pipeline, json_extract(r.data, '$.finished_at'), NULL,
           coalesce(json_extract(r.data, '$.state.state') = 'failed', 1)
    FROM runs r JOIN items i ON i.id = r.item
    WHERE r.terminal = 1 AND json_valid(r.data)
      AND (?3 IS NULL OR i.repo = ?3) AND (?4 IS NULL OR r.pipeline = ?4)
      AND NOT EXISTS (SELECT 1 FROM kv WHERE key = ?1 || r.id)
    UNION ALL
    SELECT o.repo, o.pipeline, o.finished_at, NULL, o.result = 'failed'
    FROM outcomes o
    WHERE (?3 IS NULL OR o.repo = ?3) AND (?4 IS NULL OR o.pipeline = ?4)
      AND NOT EXISTS (SELECT 1 FROM kv WHERE key = ?1 || o.run)
      AND NOT EXISTS (SELECT 1 FROM runs WHERE id = o.run AND json_valid(data))
)
SELECT repo, pipeline, sum(missed), sum(failed AND missed IS NULL)
FROM evidence
WHERE (?3 IS NULL OR repo = ?3) AND (?4 IS NULL OR pipeline = ?4)
  AND date(finished_at, 'localtime') BETWEEN ?5 AND ?6
GROUP BY repo, pipeline ORDER BY repo, pipeline
";

fn shape_pipeline<'a>(item: &QueueItemId, edit: &'a RunEdit) -> Result<&'a str, ArchonError> {
    match edit {
        RunEdit::SwitchPipeline { pipeline, .. } if !pipeline.trim().is_empty() => Ok(pipeline),
        _ => Err(ArchonError::Corrupt(format!(
            "steered stages of item {item} must name a pipeline in a switch edit"
        ))),
    }
}

fn steered_pipeline(conn: &Connection, item: &QueueItemId) -> Result<Option<String>, ArchonError> {
    let Some(raw) = settings::kv_get(conn, &super::keys::item_shape(item))? else {
        return Ok(None);
    };
    let edit = from_json(&raw, &format!("steered stages of item {item}"))?;
    Ok(Some(shape_pipeline(item, &edit)?.to_owned()))
}

fn observe_requeue(
    tx: &Connection,
    item: &QueueItem,
    next_pipeline: Option<&str>,
) -> Result<(), ArchonError> {
    assert!(
        !tx.is_autocommit(),
        "the observation belongs to the revival"
    );
    let QueueState::Failed { run: Some(id), .. } = &item.state else {
        return Ok(());
    };
    let run = match runs::load(tx, id) {
        Err(error @ (ArchonError::NotFound(_) | ArchonError::Corrupt(_)))
            if next_pipeline.is_none() =>
        {
            tracing::warn!(run = %id, %error, "retry has no readable run for routing evidence");
            return Ok(());
        }
        other => other?,
    };
    assert_eq!(run.item, item.id, "a failed item names its own run");
    let Some(finished_at) = run
        .finished_at
        .filter(|_| matches!(run.state, RunState::Failed { .. }))
    else {
        return Ok(());
    };
    let pipeline = steered_pipeline(tx, &item.id)?.unwrap_or_else(|| item.pipeline.clone());
    let missed = if next_pipeline.is_some_and(|next| next != pipeline) {
        match escalated(tx, &run) {
            Err(error @ ArchonError::Corrupt(_)) => {
                tracing::warn!(run = %run.id, %error, "routing observation has unreadable evidence");
                None
            }
            other => other?,
        }
    } else {
        Some(false)
    };
    let observation = Observation {
        repo: item.repo.as_ref(),
        pipeline: &pipeline,
        finished_at,
        missed,
    };
    let inserted = tx
        .execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO NOTHING",
            params![
                format!("{PREFIX}{id}"),
                to_json(&observation, "routing observation")?
            ],
        )
        .ctx("record routing observation")?;
    assert!(inserted <= 1, "one observation per failed run");
    Ok(())
}

fn escalated(conn: &Connection, run: &PipelineRun) -> Result<Option<bool>, ArchonError> {
    let RunState::Failed { stage_id, error } = &run.state else {
        unreachable!("only a failed run can escalate");
    };
    let stages = runs::stages_of_strict(conn, &run.id)?;
    if stages.iter().any(|stage| stage.run != run.id) {
        return Err(ArchonError::Corrupt(format!(
            "a stage of run {} names another run",
            run.id,
        )));
    }
    if error.starts_with(archon_core::run::UNRESUMABLE_RUN)
        || !(stages.iter().any(|s| &s.stage_id == stage_id)
            || class::classed_without_its_stage(stage_id, error, &stages))
    {
        return Ok(None);
    }
    assert!(run.finished_at.is_some(), "the observed run finished");
    let reviews = class::failed_reviews(conn, &stages)?;
    assert!(reviews.len() <= stages.len());
    Ok(Some(
        class::failure_class(run, &stages, &reviews) == FailureClass::Escalated,
    ))
}
