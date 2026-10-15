use super::super::{
    Store,
    codec::{SqlContext, from_json},
};
use archon_core::{
    ArchonError,
    id::RepoId,
    queue::{QueueItem, QueueState},
};

impl Store {
    pub fn distillation_available(
        &self,
        repo: &RepoId,
        daily_budget: Option<f64>,
    ) -> Result<bool, ArchonError> {
        assert!(daily_budget.is_none_or(|b| b.is_finite() && b >= 0.0));
        let active: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM items i WHERE repo=?1 AND state IN ('queued','dispatching','running','blocked')
             AND EXISTS (SELECT 1 FROM json_each(i.data,'$.tags') WHERE value='archon:distillation'))",
            [repo.as_str()], |r| r.get(0),
        ).ctx("check active distillation")?;
        let today = archon_core::now().date_naive().to_string();
        let reserved: f64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(json_extract(i.data,'$.budget_usd')),0) FROM items i
             WHERE substr(created_at,1,10)=?1 AND EXISTS
             (SELECT 1 FROM json_each(i.data,'$.tags') WHERE value='archon:distillation')",
                [today],
                |r| r.get(0),
            )
            .ctx("check daily distillation reservation")?;
        assert!(reserved.is_finite() && reserved >= 0.0);
        Ok(!active && daily_budget.is_none_or(|budget| reserved + 1.0 <= budget))
    }

    pub fn cancel_queued_distillations(&mut self) -> Result<(), ArchonError> {
        let items: Vec<QueueItem> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT data FROM items i WHERE state='queued' AND EXISTS
                 (SELECT 1 FROM json_each(i.data,'$.tags') WHERE value='archon:distillation')",
                )
                .ctx("prepare queued distillations")?;
            stmt.query_map([], |r| r.get::<_, String>(0))
                .ctx("query queued distillations")?
                .map(|r| from_json(&r.ctx("read distillation")?, "distillation item"))
                .collect::<Result<_, _>>()?
        };
        for item in items {
            assert_eq!(item.state, QueueState::Queued);
            assert!(item.tags.iter().any(|t| t == "archon:distillation"));
            self.transition_item(
                &item.id,
                QueueState::Cancelled {
                    reason: Some("Learning disabled".into()),
                },
                archon_core::now(),
            )?;
        }
        Ok(())
    }
}

impl Store {
    pub fn unapplied_distillations(
        &self,
    ) -> Result<Vec<archon_core::run::PipelineRun>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT r.data FROM runs r JOIN items i ON i.id=r.item WHERE r.terminal=1
             AND EXISTS (SELECT 1 FROM json_each(i.data,'$.tags') WHERE value='archon:distillation')
             AND NOT EXISTS (SELECT 1 FROM kv WHERE key='learning/applied/'||r.id)
             ORDER BY r.id LIMIT 20",
            )
            .ctx("prepare unapplied distillations")?;
        stmt.query_map([], |r| r.get::<_, String>(0))
            .ctx("query unapplied distillations")?
            .map(|raw| from_json(&raw.ctx("read unapplied distillation")?, "distillation run"))
            .collect()
    }
}
