//! `refresh_item`: bring a finished item's branch up to its base branch again
//! (ARCHITECTURE §13). The base is fetched from `origin`, and an item on the
//! same branch merges it in, never rebasing: its agent resolves conflicts and
//! runs the checks, its review reads the change from the new merge base, and it
//! ships into the pull request the branch already has. The base of an `onto`
//! item is the head of its pull request, which other deliveries move.
//!
//! What needs no agent is done in place: a parent squash- or rebase-merged
//! into the base is merged over (`super::landed`), and an `onto` item whose
//! branch already holds the moved head is frozen at it again.

use std::collections::BTreeMap;
use std::path::Path;

use archon_core::autonomy::{ProposalMode, ReportMode, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{ModelPlan, ModelPlanDraft, ModelReasons};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{PipelineRun, StageOutcome};
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
/// for the same base commit. The item itself is returned, refreshed in
/// place, when that took no agent.
pub(crate) async fn refresh_item(
    ctx: &DaemonContext,
    id: &QueueItemId,
    asked: Asked,
) -> Result<QueueItem, ArchonError> {
    let (owner, run, repo) = target(ctx, id).await?;
    let env = RepoEnv::new(Some(repo.clone()))?;
    let base = match &owner.onto {
        Some(onto) => onto.head_ref.clone(),
        None => base_for(ctx, &owner, &run, &env).await?,
    };
    let catalog = crate::learned::catalog(ctx).await?;
    let mut prepared = refresh_owner(ctx, &owner, &env, &catalog, asked)?;
    let child_head = match repo.ship == ShipMode::Pr && run.shipped {
        true => check_pr_open(ctx, &repo, &run).await?,
        false => None,
    };
    let restacked = match owner.onto {
        Some(_) => false,
        None => {
            let landing = (&repo, &owner, &run);
            super::landed::for_refresh(ctx, landing, &base, child_head.as_deref()).await?
        }
    };
    let fresh = match base_head(ctx, &repo, &base, &run.branch).await? {
        Ok(fresh) => fresh,
        Err(_) if restacked => return refreshed(ctx, &owner, &run, None).await,
        Err(held) if held.ahead && owner.onto.is_some() => {
            return refreshed(ctx, &owner, &run, Some(&held.sha)).await;
        }
        Err(held) => return Err(ArchonError::Invalid(held.refusal)),
    };
    let reviewed = reviewed_tip(ctx, &owner.id, &fresh.from).await?;
    if reviewed {
        prepared.autonomy.review = refresh_review(prepared.autonomy.review, &fresh.conflicts);
    }
    let refresh = Refresh {
        base,
        sha: fresh.sha,
        from: reviewed.then_some(fresh.from),
    };
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
    let review = queued.autonomy.review;
    tracing::info!(item = %id, refresh = %queued.id, base = %refresh.base, sha = %refresh.sha, %review, "refresh queued");
    ctx.dispatch.notify_one();
    Ok(queued)
}

/// `owner` as it stands once refreshed in place, with an `onto` item frozen
/// again at `head`, its pull request's moved head, which its branch holds:
/// its next delivery is a fast-forward from there.
async fn refreshed(
    ctx: &DaemonContext,
    owner: &QueueItem,
    run: &PipelineRun,
    head: Option<&str>,
) -> Result<QueueItem, ArchonError> {
    assert_eq!(run.item, owner.id, "the owner's own run");
    assert!(head.is_none_or(stack::is_commit_id), "a fetched head");
    let _table = ctx.runs.lock().await;
    let mut store = ctx.store.lock().await;
    let mut now = store.item(&owner.id)?;
    let last = store.runs_for_item(&owner.id)?.pop();
    if now.state != owner.state || last.as_ref().map(|r| &r.id) != Some(&run.id) {
        return Err(ArchonError::InvalidTransition(format!(
            "{} changed while its refresh was prepared; ask again",
            owner.id
        )));
    }
    let (Some(head), Some(onto)) = (head, now.onto.as_mut()) else {
        tracing::info!(item = %owner.id, branch = %run.branch, "restacked over merged parents");
        return Ok(now);
    };
    if onto.head_sha != head {
        tracing::info!(item = %owner.id, from = %onto.head_sha, to = %head, "onto head frozen again");
        onto.head_sha = head.to_owned();
        now.updated_at = archon_core::now();
        store.update_item(&now)?;
    }
    Ok(now)
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

/// Each of `spec`'s stages' timeout in a refresh: the `asked` one, else
/// `[limits] refresh_stage_timeout_minutes`, never the owner's.
fn refresh_timeouts(
    limits: &archon_core::config::LimitsConfig,
    spec: &archon_pipeline::PipelineSpec,
    asked: BTreeMap<String, u32>,
) -> BTreeMap<String, u32> {
    let minutes = limits
        .refresh_stage_timeout_minutes
        .min(limits.max_stage_timeout_minutes);
    assert!(minutes >= 1, "validated: a refresh's stages get time");
    let mut timeouts = asked;
    for stage in &spec.file.stages {
        timeouts.entry(stage.id.clone()).or_insert(minutes);
    }
    assert!(
        timeouts.keys().all(|id| spec.stage(id).is_some()),
        "checked: only the pipeline's own stages"
    );
    timeouts
}

/// The owner's pipeline and knobs, proposal and report off, on the plan and
/// stage timeouts `asked` names, else the stage defaults and the refresh
/// timeout: never the owner's own, which were chosen for other work.
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
    item.stage_timeouts = refresh_timeouts(&ctx.config.limits, &spec, asked.stage_timeouts);
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

/// The branch a refresh brings `owner` up to: the repository's base for a
/// stacked item, whose parents all merged or closed, else [`base_of`].
async fn base_for(
    ctx: &DaemonContext,
    owner: &QueueItem,
    run: &PipelineRun,
    env: &RepoEnv,
) -> Result<String, ArchonError> {
    assert_eq!(run.item, owner.id, "the owner's own run");
    let stacked = super::restack::target_base(&*ctx.store.lock().await, &owner.id)?;
    match stacked {
        Some(base) => {
            assert!(is_plain_branch(&base), "a recorded base branch");
            Ok(base)
        }
        None => base_of(ctx, run, env).await,
    }
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
/// it would only open a new one. The open pull request's head, when GitHub gave one.
async fn check_pr_open(
    ctx: &DaemonContext,
    repo: &Repo,
    run: &PipelineRun,
) -> Result<Option<String>, ArchonError> {
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
    let Some(pr) = open.pop() else {
        return Err(ArchonError::Invalid(format!(
            "the pull request `{branch}` was delivered to is not open: it merged or closed, so \
             there is nothing to refresh; enqueue new work instead"
        )));
    };
    assert!(pr.is_open(), "listed open");
    Ok(pr.head_ref_oid)
}

/// Whether `tip`, the branch head a refresh merges into, is the commit a
/// succeeded item on that branch (`owner` or a fix of it) handed in as its
/// ship record, which its review gate held: only then may the refresh's review
/// start there. Anything else is reviewed whole, from the base, as before.
async fn reviewed_tip(
    ctx: &DaemonContext,
    owner: &QueueItemId,
    tip: &str,
) -> Result<bool, ArchonError> {
    assert!(stack::is_commit_id(tip), "a branch head in full");
    let store = ctx.store.lock().await;
    let mut items = store.stack_fixes_of(owner)?;
    items.push(owner.clone());
    for id in &items {
        if !matches!(store.item(id)?.state, QueueState::Succeeded { .. }) {
            continue;
        }
        let Some(run) = store.runs_for_item(id)?.pop() else {
            continue;
        };
        assert_eq!(&run.item, id, "the item's own run");
        if handed_in(&store.stage_runs(&run.id)?).as_deref() == Some(tip) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The commit of the last ship record among a run's completed `stages`.
fn handed_in(stages: &[archon_core::run::StageRun]) -> Option<String> {
    use archon_core::run::StageState;
    let recorded = stages
        .iter()
        .filter(|stage| stage.state == StageState::Completed)
        .filter_map(|stage| {
            let record = archon_pipeline::ShipRecord::read(stage.structured.as_ref()).ok()?;
            Some((stage.started_at, record.commit))
        })
        .max_by_key(|(started, _)| *started);
    recorded
        .map(|(_, commit)| commit)
        .filter(|commit| stack::is_commit_id(commit))
}

/// The review a refresh owes: none when its merge is clean or conflicts only in
/// changelogs and docs, where the review that passed the branch stands; else one
/// reviewer of the resolution, unless the owner's level asks for none.
fn refresh_review(owner: ReviewLevel, conflicts: &[String]) -> ReviewLevel {
    assert!(
        conflicts.iter().all(|path| !path.is_empty()),
        "conflicts are named paths"
    );
    let resolves_code = conflicts.iter().any(|path| !is_prose(path));
    match (owner, resolves_code) {
        (ReviewLevel::None, _) | (_, false) => ReviewLevel::None,
        (ReviewLevel::Standard | ReviewLevel::Strict, true) => ReviewLevel::Standard,
    }
}

/// Whether `path` is prose a refresh resolves with no reviewer: a changelog
/// anywhere, a top-level Markdown file or anything under `docs/`.
pub(super) fn is_prose(path: &str) -> bool {
    assert!(!path.is_empty(), "a named path");
    // A name git would quote is never taken for prose.
    if path.chars().any(char::is_control) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let top_markdown = !path.contains('/') && name.to_ascii_lowercase().ends_with(".md");
    name.to_ascii_uppercase().starts_with("CHANGELOG") || top_markdown || path.starts_with("docs/")
}

/// The base commit a refresh merges, the branch head it merges into, and the
/// paths that merge conflicts on, as `git merge-tree` previews it.
#[derive(Debug)]
struct Fresh {
    sha: String,
    from: String,
    conflicts: Vec<String>,
}

/// A base head the item's branch holds already, and why that leaves nothing to refresh.
#[derive(Debug)]
struct Held {
    sha: String,
    /// The branch has commits of its own beyond `sha`.
    ahead: bool,
    refusal: String,
}

/// The commit of `base` a refresh merges: `origin`'s head, fetched now, in
/// `pr` mode, else the local branch's; [`Held`] when `branch` holds it already.
async fn base_head(
    ctx: &DaemonContext,
    repo: &Repo,
    base: &str,
    branch: &str,
) -> Result<Result<Fresh, Held>, ArchonError> {
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
) -> Result<Result<Fresh, Held>, ArchonError> {
    if !crate::worktree::branch_exists(repo, branch)? {
        return Err(ArchonError::Invalid(format!(
            "the item's branch `{branch}` is gone from the repository"
        )));
    }
    let shadow = Shadow::of(data_dir, repo)?;
    let sha = super::landed::base_head(&shadow, repo, remote, base)?;
    let tip = stack::sha(repo, branch)?;
    if !stack::contains(repo, &sha, &tip)? {
        let conflicts = match stack::merge_tree(&shadow, None, &tip, &sha)? {
            Ok(_) => Vec::new(),
            Err(files) => files.lines().map(str::to_owned).collect(),
        };
        let from = tip;
        return Ok(Ok(Fresh {
            sha,
            from,
            conflicts,
        }));
    }
    let shipped = !remote && stack::contains(repo, &tip, &format!("refs/heads/{base}"))?;
    let why = match shipped {
        true => format!("it already shipped to `{base}` locally, so there is nothing to refresh"),
        false => "there is nothing to refresh".to_owned(),
    };
    let refusal = format!("`{branch}` already holds `{base}` at {}; {why}", &sha[..12]);
    let ahead = tip != sha;
    Ok(Err(Held {
        sha,
        ahead,
        refusal,
    }))
}

/// Why the claimed refresh `item` has nothing to merge at its first start:
/// its branch holds the base commit already. A retry runs whatever an
/// earlier attempt merged through its checks and delivery.
pub(super) async fn nothing_to_merge(
    ctx: &DaemonContext,
    item: &QueueItem,
) -> Result<Option<String>, ArchonError> {
    assert_eq!(item.state, QueueState::Dispatching, "checked once claimed");
    let (fix, refresh, repo) = {
        let store = ctx.store.lock().await;
        let Some(mut fix) = store.stack_fix(&item.id)? else {
            return Ok(None);
        };
        let (Some(refresh), Some(repo)) = (fix.refresh.take(), &item.repo) else {
            return Ok(None);
        };
        if !store.runs_for_item(&item.id)?.is_empty() {
            return Ok(None);
        }
        (fix, refresh, store.repo(&crate::resolve::repo_ref(repo))?)
    };
    assert!(stack::is_commit_id(&refresh.sha), "a fetched base commit");
    let (path, branch, sha) = (repo.path.clone(), fix.branch.clone(), refresh.sha.clone());
    let held = tokio::task::spawn_blocking(move || -> Result<bool, ArchonError> {
        let at = format!("refs/heads/{branch}");
        Ok(crate::worktree::branch_exists(&path, &branch)? && stack::contains(&path, &sha, &at)?)
    })
    .await
    .map_err(|e| ArchonError::io("refresh merge check task", std::io::Error::other(e)))??;
    Ok(held.then(|| {
        format!(
            "nothing to merge: `{}` already holds `{}` at {}, so the refresh finished at once \
             with no checks and no reviewer",
            fix.branch,
            refresh.base,
            &refresh.sha[..12]
        )
    }))
}

/// `outcome`, failed when the record stage of a refresh whose review was
/// dropped, its merge previewed clean or conflicting only in prose, hands in a
/// branch that changes code beyond what git merged by itself: none reviewed it.
pub(super) async fn verify_unreviewed(
    ctx: &DaemonContext,
    run: &PipelineRun,
    owed: super::review::Owed,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    let completed = matches!(outcome, StageOutcome::Completed { .. });
    if !completed || !owed.records_ship || owed.reviewers > 0 {
        return Ok(outcome);
    }
    let Some((from, sha)) = dropped_review(ctx, run).await? else {
        return Ok(outcome);
    };
    let workspace = run.workspace.clone();
    let (start, merged) = (from.clone(), sha.clone());
    let authored = tokio::task::spawn_blocking(move || authored_paths(&workspace, &start, &merged))
        .await
        .map_err(|e| ArchonError::io("refresh scope task", std::io::Error::other(e)))??;
    let code: Vec<&str> = authored
        .iter()
        .map(String::as_str)
        .filter(|path| !is_prose(path))
        .collect();
    if code.is_empty() {
        return Ok(outcome);
    }
    let shown = code
        .iter()
        .take(MAX_SHOWN_PATHS)
        .copied()
        .collect::<Vec<_>>();
    Ok(StageOutcome::Failed {
        error: format!(
            "this refresh owes no reviewer, as its merge of {} needed no code resolved, but the \
             branch since {} changes code beyond that merge: {}. A refresh changes nothing \
             beyond its merge: undo those changes, or fail and name what the merge breaks so \
             it is queued as follow-up work",
            &sha[..12],
            &from[..12],
            shown.join(", ")
        ),
        infra: false,
    })
}

/// Paths a refusal names at most.
const MAX_SHOWN_PATHS: usize = 8;

/// The branch head a refresh of `run`'s item merged into and the base commit
/// it merged, when its review was dropped below its owner's.
async fn dropped_review(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<Option<(String, String)>, ArchonError> {
    let store = ctx.store.lock().await;
    let Some(fix) = store.stack_fix(&run.item)? else {
        return Ok(None);
    };
    let Some(Refresh {
        sha,
        from: Some(from),
        ..
    }) = fix.refresh
    else {
        return Ok(None);
    };
    let level = store.item(&run.item)?.autonomy.review;
    let owner = store.item(&fix.owner)?.autonomy.review;
    if level != ReviewLevel::None || owner == ReviewLevel::None {
        return Ok(None);
    }
    assert!(stack::is_commit_id(&from), "a recorded branch head");
    assert!(stack::is_commit_id(&sha), "a fetched base commit");
    Ok(Some((from, sha)))
}

/// What the branch in `dir` changed itself since `from` beyond merging `sha`:
/// each merge's resolution (`--remerge-diff`) and every other commit's paths,
/// side branches merged in included.
fn authored_paths(dir: &Path, from: &str, sha: &str) -> Result<Vec<String>, ArchonError> {
    assert!(
        stack::is_commit_id(from) && stack::is_commit_id(sha),
        "commit ids"
    );
    let (not_from, not_sha) = (format!("^{from}"), format!("^{sha}"));
    let args = [
        "log",
        "--diff-merges=remerge",
        "--name-only",
        "-z",
        "--format=",
        "HEAD",
        not_from.as_str(),
        not_sha.as_str(),
    ];
    let ran = crate::worktree::daemon_git::DaemonGit::sealed_in(dir)
        .args(args)
        .output()?;
    if !ran.status.success() {
        return Err(ran.failure());
    }
    let printed = String::from_utf8_lossy(&ran.stdout);
    let mut paths: Vec<String> = printed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changelogs_top_level_markdown_and_docs_are_prose() {
        for prose in [
            "CHANGELOG.md",
            "crates/x/CHANGELOG.md",
            "changelog",
            "README.md",
            "docs/guide.md",
            "docs/img/flow.svg",
        ] {
            assert!(is_prose(prose), "{prose}");
        }
        for code in [
            "src/main.rs",
            "crates/x/README.md",
            "Cargo.toml",
            "docsy/a.rs",
            " docs/a.rs",
            "\nREADME.md",
        ] {
            assert!(!is_prose(code), "{code}");
        }
    }

    #[test]
    fn only_a_code_conflict_owes_one_reviewer() {
        let paths = |list: &[&str]| list.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        let clean = paths(&[]);
        let prose = paths(&["CHANGELOG.md", "docs/a.md"]);
        let code = paths(&["CHANGELOG.md", "src/lib.rs"]);
        for owner in [ReviewLevel::Standard, ReviewLevel::Strict] {
            assert_eq!(refresh_review(owner, &clean), ReviewLevel::None);
            assert_eq!(refresh_review(owner, &prose), ReviewLevel::None);
            assert_eq!(refresh_review(owner, &code), ReviewLevel::Standard);
        }
        assert_eq!(refresh_review(ReviewLevel::None, &code), ReviewLevel::None);
    }
}
