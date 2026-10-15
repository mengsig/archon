//! Queue items: enqueue, inspect, update, cancel and retry (§6.1, §7.4).

use std::collections::BTreeMap;

use archon_core::catalog::{StageKind, WorkspaceMode};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{ModelPlan, ModelReasons, check_reason_lengths};
use archon_core::notify::NotificationKind;
use archon_core::protocol::{EnqueueRequest, ItemDetail, ItemPatch, ItemQuery, Reply};
use archon_core::queue::{QueueItem, QueueState, check_enqueue, check_item_budget, check_tags};
use archon_core::repo::Repo;
use archon_pipeline::PipelineSpec;
use archon_queue::dispatch::effective_agent_cap;

use crate::context::DaemonContext;
use crate::resolve::{self, RepoEnv};
use crate::worktree::WorktreeManager;
use crate::{executor, knobs, learned};

pub(super) async fn enqueue(
    ctx: &DaemonContext,
    req: EnqueueRequest,
) -> Result<Reply, ArchonError> {
    let title = req.title.trim().to_owned();
    check_enqueue(&req.title, &req.brief, &req.tags, req.budget_usd)?;
    let (env, resolved) = enqueue_context(ctx, &req).await?;
    let id = QueueItemId::new();
    crate::learned::prepare_routing(ctx, &id, &resolved, &req.tags).await?;
    let plan = filled_plan(
        ctx,
        &id,
        &env,
        &resolved.base,
        &resolved.spec,
        req.model_plan,
    )
    .await?;
    check_plan(ctx, &env, &resolved.spec, &plan)?;
    check_reasons(&resolved.spec, req.model_reasons.as_ref())?;
    let now = archon_core::now();
    let item = QueueItem {
        id,
        repo: env.repo.as_ref().map(|r| r.id.clone()),
        pipeline: resolved.pipeline,
        title,
        brief: req.brief,
        priority: req.priority,
        dependencies: req.depends_on,
        tags: req.tags,
        model_plan: plan,
        model_reasons: req.model_reasons,
        budget_usd: req.budget_usd,
        autonomy: resolved.autonomy,
        origin: req.origin.clone(),
        fresh_retries: 0,
        initiative: None,
        state: QueueState::Queued,
        created_at: now,
        updated_at: now,
    };
    let key = req
        .idempotency_key
        .as_deref()
        .filter(|k| !k.trim().is_empty());
    let stored = ctx
        .store
        .lock()
        .await
        .insert_enqueued(&item, key, &req.autonomy)?;
    assert!(
        stored.id == item.id || key.is_some(),
        "only a key returns an older item"
    );
    ctx.dispatch.notify_one();
    Ok(item_reply(ctx, stored, req.budget_usd.is_some()).await)
}

/// `Reply::Item`, or `ItemBudgetsOff` when the change set a budget that
/// is not enforced while budgets are globally off.
pub(super) async fn item_reply(ctx: &DaemonContext, item: QueueItem, set_budget: bool) -> Reply {
    match set_budget && !resolve::caps(ctx).await.budgets_enabled {
        true => Reply::ItemBudgetsOff(item),
        false => Reply::Item(item),
    }
}

async fn enqueue_context(
    ctx: &DaemonContext,
    req: &EnqueueRequest,
) -> Result<(RepoEnv, knobs::Resolved), ArchonError> {
    let repo = match &req.repo {
        Some(reference) => Some(ctx.store.lock().await.repo(reference)?),
        None => None,
    };
    let env = RepoEnv::new(repo)?;
    if env.repo.is_none() && req.pipeline.is_none() {
        return Err(ArchonError::Invalid(
            "name a pipeline; only repositories have a default one".into(),
        ));
    }
    let resolved = knobs::resolve(&ctx.config, &env, req.pipeline.clone(), &req.autonomy)?;
    check_runs_here(ctx, &env, &resolved.spec).await?;
    assert_eq!(resolved.spec.name(), resolved.pipeline);
    assert!(!resolved.spec.file.stages.is_empty());
    Ok((env, resolved))
}

pub(super) async fn list(ctx: &DaemonContext, query: &ItemQuery) -> Result<Reply, ArchonError> {
    Ok(Reply::Items(ctx.store.lock().await.items(query)?))
}

/// Run costs include stages and descendants, whose ledger costs remain current even
/// after they settle. Persisted finish-time snapshots are not authoritative.
pub(super) async fn get(ctx: &DaemonContext, id: &QueueItemId) -> Result<Reply, ArchonError> {
    let waiting = super::waiting::WaitingView::read(ctx).await?;
    let estimates = ctx.config.limits.count_estimated_cost;
    let store = ctx.store.lock().await;
    let item = store.item(id)?;
    let mut runs = store.runs_for_item(id)?;
    let mut stages = Vec::new();
    let mut delegations = BTreeMap::new();
    for run in &mut runs {
        let mut run_stages = store.stage_runs(&run.id)?;
        for stage in &mut run_stages {
            if let Some(delegation) = super::stages::charge(&store, stage, estimates)? {
                delegations.insert(stage.id.clone(), delegation);
            }
        }
        run.cost_usd = store.spend_for_run(&run.id, estimates)?;
        stages.extend(run_stages);
    }
    let spent_usd = store.spend_for_item(id, estimates)?;
    let gates = store.gates_for_item(id)?;
    assert!(
        runs.iter().all(|r| r.item == item.id),
        "runs belong to their item"
    );
    assert!(
        gates.iter().all(|g| g.item == item.id),
        "gates belong to their item"
    );
    let waiting_on = super::item_views::waiting_on(&store, &item, &runs, &stages)?;
    let waiting_reason = waiting.reason(&store, &item, waiting_on.as_ref())?;
    let steers = executor::steer::records(&store, id)?;
    let switched_to = executor::steer::switched_to(&store, &item)?;
    Ok(Reply::ItemDetail(Box::new(ItemDetail {
        switched_to,
        steers,
        waiting_reason,
        waiting_on,
        item,
        runs,
        stages,
        spent_usd,
        gates,
        delegations,
    })))
}

/// Priority, tags, budget and dependencies change any time before success;
/// the model plan and the brief here only while no run is starting or running.
pub(super) async fn update(
    ctx: &DaemonContext,
    id: &QueueItemId,
    mut patch: ItemPatch,
) -> Result<Reply, ArchonError> {
    check_item_budget(patch.budget_usd)?;
    if let Some(tags) = &patch.tags {
        check_tags(tags)?;
    }
    let current = ctx.store.lock().await.item(id)?;
    if patch.model_plan.is_some() || patch.model_reasons.is_some() {
        let (env, base, spec) = item_pipeline(ctx, &current).await?;
        if let Some(plan) = patch.model_plan.take() {
            let plan = filled_plan(ctx, id, &env, &base, &spec, plan).await?;
            check_plan(ctx, &env, &spec, &plan)?;
            patch.model_plan = Some(plan);
        }
        check_reasons(&spec, patch.model_reasons.as_ref())?;
    }
    let amendment = patch.brief_append.clone();
    let set_budget = patch.budget_usd.is_some();
    let item = {
        // Under the run table, so the dispatcher cannot start the item meanwhile.
        let _table = ctx.runs.lock().await;
        let mut store = ctx.store.lock().await;
        let mut item = store.item(id)?;
        check_patchable(&item, &patch)?;
        apply_patch(&mut item, patch)?;
        item.updated_at = archon_core::now().max(item.updated_at);
        store.update_item(&item)?;
        item
    };
    if let Some(text) = amendment {
        note_amendment(ctx, id, &text, "update_item").await;
    }
    ctx.dispatch.notify_one();
    Ok(item_reply(ctx, item, set_budget).await)
}

/// Refuse a patch `item`'s state does not allow: nothing after success, and
/// no plan or brief while a run is starting, before a steer can reach it.
fn check_patchable(item: &QueueItem, patch: &ItemPatch) -> Result<(), ArchonError> {
    let id = &item.id;
    if matches!(item.state, QueueState::Succeeded { .. }) {
        return Err(ArchonError::InvalidTransition(format!(
            "item {id} already succeeded"
        )));
    }
    let steered = patch.model_plan.is_some() || patch.brief_append.is_some();
    if item.state.is_active() && steered {
        return Err(ArchonError::InvalidTransition(format!(
            "the run of item {id} is just starting; send its new model_plan or brief_append \
             again in a moment, and it is steered into the run"
        )));
    }
    Ok(())
}

fn apply_patch(item: &mut QueueItem, patch: ItemPatch) -> Result<(), ArchonError> {
    // First: an empty amendment is refused before anything changes.
    if let Some(text) = &patch.brief_append {
        item.amend_brief(text, archon_core::now())?;
    }
    if let Some(priority) = patch.priority {
        item.priority = priority;
    }
    if let Some(plan) = patch.model_plan {
        item.model_plan = plan;
        item.model_reasons = patch.model_reasons;
    } else if let Some(reasons) = patch.model_reasons {
        item.model_reasons = Some(reasons);
    }
    if let Some(tags) = patch.tags {
        item.tags = tags;
    }
    if let Some(budget) = patch.budget_usd {
        item.budget_usd = Some(budget);
    }
    for dep in patch.add_depends_on {
        if !item.dependencies.contains(&dep) {
            item.dependencies.push(dep);
        }
    }
    Ok(())
}

pub(super) async fn cancel(
    ctx: &DaemonContext,
    id: &QueueItemId,
    reason: Option<String>,
) -> Result<Reply, ArchonError> {
    let item = executor::cancel_item(ctx, id, reason.as_deref().unwrap_or_default()).await?;
    Ok(Reply::Item(item))
}

/// What a retry changes besides the item's state.
pub(super) struct Retry {
    pub model_plan: Option<ModelPlan>,
    pub model_reasons: Option<ModelReasons>,
    /// Start from the base branch on a new branch, not in the old worktree.
    pub fresh: bool,
    pub brief_append: Option<String>,
}

/// `failed | cancelled → queued` (a new run), optionally with a new plan
/// and an amended brief; items it blocked and nothing else blocks are
/// queued again. A fresh retry first sets the item's run worktree aside,
/// so the new run forks a new branch from the base branch.
pub(super) async fn retry(
    ctx: &DaemonContext,
    id: &QueueItemId,
    retry: Retry,
) -> Result<Reply, ArchonError> {
    let current = ctx.store.lock().await.item(id)?;
    let replaces_plan = retry.model_plan.is_some();
    let (env, base, spec) = item_pipeline(ctx, &current).await?;
    // A 1.0 item names the ship agent its built-in no longer has; the retry stores the repair.
    let mut kept_plan = current.model_plan.clone();
    spec.drop_retired_ship(&mut kept_plan, None);
    let plan = retry.model_plan.unwrap_or(kept_plan);
    let plan = filled_plan(ctx, id, &env, &base, &spec, plan).await?;
    check_plan(ctx, &env, &spec, &plan)?;
    check_reasons(&spec, retry.model_reasons.as_ref())?;
    check_runs_here(ctx, &env, &spec).await?;
    let fresh_in = match retry.fresh {
        true => Some(fresh_repo(&env, &spec)?),
        false => None,
    };
    let (item, fresh_note) = {
        let _table = ctx.runs.lock().await;
        let loaded = ctx.store.lock().await.item(id)?;
        check_retryable(&*ctx.store.lock().await, &loaded)?;
        let mut item = loaded.clone();
        item.model_plan = plan;
        if replaces_plan || retry.model_reasons.is_some() {
            item.model_reasons = retry.model_reasons;
        }
        spec.drop_retired_ship(&mut item.model_plan, item.model_reasons.as_mut());
        if let Some(text) = &retry.brief_append {
            item.amend_brief(text, archon_core::now())?;
        }
        // Git runs under the run table on purpose, so run starts and gc passes wait for this one
        // move; without it a gc pass could remove the worktree mid-move, or a second retry move it.
        let fresh = match &fresh_in {
            Some(repo) => Some(start_fresh(ctx, repo, &mut item).await?),
            None => None,
        };
        let kept = fresh.as_ref().and_then(|f| f.kept.as_deref());
        let mut store = ctx.store.lock().await;
        let queued = queue_again(&mut store, &loaded, item).map_err(|error| match kept {
            Some(kept) => error.with_detail(&format!("the retry was not queued, but: {kept}")),
            None => error,
        })?;
        (queued, fresh.map(|f| f.note))
    };
    if let Some(note) = &fresh_note {
        post_note(ctx, id, note).await;
    }
    if let Some(text) = &retry.brief_append {
        note_amendment(ctx, id, text, "retry_item").await;
    }
    ctx.dispatch.notify_one();
    Ok(Reply::Item(item))
}

/// A finished item whose restack fix, which works on its branch, is not
/// still to run.
fn check_retryable(store: &archon_queue::Store, item: &QueueItem) -> Result<(), ArchonError> {
    if !matches!(
        item.state,
        QueueState::Succeeded { .. } | QueueState::Failed { .. } | QueueState::Cancelled { .. }
    ) {
        return Err(ArchonError::InvalidTransition(format!(
            "only finished items are retried; {} is {}",
            item.id,
            item.state.kind()
        )));
    }
    if store.has_pending_stack_fix(&item.id)? {
        return Err(ArchonError::InvalidTransition(format!(
            "{} has a restack fix still to finish, which works on its branch; retry it once \
             the fix is done, or cancel the fix first",
            item.id
        )));
    }
    Ok(())
}

/// The repository a fresh retry starts again in: only a pipeline that works
/// in a worktree has a branch to start again from the base.
fn fresh_repo(env: &RepoEnv, spec: &PipelineSpec) -> Result<Repo, ArchonError> {
    let workspace = spec.file.pipeline.workspace;
    match (&env.repo, workspace) {
        (Some(repo), WorkspaceMode::Worktree) => Ok(repo.clone()),
        _ => Err(ArchonError::Invalid(format!(
            "pipeline {:?} does not work in a worktree of a repository, so a fresh retry has \
             no base branch to start from; retry it without fresh",
            spec.name()
        ))),
    }
}

/// What a fresh retry did before its item is queued again.
struct FreshStart {
    /// The progress note, posted once the item is queued.
    note: String,
    /// Where the old worktree went, when there was one to set aside.
    kept: Option<String>,
}

/// Move the item's run worktree aside and the item onto its next fresh
/// branch, which its next run forks from the base branch. The caller holds
/// the run table, so no run of the item starts nor gc pass marks it anew.
async fn start_fresh(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &mut QueueItem,
) -> Result<FreshStart, ArchonError> {
    assert!(!item.state.is_active(), "only a stopped item starts fresh");
    if ctx.collecting.contains(&item.id) {
        return Err(ArchonError::Unavailable(format!(
            "worktree gc is collecting the worktree of {} right now; retry it fresh once the \
             pass ends",
            item.id
        )));
    }
    let manager = WorktreeManager::new(
        ctx.paths.worktrees.clone(),
        ctx.config.worktrees.branch_prefix.clone(),
    );
    let old = manager.branch_name(&item.id, &item.title, item.fresh_retries);
    let next = item.fresh_retries.checked_add(1).ok_or_else(|| {
        ArchonError::Invalid(format!("item {} was retried fresh too often", item.id))
    })?;
    let branch = manager.branch_name(&item.id, &item.title, next);
    assert_ne!(old, branch, "a fresh retry names a new branch");
    let (path, name, id, new) = (
        repo.path.clone(),
        repo.name.clone(),
        item.id.clone(),
        branch.clone(),
    );
    let aside = tokio::task::spawn_blocking(move || manager.start_fresh(&path, &name, &id, &new))
        .await
        .map_err(|e| ArchonError::io("fresh retry task", std::io::Error::other(e)))??;
    item.fresh_retries = next;
    let kept = aside.map(|a| a.describe());
    let aside_note = match &kept {
        Some(kept) => format!(
            "{kept}. Worktree gc deletes the set-aside worktree and its git directory, \
             uncommitted work included, `worktrees.gc_after` ({}) after the item's last run.",
            ctx.config.worktrees.gc_after
        ),
        None => String::new(),
    };
    let note = format!(
        "Retrying {} fresh: its next run forks `{branch}` from the base branch. `{old}` and any \
         open pull request from it are left as they are. {aside_note}",
        item.id
    );
    Ok(FreshStart {
        note: note.trim_end().to_owned(),
        kept,
    })
}

/// Store what a retry changed on `loaded` and queue it again, with the
/// dependents it alone blocked. The caller holds the run table.
pub(super) fn queue_again(
    store: &mut archon_queue::Store,
    loaded: &QueueItem,
    mut item: QueueItem,
) -> Result<QueueItem, ArchonError> {
    assert_eq!(loaded.id, item.id, "a retry changes its own item");
    assert_eq!(loaded.state, item.state, "the state changes by transition");
    if item != *loaded {
        item.updated_at = archon_core::now().max(item.updated_at);
        store.update_item(&item)?;
    }
    let item = store.transition_item(&item.id, QueueState::Queued, archon_core::now())?;
    unblock_dependents(store, &item.id)?;
    Ok(item)
}

/// Record an amendment of the item's brief in its feed history.
async fn note_amendment(ctx: &DaemonContext, id: &QueueItemId, text: &str, via: &str) {
    let text = format!("Amended the brief of {id} ({via}): {}", text.trim());
    post_note(ctx, id, &text).await;
}

/// Post `text` to the item's progress. What it reports is done and stored
/// already, and repeating the request would do it twice, so a note that
/// cannot be posted goes to the daemon's log instead of failing it.
pub(super) async fn post_note(ctx: &DaemonContext, id: &QueueItemId, text: &str) {
    assert!(!text.trim().is_empty(), "a note says what was done");
    let posted = executor::notify(ctx, NotificationKind::Progress, text, Some(id), None).await;
    if let Err(error) = posted {
        tracing::error!(item = %id, %error, note = %text, "item note not posted");
    }
}

/// Blocked dependents of `id` whose dependencies are no longer broken.
fn unblock_dependents(
    store: &mut archon_queue::Store,
    id: &QueueItemId,
) -> Result<(), ArchonError> {
    assert_eq!(
        store.item(id)?.state,
        QueueState::Queued,
        "the retried item is queued"
    );
    for dependent in store.dependents(id)? {
        let item = store.item(&dependent)?;
        if !matches!(item.state, QueueState::Blocked { .. }) {
            continue;
        }
        let mut broken = false;
        for dep in &item.dependencies {
            let state = store.item(dep)?.state;
            broken |= matches!(
                state,
                QueueState::Failed { .. } | QueueState::Cancelled { .. }
            );
        }
        if !broken {
            store.transition_item(&dependent, QueueState::Queued, archon_core::now())?;
        }
    }
    Ok(())
}

/// The item's repository environment and the pipeline it would run now: as
/// the catalog has it, and with its knobs' stages and any steered stage list.
async fn item_pipeline(
    ctx: &DaemonContext,
    item: &QueueItem,
) -> Result<(RepoEnv, PipelineSpec, PipelineSpec), ArchonError> {
    let env = RepoEnv::load(ctx, item.repo.as_ref()).await?;
    let (base, spec) = knobs::item_specs(&env, item)?;
    let spec = executor::steer::shaped(ctx, item, spec).await?;
    Ok((env, base, spec))
}

/// `plan` with what it leaves out filled for `env`'s repository.
pub(super) async fn filled_plan(
    ctx: &DaemonContext,
    id: &QueueItemId,
    env: &RepoEnv,
    base: &PipelineSpec,
    spec: &PipelineSpec,
    plan: ModelPlan,
) -> Result<ModelPlan, ArchonError> {
    let policy = env.policy(&ctx.config);
    let fallback = knobs::default_choice(&ctx.config, &policy, spec);
    let store = ctx.store.lock().await;
    store.configure_learning(&ctx.config.learning)?;
    let repo = env.repo.as_ref().map(|repo| &repo.id);
    // What an enqueue naming no model runs with nothing learned: savings are measured against it.
    let unnamed = ModelPlan::default();
    let baseline = knobs::fill_static_stages(base, spec, unnamed, fallback.as_ref())?;
    store.record_static_plan(repo, spec.name(), &baseline)?;
    let learned = learned::plan_for_item(&store, id, repo, spec, &policy, &plan)?;
    let mut plan = plan;
    plan.0.extend(learned.0);
    knobs::fill_static_stages(base, spec, plan, fallback.as_ref())
}

/// Every stage has 1 to `fan_out` choices, no unknown stage is named, and
/// every choice passes the effective model policy.
pub(super) fn check_plan(
    ctx: &DaemonContext,
    env: &RepoEnv,
    spec: &PipelineSpec,
    plan: &ModelPlan,
) -> Result<(), ArchonError> {
    for stage in &spec.file.stages {
        let choices = plan.choices_for(&stage.id).unwrap_or_default();
        if choices.is_empty() || choices.len() > stage.fan_out as usize {
            return Err(ArchonError::Invalid(format!(
                "model_plan must give stage {:?} of {:?} 1 to {} model choice(s), not {}",
                stage.id,
                spec.name(),
                stage.fan_out,
                choices.len()
            )));
        }
    }
    if let Some(extra) = plan.0.keys().find(|id| spec.stage(id).is_none()) {
        let ids: Vec<&str> = spec.file.stages.iter().map(|s| s.id.as_str()).collect();
        return Err(ArchonError::Invalid(format!(
            "model_plan names stage {extra:?}; {:?} has stages {}",
            spec.name(),
            ids.join(", ")
        )));
    }
    env.policy(&ctx.config).check_plan(plan)
}

pub(super) fn check_reasons(
    spec: &PipelineSpec,
    reasons: Option<&ModelReasons>,
) -> Result<(), ArchonError> {
    let Some(reasons) = reasons else {
        return Ok(());
    };
    check_reason_lengths(reasons)?;
    if let Some(extra) = reasons.keys().find(|id| spec.stage(id).is_none()) {
        let ids: Vec<&str> = spec.file.stages.iter().map(|s| s.id.as_str()).collect();
        return Err(ArchonError::Invalid(format!(
            "model_reasons key {extra:?} is not a stage id: keys are the stage ids of pipeline \
             {:?}, not the pipeline name; valid ids: {}",
            spec.name(),
            ids.join(", ")
        )));
    }
    Ok(())
}

/// The pipeline can run in `env`: on a repository when it needs one, and
/// with every team stage within the agent caps. A team stage is admitted
/// whole (ARCHITECTURE §7.3), so a bigger one would wait forever.
pub(super) async fn check_runs_here(
    ctx: &DaemonContext,
    env: &RepoEnv,
    spec: &PipelineSpec,
) -> Result<(), ArchonError> {
    if spec.file.pipeline.requires_repo && env.repo.is_none() {
        return Err(ArchonError::Invalid(format!(
            "pipeline {:?} works on a repository; pass repo",
            spec.name()
        )));
    }
    let caps = resolve::caps(ctx).await;
    let repo_cap = env
        .repo
        .as_ref()
        .map(|_| env.config.limits.max_per_repo.unwrap_or(0));
    let cap = effective_agent_cap(&caps, repo_cap, spec.name());
    let teams = spec
        .file
        .stages
        .iter()
        .filter(|s| s.kind == StageKind::Team);
    for stage in teams {
        assert!(!stage.roles.is_empty(), "a validated team stage has roles");
        let roles = u32::try_from(stage.roles.len()).expect("a team has few roles");
        if cap != 0 && roles > cap {
            return Err(ArchonError::Invalid(format!(
                "team stage {:?} of {:?} runs its {roles} roles at once, but the agent caps \
                 allow at most {cap} here; raise max_parallel_agents, max_per_repo or \
                 max_per_pipeline, or use fewer roles",
                stage.id,
                spec.name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_pipeline::Catalog;

    fn reasons(key: &str) -> ModelReasons {
        [(key.to_owned(), "why".to_owned())].into()
    }

    #[test]
    fn a_pipeline_name_as_a_reasons_key_is_refused_with_the_stage_ids() {
        let direct = Catalog::builtin().get("direct").expect("built-in").clone();
        let message = check_reasons(&direct, Some(&reasons("direct")))
            .unwrap_err()
            .to_string();
        assert!(message.contains("not the pipeline name"), "{message}");
        assert!(message.contains("valid ids: implement"), "{message}");
        assert!(message.contains("\"direct\""), "{message}");
    }

    #[test]
    fn stage_ids_and_no_reasons_are_accepted() {
        let direct = Catalog::builtin().get("direct").expect("built-in").clone();
        assert!(check_reasons(&direct, Some(&reasons("implement"))).is_ok());
        assert!(check_reasons(&direct, None).is_ok());
    }
}
