use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::protocol::Reply;
use archon_core::run::StageOutcome;

use super::{RecordCheck, Submitted, check, contract, held_stage};
use crate::context::DaemonContext;
use crate::executor::{refresh, review, ship};

impl RecordCheck {
    pub(super) async fn check(
        &self,
        ctx: &DaemonContext,
        agent: &AetherAgentId,
        sent: &Submitted<'_>,
    ) -> Result<Reply, ArchonError> {
        self.verify(ctx, sent).await?;
        let table = ctx.runs.lock().await;
        let mut store = ctx.store.lock().await;
        let Some(stage) = held_stage(&store, agent)?.filter(|stage| stage.id == self.stage.id)
        else {
            return Err(ArchonError::Forbidden(
                "the stage ended or was replaced during precheck".into(),
            ));
        };
        if contract(&table, &stage)? != self.contract {
            return Err(ArchonError::Unavailable(
                "the stage contract changed during precheck; submit again".into(),
            ));
        }
        if self.contract.verdict {
            return check(&mut store, &stage, sent).map(Reply::VerdictCheck);
        }
        Ok(Reply::Ok)
    }

    async fn verify(&self, ctx: &DaemonContext, sent: &Submitted<'_>) -> Result<(), ArchonError> {
        let owed = self.contract.owed;
        assert!(owed.requires_records(), "only owed records are verified");
        assert_eq!(self.stage.run, self.run.id, "verify in the stage's own run");
        if sent.structured.is_none() {
            return archon_pipeline::check_records(owed, None).map_err(ArchonError::Invalid);
        }
        let mut outcome = StageOutcome::Completed {
            summary: sent.summary.to_owned(),
            structured: sent.structured.cloned(),
        };
        if owed.ship_record {
            outcome = ship::verify_record(ctx, &self.run, outcome).await?;
        }
        let outcome = refresh::verify_unreviewed(ctx, &self.run, owed, outcome).await?;
        let (outcome, _) = review::verify(ctx, &self.run, &self.stage, owed, outcome).await?;
        match outcome {
            StageOutcome::Completed { structured, .. } => {
                archon_pipeline::check_records(owed, structured.as_ref())
                    .map_err(ArchonError::Invalid)
            }
            StageOutcome::Failed { error, .. } => Err(ArchonError::Invalid(error)),
            other => unreachable!("record verification cannot produce {other:?}"),
        }
    }
}
