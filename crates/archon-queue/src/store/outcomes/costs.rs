use super::super::{Store, codec::SqlContext};
use archon_core::{ArchonError, outcome::Outcome};

impl Store {
    /// Spend may arrive after terminal state, and provider billing can lower an estimate.
    pub(super) fn settled_outcome(&self, mut outcome: Outcome) -> Result<Outcome, ArchonError> {
        let stages = self.stage_runs(&outcome.run)?;
        if stages.is_empty() {
            return Ok(outcome);
        }
        let live = super::capture::stage_observations(self, &stages, outcome.count_estimates)?;
        assert!(
            live.iter()
                .all(|s| s.cost_usd.is_finite() && s.cost_usd >= 0.0)
        );
        for stage in &mut outcome.stages {
            if let Some(current) = live
                .iter()
                .find(|s| s.stage == stage.stage && s.choice == stage.choice)
            {
                stage.cost_usd = current.cost_usd;
            }
        }
        outcome.cost_usd = self
            .spend_for_run(&outcome.run, outcome.count_estimates)?
            .max(outcome.stages.iter().map(|s| s.cost_usd).sum());
        let (count, provisional): (u32, u32) = self.conn.query_row(
            "SELECT COUNT(*),COALESCE(SUM(s.cost_basis<>'provider_reported' OR s.cost_usd<>s.settled_cost_usd OR (?2 AND s.estimated_cost_usd>0)),0)
             FROM spend s WHERE s.agent IN (
                 SELECT aether_agent FROM stage_runs WHERE run=?1
                 UNION SELECT agent FROM agent_runs WHERE run=?1)",
            rusqlite::params![outcome.run.as_str(), outcome.count_estimates], |r| Ok((r.get(0)?,r.get(1)?)),
        ).ctx("read outcome settlement")?;
        outcome.cost_provisional = count == 0 || provisional > 0;
        assert!(outcome.check().is_ok());
        Ok(outcome)
    }
}
