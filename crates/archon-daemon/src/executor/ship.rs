//! Shipping (ARCHITECTURE §8.7). archond delivers a run itself where its
//! pipeline declares `[pipeline.ship]` ([`deliver`]); a pipeline that still
//! ends in a ship agent has that agent's claim checked here. Either way, PR
//! evidence comes from GitHub, never from an agent's report.

mod auto_merge;
mod checks;
mod deliver;
mod pr_checks;
mod remote;

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use serde_json::Value;

use super::{frozen, recover};
use crate::context::DaemonContext;

pub(crate) use auto_merge::pay as pay_owed_merges;
pub(crate) use auto_merge::withdraw as withdraw_owed_merge;
pub(super) use deliver::{deliver, delivering, queue, replay, stalled};
pub(crate) use deliver::{prune_finished_ships, shipped_commit};
pub(crate) use pr_checks::look as look_at_checks;
pub(super) use remote::{MergeRefusal, auto_merge as pin_auto_merge, merge_method};

/// `outcome` with the pull request GitHub lists for a ship agent's stage, or
/// failed without one; nothing to check when archond ships (`daemon_ships`).
pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    daemon_ships: bool,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    assert_eq!(stage.run, run.id, "a stage is verified with its own run");
    let StageOutcome::Completed {
        summary,
        structured,
    } = outcome
    else {
        return Ok(outcome);
    };
    if run.state.is_terminal() || daemon_ships {
        return Ok(StageOutcome::Completed {
            summary,
            structured: without_pr_url(structured),
        });
    }
    match agent_pr(ctx, run, stage).await {
        Ok(None) => Ok(StageOutcome::Completed {
            summary,
            structured,
        }),
        Err(error) if error.is_store() => Err(error),
        Ok(Some(url)) => {
            let pushed = format!("refs/heads/{}", run.branch);
            super::initiative_ship::verify_stack(ctx, run, &pushed).await?;
            super::initiative_ship::record(ctx, run, &url).await?;
            let store = &mut *ctx.store.lock().await;
            super::pull_requests::record_delivery(store, &run.id, &url)?;
            let mut data = structured
                .and_then(|s| s.as_object().cloned())
                .unwrap_or_default();
            data.insert("pr_url".into(), url.into());
            Ok(StageOutcome::Completed {
                summary,
                structured: Some(data.into()),
            })
        }
        Err(error) => Ok(StageOutcome::Failed {
            error: format!(
                "PR shipping verification failed for pushed branch `{}`: {error}. \
                 Create or reopen its PR, confirm `gh pr list --head {} --state open`, then retry the item.",
                run.branch, run.branch
            ),
            infra: false,
        }),
    }
}

/// `structured` without a pull request URL, which only GitHub vouches for.
fn without_pr_url(mut structured: Option<Value>) -> Option<Value> {
    if let Some(data) = structured.as_mut().and_then(Value::as_object_mut) {
        data.remove("pr_url");
    }
    structured
}

/// The open pull request a ship agent's `stage` must have left in `pr`
/// mode; `None` for any other stage and in `local` mode.
async fn agent_pr(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
) -> Result<Option<String>, ArchonError> {
    let vars = recover::load_run_vars(&*ctx.store.lock().await, &run.id)?;
    match vars.get("ship.mode").map(String::as_str) {
        Some("pr") => {}
        Some("local") => return Ok(None),
        mode => {
            return Err(ArchonError::Protocol(format!(
                "invalid recorded ship mode: {mode:?}"
            )));
        }
    }
    let spec = frozen::thaw(ctx, &run.id, &run.pipeline).await?;
    let ships =
        |s: &archon_pipeline::spec::StageSpec| s.id == stage.stage_id && s.kind == StageKind::Ship;
    if !spec.file.stages.iter().any(ships) {
        return Ok(None);
    }
    let repo = {
        let store = ctx.store.lock().await;
        let Some(repo) = store.item(&run.item)?.repo else {
            return Err(ArchonError::Invalid("the item has no repository".into()));
        };
        store.repo(&crate::resolve::repo_ref(&repo))?
    };
    let gh = &ctx.config.github.binary;
    remote::open_pr(gh, &repo, &run.branch).await.map(Some)
}
