//! `refresh_item`: bring a finished item's branch up to its base branch again
//! (ARCHITECTURE §13). The base is fetched from `origin`, and an item on the
//! same branch merges it in, never rebasing: its agent resolves conflicts and
//! runs the checks, its review reads the change from the new merge base, and it
//! ships into the pull request the branch already has. The base of an `onto`
//! item is the head of its pull request, which other deliveries move.

use std::collections::BTreeMap;
use std::path::Path;

use archon_core::autonomy::{ProposalMode, ReportMode};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{ModelPlan, ModelPlanDraft, ModelReasons};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::PipelineRun;
use archon_queue::store::stacks::Refresh;

use super::dependencies::is_plain_branch;
use super::{gh, recover};
use crate::context::DaemonContext;
use crate::knobs;
use crate::resolve::RepoEnv;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// Queue a refresh of the finished item `id` onto its base branch (an `onto`
/// item: its pull request's head branch), or return the one still to finish
/// for the same base commit.
pub(crate) async fn refresh_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
    asked: Asked,
) -> Result<QueueItem, ArchonError> {
    let (owner, run, repo) = target(ctx, id).await?;
    let env = RepoEnv::new(Some(repo.clone()))?;
    let base = match &owner.onto {
        Some(onto) => onto.head_ref.clone(),
        None => base_of(ctx, &run, &env).await?,
    };
    let catalog = crate::learned::catalog(ctx).await?;
    let prepared = refresh_owner(ctx, &owner, &env, &catalog, asked)?;
    if repo.ship == ShipMode::Pr && run.shipped {
        check_pr_open(ctx, &repo, &run).await?;
    }
    let sha = base_head(ctx, &repo, &base, &run.branch).await?;
    let refresh = Refresh { base, sha };
    let queued = {
        let _table = ctx.runs.lock().await;
        let mut store = ctx.store.lock().await;
        let now = store.item(id)?;
        let last = store.runs_for_item(id)?.pop();
        if now.state != owner.state || last.as_ref().map(|r| &r.id) != Some(&run.id) {
            return Err(ArchonError::InvalidTransition(format!(
                "{id} changed while its refresh was prepared; ask again"
            )));
        }
        store.enqueue_refresh(&prepared, &run, &refresh)?
    };
    assert!(!queued.state.is_terminal(), "a refresh still to run");
    tracing::info!(item = %id, refresh = %queued.id, base = %refresh.base, sha = %refresh.sha, "refresh queued");
    ctx.dispatch.notify_one();
    Ok(queued)
}

/// The item, its last run and its repository, when that run left a branch of
/// a finished item that is no fix itself and is stacked on nothing unmerged.
async fn target(
    ctx: &DaemonContext,
    id: &QueueItemId,
) -> Result<(QueueItem, PipelineRun, Repo), ArchonError> {
    let store = ctx.store.lock().await;
    let item = store.item(id)?;
    if let Some(fix) = store.stack_fix(id)? {
        return Err(ArchonError::Invalid(format!(
            "{id} works on the branch of {}; refresh that item instead",
            fix.owner
        )));
    }
    if !matches!(
        item.state,
        QueueState::Succeeded { .. } | QueueState::Failed { .. } | QueueState::Cancelled { .. }
    ) {
        return Err(ArchonError::InvalidTransition(format!(
            "only a finished item is refreshed; {id} is {}",
            item.state.kind()
        )));
    }
    let Some(repo) = &item.repo else {
        return Err(ArchonError::Invalid(format!("{id} has no repository")));
    };
    let repo = store.repo(&crate::resolve::repo_ref(repo))?;
    let run = store.runs_for_item(id)?.pop();
    let Some(run) = run.filter(|r| is_plain_branch(&r.branch)) else {
        return Err(ArchonError::Invalid(format!(
            "{id} has no run branch to refresh"
        )));
    };
    if let Some(parent) = store.stack_parents(id)?.iter().find(|p| p.is_open()) {
        return Err(ArchonError::Invalid(format!(
            "{id} is stacked on `{}`, which restacking merges in as it moves",
            parent.parent_branch
        )));
    }
    if let (Some(onto), ShipMode::Pr, true) = (&item.onto, repo.ship, run.shipped) {
        return Err(ArchonError::Invalid(format!(
            "{id} went into the pull request {}, not a branch of its own: queue the \
             refresh `onto` {id} instead",
            onto.pr.url()
        )));
    }
    assert_eq!(run.item, item.id, "the item's own run");
    Ok((item, run, repo))
}

/// What a refresh is asked to run with; the stage defaults where unset.
#[derive(Debug, Default)]
pub(crate) struct Asked {
    pub model_plan: Option<ModelPlanDraft>,
    pub model_reasons: Option<ModelReasons>,
    pub stage_timeouts: BTreeMap<String, u32>,
}

/// The owner's pipeline and knobs, proposal and report off, on the plan and
/// stage timeouts `asked` names, else the stage defaults: never the owner's
/// own, which were chosen for other work.
fn refresh_owner(
    ctx: &DaemonContext,
    owner: &QueueItem,
    env: &RepoEnv,
    catalog: &archon_core::model_tier::ModelCatalog,
    asked: Asked,
) -> Result<QueueItem, ArchonError> {
    let mut item = owner.clone();
    item.autonomy.proposal = ProposalMode::Off;
    item.autonomy.report = ReportMode::Off;
    let (base, spec) = knobs::item_specs(env, &item)?;
    knobs::check_ceiling(&env.autonomy(ctx), env, &base, item.autonomy)?;
    crate::handlers::check_reasons(&spec, asked.model_reasons.as_ref())?;
    crate::handlers::check_stage_timeouts(ctx, &spec, &asked.stage_timeouts)?;
    let policy = env.policy(&ctx.config);
    let weights = env.provider_weights(&ctx.config);
    let quota = crate::quota::view(ctx);
    let route = knobs::ItemRoute::new(owner).by(weights.as_ref(), quota.as_ref());
    let tiers = knobs::TierScope::new(&ctx.config, &policy, catalog).routed(route);
    let max_tier = env.max_default_tier(&ctx.config);
    let fallback = knobs::default_choice(&ctx.config, tiers, max_tier, &spec);
    let given = match asked.model_plan {
        Some(draft) => knobs::resolve_plan(&spec, draft, asked.model_reasons.as_ref(), tiers)?,
        None => ModelPlan::default(),
    };
    let plan = knobs::fill_stage_tiers(tiers, max_tier, &spec, given);
    item.model_plan = knobs::fill_static_stages(&base, &spec, plan, fallback.as_ref())?;
    item.model_reasons = asked.model_reasons;
    item.stage_timeouts = asked.stage_timeouts;
    for stage in &spec.file.stages {
        if item
            .model_plan
            .choices_for(&stage.id)
            .is_none_or(|choices| choices.is_empty())
        {
            return Err(ArchonError::Invalid(format!(
                "no model allowed for refresh stage {:?}",
                stage.id
            )));
        }
    }
    crate::handlers::check_plan(ctx, env, &spec, &item.model_plan)?;
    Ok(item)
}

/// Retained fork metadata survives template expiry; old runs use the configured
/// repository base when neither the metadata nor their templates remain.
async fn base_of(
    ctx: &DaemonContext,
    run: &PipelineRun,
    env: &RepoEnv,
) -> Result<String, ArchonError> {
    let store = ctx.store.lock().await;
    let base = match recover::forked_base(&store, &run.id)? {
        Some(base) => base,
        None => env
            .repo
            .as_ref()
            .and_then(|repo| repo.base_branch.clone())
            .or_else(|| env.config.repo.base_branch.clone())
            .unwrap_or_default(),
    };
    let base = base.trim();
    if stack::is_commit_id(base) || !is_plain_branch(base) {
        return Err(ArchonError::Invalid(format!(
            "the item's run forked from {base:?}, not a base branch; set the repository's base \
             branch with `archon repo set`"
        )));
    }
    Ok(base.to_owned())
}

/// Refuse a refresh of a delivered branch whose pull request merged or closed:
/// it would only open a new one.
async fn check_pr_open(
    ctx: &DaemonContext,
    repo: &Repo,
    run: &PipelineRun,
) -> Result<(), ArchonError> {
    let branch = &run.branch;
    let (own, settled) = {
        let store = ctx.store.lock().await;
        let own = store.run_pull_request(&run.id)?.map(|recorded| recorded.pr);
        let settled = own.as_ref().map(|pr| gh::settled(&store, pr)).transpose()?;
        (own, settled.flatten())
    };
    let mut open = match settled {
        Some(_) => Vec::new(),
        None => gh::of_branch(&ctx.config.github.binary, repo, branch, true).await?,
    };
    // A newer pull request of the branch is not the one the run delivered to.
    open.retain(|pr| own.as_ref().is_none_or(|own| pr.identity() == *own));
    if open.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "the pull request `{branch}` was delivered to is not open: it merged or closed, so \
             there is nothing to refresh; enqueue new work instead"
        )));
    }
    Ok(())
}

/// The commit of `base` a refresh merges: `origin`'s head, fetched now, in
/// `pr` mode, else the local branch's. Refused when `branch` holds it already.
async fn base_head(
    ctx: &DaemonContext,
    repo: &Repo,
    base: &str,
    branch: &str,
) -> Result<String, ArchonError> {
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let (base, branch) = (base.to_owned(), branch.to_owned());
    let remote = repo.ship == ShipMode::Pr;
    tokio::task::spawn_blocking(move || head_now(&data_dir, &path, remote, &base, &branch))
        .await
        .map_err(|e| ArchonError::io("refresh base task", std::io::Error::other(e)))?
}

fn head_now(
    data_dir: &Path,
    repo: &Path,
    remote: bool,
    base: &str,
    branch: &str,
) -> Result<String, ArchonError> {
    if !crate::worktree::branch_exists(repo, branch)? {
        return Err(ArchonError::Invalid(format!(
            "the item's branch `{branch}` is gone from the repository"
        )));
    }
    let sha = match remote {
        true => stack::fetch_head(&Shadow::of(data_dir, repo)?, repo, base)?.ok_or_else(|| {
            ArchonError::Invalid("the repository has no `origin` to fetch the base from".into())
        })?,
        false => stack::sha(repo, base)?,
    };
    if stack::contains(repo, &sha, &format!("refs/heads/{branch}"))? {
        let tip = stack::sha(repo, branch)?;
        let shipped = !remote && stack::contains(repo, &tip, &format!("refs/heads/{base}"))?;
        let why = match shipped {
            true => {
                format!("it already shipped to `{base}` locally, so there is nothing to refresh")
            }
            false => "there is nothing to refresh".to_owned(),
        };
        return Err(ArchonError::Invalid(format!(
            "`{branch}` already holds `{base}` at {}; {why}",
            &sha[..12]
        )));
    }
    Ok(sha)
}
