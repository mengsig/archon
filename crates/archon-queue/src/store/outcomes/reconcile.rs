//! Revisit missing terminal outcomes, including runs which finished after an earlier scan.
use super::super::codec::{SqlContext, from_json};
use super::super::{Store, settings};
use archon_core::{ArchonError, run::PipelineRun};

impl Store {
    pub fn backfill_outcomes(&self, limit: usize, estimates: bool) -> Result<bool, ArchonError> {
        if !(1..=100).contains(&limit) {
            return Err(ArchonError::Invalid("backfill batch must be 1..100".into()));
        }
        if !self.learning_config()?.enabled {
            return Ok(true);
        }
        let cursor = self.kv_get("learning/backfill/cursor")?.unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare("SELECT id,data FROM runs WHERE id>?1 ORDER BY id LIMIT ?2")
            .ctx("prepare missing outcomes")?;
        let rows = stmt
            .query_map(rusqlite::params![cursor, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ctx("query missing outcomes")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .ctx("read missing outcomes")?;
        assert!(rows.len() <= limit);
        for (id, raw) in &rows {
            if self
                .kv_get(&format!("learning/backfill/skipped/{id}"))?
                .is_some()
            {
                settings::kv_set(&self.conn, "learning/backfill/cursor", id)?;
                continue;
            }
            if let Err(error) = self.import_outcome(id, raw, estimates) {
                tracing::warn!(run = id, %error, "skipping invalid learning history");
                settings::kv_set(
                    &self.conn,
                    &format!("learning/backfill/skipped/{id}"),
                    &error.to_string(),
                )?;
            }
            settings::kv_set(&self.conn, "learning/backfill/cursor", id)?;
        }
        let complete = rows.len() < limit;
        if complete {
            settings::kv_set(&self.conn, "learning/backfill/done", "1")?;
            settings::kv_set(&self.conn, "learning/backfill/cursor", "")?;
        }
        Ok(complete)
    }

    fn import_outcome(&self, id: &str, raw: &str, estimates: bool) -> Result<(), ArchonError> {
        let run: PipelineRun = from_json(raw, "backfill run")?;
        if run.id.as_str() != id {
            return Err(ArchonError::Store(
                "backfill run identity differs from its row".into(),
            ));
        }
        if !run.state.is_terminal() || super::load(&self.conn, &run.id)?.is_some() {
            return Ok(());
        }
        if run.finished_at.is_none() {
            return Err(ArchonError::Store(
                "terminal backfill run has no finish time".into(),
            ));
        }
        assert_eq!(run.id.as_str(), id);
        assert!(run.state.is_terminal());
        if self
            .kv_get(&format!("learning/distillation/{}", run.item))?
            .is_some()
        {
            return settings::kv_set(
                &self.conn,
                &format!("learning/backfill/skipped/{id}"),
                "distillation",
            );
        }
        self.capture_outcome(&run, None, estimates)?;
        self.save_learning_value(&format!("learning/imported/{id}"), "1")
    }
}
