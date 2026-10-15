//! Operator corrections (`archon learning`): bounded, reversible records
//! beside the immutable outcomes. Nothing here touches the legacy
//! corrections list or lesson uses.

mod attribution;

use std::collections::HashSet;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::feedback::{
    CorrectionRequest, Credit, MAX_FEEDBACK_PER_RUN, MemoryCuration, OperatorFeedback,
    feedback_window,
};
use archon_core::id::{FeedbackId, PipelineRunId, QueueItemId, RepoId};
use archon_core::model::ModelChoice;
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, from_json, to_json, ts};
use super::model_evidence::RouteQuery;

/// What the daemon knows about a correction that the store does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionFacts {
    pub delivery_commit: Option<String>,
    /// The Aether instance a requested forget is bound to; required with one.
    pub aether_instance: Option<String>,
    pub observed_at: Timestamp,
    /// Delivery intent frozen before the correction's first external call.
    pub memory_feedback: Option<archon_core::feedback::MemoryFeedback>,
}

impl Store {
    /// Records a correction, or returns the unrevoked one recorded for the
    /// same request with `true`. Refuses ids that do not belong together.
    pub fn record_operator_correction(
        &self,
        request: &CorrectionRequest,
        facts: &CorrectionFacts,
    ) -> Result<(OperatorFeedback, bool), ArchonError> {
        request.check()?;
        if !self.learning_config()?.enabled {
            return Err(ArchonError::Invalid(
                "learning is disabled, so no correction is recorded; enable [learning] first"
                    .into(),
            ));
        }
        let feedback = attribution::feedback(self, request, facts)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin operator correction")?;
        if let Some(existing) = by_request(&tx, &feedback.request_key)? {
            assert_eq!(existing.run, feedback.run, "a request key names its run");
            return Ok((existing, true));
        }
        let recorded: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM operator_feedback WHERE run = ?1",
                [feedback.run.as_str()],
                |row| row.get(0),
            )
            .ctx("count run corrections")?;
        if recorded >= MAX_FEEDBACK_PER_RUN as i64 {
            return Err(ArchonError::Invalid(format!(
                "run {} already has {MAX_FEEDBACK_PER_RUN} corrections",
                feedback.run
            )));
        }
        insert(&tx, &feedback)?;
        tx.commit().ctx("commit operator correction")?;
        Ok((feedback, false))
    }

    pub fn operator_feedback(&self, id: &FeedbackId) -> Result<OperatorFeedback, ArchonError> {
        load(&self.conn, id)?.ok_or_else(|| ArchonError::NotFound(format!("feedback {id}")))
    }

    /// Corrections with unfinished delivery or evidence still owed a retraction.
    pub fn pending_memory_feedback(&self) -> Result<Vec<FeedbackId>, ArchonError> {
        let mut statement = self.conn.prepare(
            "SELECT data FROM operator_feedback WHERE json_type(data, '$.memory_feedback') = 'object' ORDER BY observed_at, id"
        ).ctx("prepare pending memory feedback")?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .ctx("query pending memory feedback")?;
        let mut ids = Vec::new();
        for raw in rows {
            let feedback: OperatorFeedback =
                from_json(&raw.ctx("read memory feedback")?, "feedback")?;
            feedback.check()?;
            let ledger = feedback
                .memory_feedback
                .as_ref()
                .expect("selected a ledger");
            assert!(ledger.check().is_ok());
            if ledger.skipped.is_some() {
                continue;
            }
            let retract = feedback.revoked_at.is_some()
                && ledger.tasks.iter().any(|task| {
                    task.records.iter().any(|entry| {
                        entry.feedback.is_some() && !entry.retracted && entry.failed.is_none()
                    })
                });
            if ledger.counts().pending > 0 || retract {
                ids.push(feedback.id);
            }
        }
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            ids.len()
        );
        Ok(ids)
    }

    /// Every correction of the item's runs, oldest first.
    pub fn item_feedback(&self, item: &QueueItemId) -> Result<Vec<OperatorFeedback>, ArchonError> {
        let mut statement = self
            .conn
            .prepare("SELECT data FROM operator_feedback WHERE item = ?1 ORDER BY observed_at, id")
            .ctx("prepare item corrections")?;
        let rows = statement
            .query_map([item.as_str()], |row| row.get::<_, String>(0))
            .ctx("query item corrections")?;
        let mut feedback = Vec::new();
        for raw in rows {
            let event: OperatorFeedback = from_json(&raw.ctx("read item correction")?, "feedback")?;
            assert_eq!(&event.item, item, "a correction is read by its item");
            feedback.push(event);
        }
        Ok(feedback)
    }

    /// Revokes the correction's model penalty at `at`; a revoked one stays as it was.
    pub fn revoke_feedback(
        &self,
        id: &FeedbackId,
        at: Timestamp,
    ) -> Result<OperatorFeedback, ArchonError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin correction undo")?;
        let mut feedback =
            load(&tx, id)?.ok_or_else(|| ArchonError::NotFound(format!("feedback {id}")))?;
        if feedback.revoked_at.is_some() {
            return Ok(feedback);
        }
        feedback.revoked_at = Some(at.max(feedback.observed_at));
        feedback.check()?;
        update(&tx, &feedback)?;
        tx.commit().ctx("commit correction undo")?;
        Ok(feedback)
    }

    /// Updates the delivery ledger without changing curation or revocation.
    pub fn update_memory_feedback(
        &self,
        id: &FeedbackId,
        ledger: &archon_core::feedback::MemoryFeedback,
    ) -> Result<OperatorFeedback, ArchonError> {
        ledger.check()?;
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin memory feedback")?;
        let mut feedback =
            load(&tx, id)?.ok_or_else(|| ArchonError::NotFound(format!("feedback {id}")))?;
        let current = feedback.memory_feedback.as_ref().ok_or_else(|| {
            ArchonError::Invalid("correction has no memory feedback ledger".into())
        })?;
        assert_eq!(
            current.instance_id, ledger.instance_id,
            "instance binding is immutable"
        );
        assert_eq!(
            current.tasks.iter().map(|t| &t.task).collect::<Vec<_>>(),
            ledger.tasks.iter().map(|t| &t.task).collect::<Vec<_>>(),
            "task bindings are immutable"
        );
        feedback.memory_feedback = Some(ledger.clone());
        feedback.check()?;
        update(&tx, &feedback)?;
        tx.commit().ctx("commit memory feedback")?;
        Ok(feedback)
    }

    /// Replaces the correction's memory curation, which must name the same memory.
    pub fn update_curation(
        &self,
        id: &FeedbackId,
        curation: &MemoryCuration,
    ) -> Result<OperatorFeedback, ArchonError> {
        curation.check()?;
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin memory curation")?;
        let mut feedback =
            load(&tx, id)?.ok_or_else(|| ArchonError::NotFound(format!("feedback {id}")))?;
        let Some(current) = &feedback.curation else {
            return Err(ArchonError::Invalid(format!(
                "feedback {id} asked for no memory curation"
            )));
        };
        if current.memory != curation.memory || current.instance_id != curation.instance_id {
            return Err(ArchonError::Invalid(format!(
                "feedback {id} curates {} in its own Aether instance only",
                current.memory
            )));
        }
        feedback.curation = Some(curation.clone());
        feedback.check()?;
        update(&tx, &feedback)?;
        tx.commit().ctx("commit memory curation")?;
        Ok(feedback)
    }
}

/// Whether the repository cohort of `query` has had a credited quality
/// correction by `as_of`, revoked or expired too: one indexed probe.
pub(super) fn has_cohort_feedback(
    conn: &Connection,
    query: &RouteQuery<'_>,
    as_of: Timestamp,
) -> Result<bool, ArchonError> {
    assert!(
        !query.pipeline.is_empty() && !query.stage.is_empty(),
        "a checked query"
    );
    let Some(repo) = query.repo else {
        return Ok(false);
    };
    let (repo, pipeline, stage, at) = (repo.as_str(), query.pipeline, query.stage, ts(&as_of));
    match query.context {
        Some(c) => probe(
            conn,
            HAS_CONTEXT_FEEDBACK,
            params![
                repo,
                pipeline,
                stage,
                c.revision,
                c.review,
                c.task_class,
                at
            ],
        ),
        None => probe(conn, HAS_FEEDBACK, params![repo, pipeline, stage, at]),
    }
}

/// Whether a correction of the repository cohort of `query` penalizes
/// `as_of`, its sample read or not: one indexed probe.
pub(super) fn cohort_corrected(
    conn: &Connection,
    query: &RouteQuery<'_>,
    as_of: Timestamp,
) -> Result<bool, ArchonError> {
    assert!(
        !query.pipeline.is_empty() && !query.stage.is_empty(),
        "a checked query"
    );
    let Some(repo) = query.repo else {
        return Ok(false);
    };
    let since = ts(&(as_of - feedback_window()));
    let (repo, pipeline, stage, at) = (repo.as_str(), query.pipeline, query.stage, ts(&as_of));
    match query.context {
        Some(c) => probe(
            conn,
            ACTIVE_CONTEXT_FEEDBACK,
            params![
                repo,
                pipeline,
                stage,
                c.revision,
                c.review,
                c.task_class,
                at,
                since
            ],
        ),
        None => probe(
            conn,
            ACTIVE_FEEDBACK,
            params![repo, pipeline, stage, at, since],
        ),
    }
}

fn probe(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<bool, ArchonError> {
    conn.prepare_cached(sql)
        .ctx("prepare cohort probe")?
        .query_row(args, |row| row.get(0))
        .ctx("probe cohort corrections")
}

const HAS_CONTEXT_FEEDBACK: &str = "SELECT EXISTS (SELECT 1 FROM operator_feedback
    WHERE repo = ?1 AND pipeline = ?2 AND stage = ?3 AND revision = ?4 AND review = ?5
      AND task_class = ?6 AND observed_at <= ?7 AND stage_run IS NOT NULL
      AND cause = 'quality')";

const HAS_FEEDBACK: &str = "SELECT EXISTS (SELECT 1 FROM operator_feedback
    WHERE repo = ?1 AND pipeline = ?2 AND stage = ?3 AND observed_at <= ?4
      AND stage_run IS NOT NULL AND cause = 'quality')";

const ACTIVE_CONTEXT_FEEDBACK: &str = "SELECT EXISTS (SELECT 1 FROM operator_feedback
    WHERE repo = ?1 AND pipeline = ?2 AND stage = ?3 AND revision = ?4 AND review = ?5
      AND task_class = ?6 AND observed_at <= ?7 AND observed_at > ?8
      AND (revoked_at IS NULL OR revoked_at > ?7)
      AND stage_run IS NOT NULL AND cause = 'quality')";

const ACTIVE_FEEDBACK: &str = "SELECT EXISTS (SELECT 1 FROM operator_feedback
    WHERE repo = ?1 AND pipeline = ?2 AND stage = ?3 AND observed_at <= ?4
      AND observed_at > ?5 AND (revoked_at IS NULL OR revoked_at > ?4)
      AND stage_run IS NOT NULL AND cause = 'quality')";

/// One quality correction penalizing a routing sample now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mask {
    pub feedback: FeedbackId,
    pub run: PipelineRunId,
    pub choice: ModelChoice,
}

/// The corrections penalizing `stage` of `runs` as of `as_of`, oldest first:
/// read by run through its index, at most `MAX_FEEDBACK_PER_RUN` per run.
pub(super) fn active_masks(
    conn: &Connection,
    runs: &[&PipelineRunId],
    stage: &str,
    as_of: Timestamp,
) -> Result<Vec<Mask>, ArchonError> {
    let wanted: HashSet<&str> = runs.iter().map(|r| r.as_str()).collect();
    let since = as_of - feedback_window();
    let mut statement = conn
        .prepare_cached(ACTIVE_MASKS)
        .ctx("prepare correction masks")?;
    let rows = statement
        .query_map(
            params![to_json(&wanted, "runs")?, stage, ts(&since), ts(&as_of)],
            |row| {
                let text = |i| row.get::<_, String>(i);
                Ok((text(0)?, text(1)?, text(2)?, text(3)?))
            },
        )
        .ctx("query correction masks")?;
    let mut masks = Vec::new();
    for row in rows {
        let (feedback, run, model, effort) = row.ctx("read correction mask")?;
        assert!(wanted.contains(run.as_str()), "a mask is read by its run");
        masks.push(Mask {
            feedback: FeedbackId::parse(&feedback)?,
            run: PipelineRunId::parse(&run)?,
            choice: ModelChoice {
                model: model.parse()?,
                effort: effort.parse()?,
                tier: None,
            },
        });
    }
    assert!(masks.len() <= wanted.len() * MAX_FEEDBACK_PER_RUN);
    Ok(masks)
}

/// `penalizing` in SQL: a credited quality correction, unrevoked `as_of`
/// and observed within the 90 days before it.
const ACTIVE_MASKS: &str = "SELECT id, run, model, effort FROM operator_feedback
    WHERE run IN (SELECT value FROM json_each(?1)) AND stage = ?2
      AND stage_run IS NOT NULL AND cause = 'quality'
      AND observed_at > ?3 AND observed_at <= ?4
      AND (revoked_at IS NULL OR revoked_at > ?4)
    ORDER BY observed_at, id";

fn by_request(conn: &Connection, key: &str) -> Result<Option<OperatorFeedback>, ArchonError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM operator_feedback WHERE request_key = ?1 AND revoked_at IS NULL",
            [key],
            |row| row.get(0),
        )
        .optional()
        .ctx("read correction by request")?;
    raw.map(|raw| from_json(&raw, "feedback")).transpose()
}

fn load(conn: &Connection, id: &FeedbackId) -> Result<Option<OperatorFeedback>, ArchonError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM operator_feedback WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .ctx("read correction")?;
    raw.map(|raw| from_json(&raw, "feedback")).transpose()
}

fn insert(conn: &Connection, feedback: &OperatorFeedback) -> Result<(), ArchonError> {
    feedback.check()?;
    let choice = match &feedback.attribution.credit {
        Credit::Model { stage_run, choice } => Some((
            stage_run.as_str(),
            choice.model.to_string(),
            choice.effort.to_string(),
        )),
        Credit::Unavailable { .. } => None,
    };
    assert!(
        feedback.penalized().is_none() || choice.is_some(),
        "a penalty names its choice"
    );
    let context = feedback.attribution.context.as_ref();
    conn.execute(
        "INSERT INTO operator_feedback (id, request_key, item, run, stage, stage_run, model,
             effort, repo, pipeline, revision, review, task_class, cause, reason, observed_at,
             revoked_at, aether_instance_id, memory_id, curation, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20, ?21)",
        params![
            feedback.id.as_str(),
            feedback.request_key,
            feedback.item.as_str(),
            feedback.run.as_str(),
            feedback.stage,
            choice.as_ref().map(|c| c.0),
            choice.as_ref().map(|c| &c.1),
            choice.as_ref().map(|c| &c.2),
            feedback.attribution.repo.as_ref().map(RepoId::as_str),
            feedback.attribution.pipeline,
            context.map(|c| &c.revision),
            context.map(|c| &c.review),
            context.map(|c| &c.task_class),
            feedback.cause.as_str(),
            feedback.reason,
            ts(&feedback.observed_at),
            feedback.revoked_at.as_ref().map(ts),
            feedback.curation.as_ref().map(|c| c.instance_id.as_str()),
            feedback.curation.as_ref().map(|c| c.memory.as_str()),
            feedback.curation.as_ref().map(|c| c.state.as_str()),
            to_json(feedback, "feedback")?,
        ],
    )
    .ctx("insert operator correction")?;
    Ok(())
}

/// Writes the parts that change: revocation and curation state.
fn update(conn: &Connection, feedback: &OperatorFeedback) -> Result<(), ArchonError> {
    let changed = conn
        .execute(
            "UPDATE operator_feedback SET revoked_at = ?2, curation = ?3, data = ?4 WHERE id = ?1",
            params![
                feedback.id.as_str(),
                feedback.revoked_at.as_ref().map(ts),
                feedback.curation.as_ref().map(|c| c.state.as_str()),
                to_json(feedback, "feedback")?,
            ],
        )
        .ctx("update operator correction")?;
    assert_eq!(
        changed, 1,
        "an update follows a load of the same correction"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The steps SQLite plans for `sql` against the migrated schema.
    fn plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let args = vec!["[]"; statement.parameter_count()];
        assert!(!args.is_empty(), "a routing read is parameterized");
        statement
            .query_map(rusqlite::params_from_iter(args), |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn routing_reads_corrections_through_their_indexes() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema::migrate(&mut conn).unwrap();
        let reads = [
            (HAS_CONTEXT_FEEDBACK, "operator_feedback_by_cohort"),
            (HAS_FEEDBACK, "operator_feedback_by_cohort"),
            (ACTIVE_CONTEXT_FEEDBACK, "operator_feedback_by_cohort"),
            (ACTIVE_FEEDBACK, "operator_feedback_by_cohort"),
            (ACTIVE_MASKS, "operator_feedback_by_run"),
        ];
        for (sql, index) in reads {
            let steps = plan(&conn, sql);
            let searched = format!("SEARCH operator_feedback USING INDEX {index}");
            assert!(
                steps.iter().any(|step| step.starts_with(&searched)),
                "{sql}: {steps:?}"
            );
            assert!(
                !steps
                    .iter()
                    .any(|step| step.starts_with("SCAN operator_feedback")),
                "never the whole table: {steps:?}"
            );
        }
    }
}
