//! Shipping (ARCHITECTURE §8.7). archond delivers a run itself where its
//! pipeline declares `[pipeline.ship]` ([`deliver`]); a pipeline that still
//! ends in a ship agent has that agent's claim checked here. Either way, PR
//! evidence comes from GitHub, never from an agent's report.

mod abbreviated;
mod auto_merge;
mod checks;
mod deliver;
mod existing;
mod gate;
mod pr_checks;
pub(crate) mod remote;
pub(crate) mod risk;

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::repo::Repo;
use archon_core::run::{PipelineRun, StageOutcome, StageRun, StageState};
use serde_json::Value;

use super::{frozen, recover};
use crate::context::DaemonContext;

pub(crate) use auto_merge::pay as pay_owed_merges;
pub(crate) use auto_merge::withdraw as withdraw_owed_merge;
pub(super) use checks::repairable as repairable_refusal;
pub(super) use deliver::{LOCAL_COMMIT_KEY, base_of, deliver, delivering, queue, replay, stalled};
pub(crate) use deliver::{prune_finished_ships, shipped_commit};
pub(super) use existing::Locks as OntoLocks;
pub use gate::Gates;
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
        Ok(Some((repo, url))) => {
            let pushed = format!("refs/heads/{}", run.branch);
            super::initiative_ship::verify_stack(ctx, run, &pushed).await?;
            super::initiative_ship::record(ctx, run, &url).await?;
            {
                let store = &mut *ctx.store.lock().await;
                super::pull_requests::record_delivery(store, &run.id, &url)?;
            }
            let risk = label_agent_pr(ctx, run, stage, &repo, &url, structured.as_ref()).await?;
            let mut data = structured
                .and_then(|s| s.as_object().cloned())
                .unwrap_or_default();
            data.insert("pr_url".into(), url.into());
            Ok(StageOutcome::Completed {
                summary: format!("{}\n\n{risk}", summary.trim_end()),
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

/// `outcome` with the abbreviated `commit` and `tree` of its ship record
/// replaced by the full ids git resolves in the run worktree, so a short hash
/// as `git commit` prints it ships like the full one.
pub(super) async fn with_full_ids(
    run: &PipelineRun,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    assert!(run.workspace.is_absolute(), "a run has a worktree");
    let StageOutcome::Completed {
        summary,
        structured: Some(mut data),
    } = outcome
    else {
        return Ok(outcome);
    };
    let workspace = run.workspace.clone();
    let data = tokio::task::spawn_blocking(move || {
        abbreviated::resolve(&workspace, &mut data).map(|()| data)
    })
    .await
    .map_err(|e| ArchonError::io("ship record id task", std::io::Error::other(e)))??;
    Ok(StageOutcome::Completed {
        summary,
        structured: Some(data),
    })
}

/// `outcome`, failed when the record stage claims "no change needed" but the
/// run worktree holds one: a claim never bypasses the review of a change.
pub(super) async fn verify_no_change(
    ctx: &DaemonContext,
    run: &PipelineRun,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    let StageOutcome::Completed { structured, .. } = &outcome else {
        return Ok(outcome);
    };
    let Some(reason) = archon_pipeline::ship::no_change_reason(structured.as_ref()) else {
        return Ok(outcome);
    };
    match deliver::prove_unchanged(ctx, run).await? {
        Ok(()) => Ok(outcome),
        Err(why) => Ok(StageOutcome::Failed {
            error: format!(
                "the stage reported no change needed ({reason}), but that is not so: {why}. \
                 Either drop the claim and write the ship record with its review, or undo the change."
            ),
            infra: false,
        }),
    }
}

/// `outcome`, failed when a stage after the record stage ends with the record
/// stage's earlier "no change needed" claim no longer true.
pub(super) async fn verify_standing_no_change(
    ctx: &DaemonContext,
    run: &PipelineRun,
    record_stage: &str,
    stage: &StageRun,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    if !matches!(outcome, StageOutcome::Completed { .. }) {
        return Ok(outcome);
    }
    let claimed = {
        let store = ctx.store.lock().await;
        let latest = store
            .stage_runs(&run.id)?
            .into_iter()
            .filter(|s| s.stage_id == record_stage && s.state == StageState::Completed)
            .filter(|s| s.started_at <= stage.started_at)
            .max_by_key(|s| s.started_at);
        latest.and_then(|s| archon_pipeline::ship::no_change_reason(s.structured.as_ref()))
    };
    let Some(reason) = claimed else {
        return Ok(outcome);
    };
    match deliver::prove_unchanged(ctx, run).await? {
        Ok(()) => Ok(outcome),
        Err(why) => Ok(StageOutcome::Failed {
            error: format!(
                "stage `{record_stage}` reported no change needed ({reason}), but this stage \
                 left the run branch changed: {why}. Review and ship the change, or undo it."
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

/// Label the pull request a ship agent delivered by the `risk` call in its
/// own `structured` result: no reviewer judged it, and without one it is high.
/// Into another item's pull request, its label never goes lower.
async fn label_agent_pr(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    repo: &Repo,
    url: &str,
    structured: Option<&Value>,
) -> Result<String, ArchonError> {
    assert_eq!(stage.run, run.id, "the run's own ship stage");
    let onto = ctx.store.lock().await.item(&run.item)?.onto.is_some();
    let proposal = archon_pipeline::risk::Proposal::read(structured);
    let labelling = risk::Labelling {
        assessment: archon_pipeline::risk::Assessment::settle(
            stage.stage_id.as_str(),
            &proposal,
            None,
        ),
        onto,
        diff: None,
        against: "the base the ship agent chose; see the pull request's files".into(),
        checks: &[],
        review: None,
    };
    risk::label(ctx, repo, Some(&run.item), url, labelling).await
}

/// The repository and open pull request a ship agent's `stage` must have
/// left in `pr` mode; `None` for any other stage and in `local` mode.
async fn agent_pr(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
) -> Result<Option<(Repo, String)>, ArchonError> {
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
    let url = remote::open_pr(gh, &repo, &run.branch).await?;
    Ok(Some((repo, url)))
}
