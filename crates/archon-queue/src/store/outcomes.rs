//! Durable run observations and corrections; all derived columns share the JSON write.

mod capture;
mod costs;
mod distillation;
mod feedback;
mod reconcile;
mod segments;
pub(super) mod stats;

use archon_core::Timestamp;
use archon_core::config::LearningConfig;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, RepoId};
use archon_core::model_tier::ModelCatalog;
use archon_core::outcome::{Correction, Outcome};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, from_json, to_json, ts};
use super::settings;

impl Store {
    pub fn learning_config(&self) -> Result<LearningConfig, ArchonError> {
        Ok(self.learned().config.clone())
    }

    pub fn configure_learning(&self, config: &LearningConfig) -> Result<(), ArchonError> {
        config.validate()?;
        if !self.learned().config_loaded {
            if self.kv_get("learning/live-since")?.is_none() {
                self.save_learning_value("learning/live-since", &ts(&archon_core::now()))?;
            }
            self.learned().config_loaded = true;
        }
        let mut learned = self.learned();
        if learned.config != *config {
            learned.config = config.clone();
        }
        Ok(())
    }

    pub fn outcome(&self, run: &PipelineRunId) -> Result<Option<Outcome>, ArchonError> {
        load(&self.conn, run)?
            .map(|o| self.settled_outcome(o))
            .transpose()
    }

    pub fn outcomes(&self, repo: Option<&RepoId>) -> Result<Vec<Outcome>, ArchonError> {
        let mut stmt = self.conn.prepare(
            "SELECT data FROM outcomes WHERE (?1 IS NULL OR repo = ?1) ORDER BY finished_at, run"
        ).ctx("prepare outcomes")?;
        let rows = stmt
            .query_map([repo.map(RepoId::as_str)], |r| r.get::<_, String>(0))
            .ctx("query outcomes")?;
        rows.map(|row| self.settled_outcome(from_json(&row.ctx("read outcome")?, "outcome")?))
            .collect()
    }

    /// Succeeded runs in a repository that finished since `since` and whose
    /// pull request is not known to be merged or closed, newest first.
    pub fn outcomes_awaiting_merge(&self, since: &Timestamp) -> Result<Vec<Outcome>, ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT data FROM outcomes WHERE merged IS NULL AND result = 'succeeded'
                 AND repo IS NOT NULL AND finished_at >= ?1 ORDER BY finished_at DESC, run",
            )
            .ctx("prepare outcomes awaiting a merge")?;
        let rows = stmt
            .query_map([ts(since)], |r| r.get::<_, String>(0))
            .ctx("query outcomes awaiting a merge")?;
        let waiting: Vec<Outcome> = rows
            .map(|row| from_json(&row.ctx("read outcome")?, "outcome"))
            .collect::<Result<_, _>>()?;
        assert!(
            waiting
                .iter()
                .all(|o| o.merged.is_none() && o.repo.is_some()),
            "the derived columns agree with the stored outcome"
        );
        Ok(waiting)
    }

    pub fn record_outcome(&self, outcome: &Outcome) -> Result<(), ArchonError> {
        outcome.check()?;
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction().ctx("begin outcome")?;
        check_run(&tx, outcome)?;
        let mut value = outcome.clone();
        if value.context.is_none() {
            value.context = settings::kv_get(&tx, &format!("learning/run-context/{}", value.run))?
                .or(settings::kv_get(
                    &tx,
                    &format!("learning/context/{}", value.item),
                )?)
                .map(|raw| from_json(&raw, "routing context"))
                .transpose()?;
        }
        if let Some(previous) = load(&tx, &value.run)? {
            assert_eq!(previous.item, value.item, "an outcome keeps its item");
            for correction in previous.corrections {
                if !value.corrections.contains(&correction) {
                    value.corrections.push(correction);
                }
            }
            value.diff = value.diff.or(previous.diff);
            value.merged = previous.merged;
            value.feedback = previous.feedback;
        }
        for correction in corrections(&tx, &value.run)? {
            if !value.corrections.contains(&correction) {
                value.corrections.push(correction);
            }
        }
        if let Some(raw) = settings::kv_get(&tx, &format!("learning/merged/{}", value.run))? {
            value.merged = Some(from_json(&raw, "merge observation")?);
        }
        if let Some(raw) = settings::kv_get(&tx, &format!("learning/feedback/{}", value.run))? {
            value.feedback = from_json(&raw, "attributed corrections")?;
        }
        write(&tx, &value)?;
        super::lessons::score_run(&tx, &value)?;
        assert!(
            value.check().is_ok(),
            "only valid observations are committed"
        );
        tx.commit().ctx("commit outcome")
    }

    pub fn record_correction(
        &self,
        run: &PipelineRunId,
        correction: Correction,
    ) -> Result<(), ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction().ctx("begin correction")?;
        correct(&tx, run, correction)?;
        tx.commit().ctx("commit correction")
    }

    pub fn outcome_merged(&self, run: &PipelineRunId, merged: bool) -> Result<(), ArchonError> {
        if !self.learning_config()?.enabled {
            return Ok(());
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("begin merge observation")?;
        super::runs::load(&tx, run)?;
        settings::kv_set(
            &tx,
            &format!("learning/merged/{run}"),
            &to_json(&merged, "merge observation")?,
        )?;
        if !merged {
            correct(&tx, run, Correction::PrClosed)?;
        }
        if let Some(mut outcome) = load(&tx, run)? {
            outcome.merged = Some(merged);
            write(&tx, &outcome)?;
        }
        tx.commit().ctx("commit merge observation")
    }
}

pub(super) fn load(conn: &Connection, run: &PipelineRunId) -> Result<Option<Outcome>, ArchonError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM outcomes WHERE run = ?1",
            [run.as_str()],
            |r| r.get(0),
        )
        .optional()
        .ctx("read outcome")?;
    raw.map(|raw| from_json(&raw, "outcome")).transpose()
}

pub(super) fn corrections(
    conn: &Connection,
    run: &PipelineRunId,
) -> Result<Vec<Correction>, ArchonError> {
    settings::kv_get(conn, &format!("learning/corrections/{run}"))?
        .map_or_else(|| Ok(Vec::new()), |raw| from_json(&raw, "corrections"))
}

pub(super) fn correct(
    conn: &Connection,
    run: &PipelineRunId,
    correction: Correction,
) -> Result<(), ArchonError> {
    super::runs::load(conn, run)?;
    let mut values = corrections(conn, run)?;
    let previous = load(conn, run)?;
    if let Some(outcome) = &previous {
        for recorded in &outcome.corrections {
            if !values.contains(recorded) {
                values.push(*recorded);
            }
        }
    }
    if values.contains(&correction) {
        return Ok(());
    }
    values.push(correction);
    assert!(values.len() <= 6, "correction kinds are deduplicated");
    settings::kv_set(
        conn,
        &format!("learning/corrections/{run}"),
        &to_json(&values, "corrections")?,
    )?;
    if let Some(mut outcome) = previous {
        outcome.corrections = values;
        assert!(!outcome.is_win(), "a correction revokes an uncorrected win");
        write(conn, &outcome)?;
        super::lessons::score_run(conn, &outcome)?;
    }
    Ok(())
}

fn write(conn: &Connection, o: &Outcome) -> Result<(), ArchonError> {
    o.check()?;
    conn.execute("INSERT INTO outcomes (run,item,repo,pipeline,size,result,failure_class,corrected,
        merged,cost_usd,finished_at,updated_at,data) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
        ON CONFLICT(run) DO UPDATE SET corrected=excluded.corrected,merged=excluded.merged,
        size=excluded.size,result=excluded.result,failure_class=excluded.failure_class,
        cost_usd=excluded.cost_usd,updated_at=excluded.updated_at,data=excluded.data",
        params![o.run.as_str(),o.item.as_str(),o.repo.as_ref().map(RepoId::as_str),o.pipeline,
            o.size().map(|s| s.as_str()),o.result.as_str(),o.failure.map(|f| f.as_str()),
            !o.corrections.is_empty(),o.merged,o.cost_usd,ts(&o.finished_at),ts(&archon_core::now()),
            to_json(o,"outcome")?]).ctx("write outcome")?;
    conn.execute("DELETE FROM outcome_stages WHERE run=?1", [o.run.as_str()])
        .ctx("replace outcome stages")?;
    let mut stmt = conn
        .prepare(
            "INSERT INTO outcome_stages
        (run,ordinal,stage_id,model,effort,cost_usd,wall_secs,attempts,passed)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        )
        .ctx("prepare outcome stages")?;
    for (i, s) in o.stages.iter().enumerate() {
        assert!(s.attempts > 0, "an observed stage has attempts");
        stmt.execute(params![
            o.run.as_str(),
            i as i64,
            s.stage,
            s.choice.model.to_string(),
            s.choice.effort.to_string(),
            s.cost_usd,
            super::codec::to_i64(s.wall_secs, "stage wall")?,
            s.attempts,
            s.passed
        ])
        .ctx("write outcome stage")?;
    }
    assert_eq!(
        o.failure.is_some(),
        o.result == archon_core::outcome::OutcomeResult::Failed
    );
    Ok(())
}

impl Store {
    /// Learning snapshots use their own keyspace and can be frozen while callers hold a read handle.
    pub fn save_learning_value(&self, key: &str, value: &str) -> Result<(), ArchonError> {
        if !key.starts_with("learning/") {
            return Err(ArchonError::Invalid(
                "learning key outside namespace".into(),
            ));
        }
        settings::kv_set(&self.conn, key, value)
    }
}

impl Store {
    pub fn outcome_window(
        &self,
        repo: &RepoId,
        offset: usize,
        limit: u32,
    ) -> Result<Vec<Outcome>, ArchonError> {
        if limit == 0 {
            return Err(ArchonError::Invalid(
                "outcome window must be nonempty".into(),
            ));
        }
        let offset = i64::try_from(offset)
            .map_err(|_| ArchonError::Invalid("outcome offset is too large".into()))?;
        let mut stmt=self.conn.prepare("SELECT data FROM outcomes WHERE repo=?1 AND finished_at>=COALESCE((SELECT value FROM kv WHERE key='learning/live-since'),'') AND NOT EXISTS (SELECT 1 FROM kv WHERE key='learning/imported/'||run) ORDER BY finished_at,run LIMIT ?2 OFFSET ?3")
            .ctx("prepare outcome window")?;
        let rows = stmt
            .query_map(params![repo.as_str(), limit, offset], |r| {
                r.get::<_, String>(0)
            })
            .ctx("query outcome window")?;
        let values: Vec<Outcome> = rows
            .map(|r| self.settled_outcome(from_json(&r.ctx("read outcome window")?, "outcome")?))
            .collect::<Result<_, _>>()?;
        assert!(values.len() <= limit as usize);
        assert!(values.iter().all(|o| o.repo.as_ref() == Some(repo)));
        Ok(values)
    }

    pub fn corrections_since(
        &self,
        repo: &RepoId,
        since: &str,
    ) -> Result<Vec<Outcome>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT data FROM outcomes WHERE repo=?1 AND corrected=1 AND updated_at>?2
            AND finished_at>=COALESCE((SELECT value FROM kv WHERE key='learning/live-since'),'')
            AND NOT EXISTS (SELECT 1 FROM kv WHERE key='learning/imported/'||run)
            ORDER BY updated_at DESC,run DESC LIMIT 100",
            )
            .ctx("prepare new corrections")?;
        let rows = stmt
            .query_map(params![repo.as_str(), since], |r| r.get::<_, String>(0))
            .ctx("query new corrections")?;
        let values: Vec<Outcome> = rows
            .map(|r| from_json(&r.ctx("read new corrections")?, "outcome"))
            .collect::<Result<_, _>>()?;
        assert!(values.len() <= 100);
        assert!(values.iter().all(|o| o.repo.as_ref() == Some(repo)));
        Ok(values)
    }
}

fn check_run(conn: &Connection, outcome: &Outcome) -> Result<(), ArchonError> {
    use archon_core::outcome::OutcomeResult;
    use archon_core::run::RunState;
    let run = super::runs::load(conn, &outcome.run)?;
    if run.item != outcome.item
        || run.pipeline != outcome.pipeline
        || run.finished_at != Some(outcome.finished_at)
    {
        return Err(ArchonError::Invalid(
            "outcome identity or finish time differs from its run".into(),
        ));
    }
    let agrees = matches!(
        (&run.state, outcome.result),
        (RunState::Succeeded { .. }, OutcomeResult::Succeeded)
            | (RunState::Failed { .. }, OutcomeResult::Failed)
            | (RunState::Cancelled { .. }, OutcomeResult::Cancelled)
    );
    if !agrees {
        return Err(ArchonError::Invalid(
            "outcome result differs from its finished run".into(),
        ));
    }
    Ok(())
}

impl Store {
    /// The newest 500 comparable outcomes; within `window` when one is given.
    pub(super) fn route_outcomes(
        &self,
        query: &super::model_evidence::RouteQuery<'_>,
        window: Option<(Timestamp, Timestamp)>,
    ) -> Result<Vec<Outcome>, ArchonError> {
        assert!(window.is_none_or(|(since, until)| since <= until));
        let mut stmt = self.conn.prepare(
            "SELECT o.data FROM outcomes o WHERE NOT EXISTS (SELECT 1 FROM run_edits WHERE run=o.run)
             AND NOT EXISTS (SELECT 1 FROM kv WHERE key='learning/reshaped/'||o.run) AND (?1 IS NULL OR o.repo=?1) AND o.pipeline=?2
             AND (?3 IS NULL OR o.size=?3) AND EXISTS
             (SELECT 1 FROM outcome_stages s WHERE s.run=o.run AND s.stage_id=?4)
             AND (?5 IS NULL OR (json_extract(o.data,'$.context.revision')=?5
                 AND json_extract(o.data,'$.context.review')=?6 AND json_extract(o.data,'$.context.task_class')=?7))
             AND (?8 IS NULL OR (o.finished_at>=?8 AND o.finished_at<=?9))
             ORDER BY o.finished_at DESC, o.run DESC LIMIT 500"
        ).ctx("prepare contextual routes")?;
        let context = query.context;
        let rows = stmt
            .query_map(
                params![
                    query.repo.map(RepoId::as_str),
                    query.pipeline,
                    query.size.map(|s| s.as_str()),
                    query.stage,
                    context.map(|c| &c.revision),
                    context.map(|c| &c.review),
                    context.map(|c| &c.task_class),
                    window.map(|(since, _)| ts(&since)),
                    window.map(|(_, until)| ts(&until)),
                ],
                |r| r.get::<_, String>(0),
            )
            .ctx("query contextual routes")?;
        let outcomes: Vec<Outcome> = rows
            .map(|raw| {
                self.settled_outcome(from_json(&raw.ctx("read contextual outcome")?, "outcome")?)
            })
            .collect::<Result<_, _>>()?;
        assert!(outcomes.len() <= 500, "routing reads a bounded history");
        Ok(outcomes)
    }
}

impl Store {
    pub fn learning_models_stale(&self) -> bool {
        self.learned()
            .catalog_at
            .is_none_or(|at| archon_core::now() - at >= chrono::Duration::minutes(5))
    }

    /// Connected models Aether places in no tier.
    #[cfg(feature = "testing")]
    pub fn set_learning_models(&self, models: Vec<archon_core::model::ModelRef>) {
        let untiered = models
            .into_iter()
            .map(|model| archon_core::model_tier::CatalogModel {
                model,
                tier: None,
                is_default: false,
            });
        self.set_model_catalog(ModelCatalog::new(untiered.collect()));
    }

    pub fn set_model_catalog(&self, catalog: ModelCatalog) {
        assert!(
            catalog
                .models()
                .iter()
                .all(|m| !m.model.provider.is_empty() && !m.model.model.is_empty())
        );
        let mut learned = self.learned();
        learned.catalog = catalog;
        learned.catalog_at = Some(archon_core::now());
    }

    /// The connected models, sorted; empty before the first refresh.
    pub fn learning_models(&self) -> Vec<archon_core::model::ModelRef> {
        let learned = self.learned();
        let mut models: Vec<_> = learned.catalog.models().iter().map(|m| m.model.clone()).collect();
        models.sort();
        models
    }

    /// The connected models in Aether's order, with their tiers.
    pub fn model_catalog(&self) -> ModelCatalog {
        self.learned().catalog.clone()
    }
}

impl Store {
    pub fn prune_learning_snapshots(&self) -> Result<usize, ArchonError> {
        let generation = self
            .kv_get(super::keys::ORCHESTRATOR_GENERATION)?
            .unwrap_or_default();
        let current = format!("learning/orchestrator/{generation}");
        self.conn
            .execute(
                "DELETE FROM kv WHERE key LIKE 'learning/orchestrator/%' AND key<>?1
             AND rowid NOT IN (SELECT rowid FROM kv WHERE key LIKE 'learning/orchestrator/%'
                              ORDER BY rowid DESC LIMIT 9)",
                [current],
            )
            .ctx("prune old orchestrator lesson snapshots")
    }
}
