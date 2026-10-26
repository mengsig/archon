//! Freeze stage requests before creation and replay them under the same key.
//! Unsupported lean context may fall back to full before an agent is recorded.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use archon_aether::wire;
use archon_core::Timestamp;
use archon_core::aether_ids::{AetherAgentId, AetherRunId};
use archon_core::autonomy::ToolsMode;
use archon_core::catalog::{StageKind, WorkspaceMode};
use archon_core::config::{BuildCache, Config};
use archon_core::cost::ItemBudget;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::lesson::CONTRACT_LESSONS;
use archon_core::policy::ModelPolicy;
use archon_core::ports::{SpawnRequest, SpawnedAgent};
use archon_core::queue::QueueItem;
use archon_core::repo::affected::{Affected, selects};
use archon_core::repo::{Repo, RepoTiers};
use archon_core::run::{PipelineRun, StageOutcome, StageRun, StageState};
use archon_pipeline::contract;
use archon_pipeline::spec::MAX_TIMEOUT_MINUTES;
use archon_pipeline::{PipelineSpec, SpawnPlan};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::resume::Takeover;
use super::{
    FollowUp, RunTable, dependencies, descendants, fast_check, finish, naming, resume, run_node,
    settle, team,
};
use crate::context::DaemonContext;
use crate::disk::{self, FullDisk};
use crate::experiments;
use crate::learned;
use crate::resolve::{self, RepoEnv};

const REQUEST_KEY_PREFIX: &str = "spawn_request:";
/// `Protocol`, `Io` or Aether `internal` failures of one `create_agent`
/// before the stage fails.
const MAX_TRANSPORT_ERRORS: u32 = 5;
/// Failures of one `create_agent` on a full disk before the stage and its
/// run fail. Each waits out a disk-full hold, so they are minutes apart;
/// the cap ends the loop when the full disk is one Archon cannot see.
const MAX_DISK_FULL_SPAWNS: u32 = 5;

/// How a stage whose workspace is gone fails, before the path.
pub(super) const WORKSPACE_MISSING: &str = "workspace missing:";

/// The persisted spawn of one stage run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredSpawn {
    pub request: SpawnRequest,
    pub timeout_minutes: u32,
    #[serde(default)]
    pub parent: ParentBinding,
    /// A team slot's worker; its worktree is `request.workspace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<StoredWorker>,
    /// The agent to resume instead of sending `request`; cleared once only
    /// a fresh agent will do (§6.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<KeptAgent>,
    /// The slot's kept agent that cannot take this stage run: stopped before
    /// `request` is sent, then cleared (§6.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<AetherAgentId>,
}

/// The agent kept from the slot's previous attempt, and what it is told to
/// make it fix its own work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptAgent {
    pub agent: AetherAgentId,
    pub message: String,
    /// The summary of a final message that left out the ship record, kept
    /// once the agent was asked for the record (§6.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_record: Option<String>,
}

/// The branch a team slot's worker worktree must have checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredWorker {
    pub role: String,
    pub branch: String,
}

/// Whether `request.parent` is final. Aether fingerprints the whole
/// `create_agent` frame under its key, so once a request may have reached
/// Aether its parent can never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParentBinding {
    /// The parent in the request is what Aether was, or will be, sent. A
    /// record from before run nodes existed has no field and is bound to
    /// no parent: Aether may already hold that stage agent as a root.
    #[default]
    Bound,
    /// Reserved and never issued: bound to the run's node at first issue.
    Pending,
}

fn request_key(id: &StageRunId) -> String {
    format!("{REQUEST_KEY_PREFIX}{id}")
}

pub(crate) fn load_spawn(store: &Store, id: &StageRunId) -> Result<StoredSpawn, ArchonError> {
    let raw = store
        .kv_get(&request_key(id))?
        .ok_or_else(|| ArchonError::NotFound(format!("spawn request of stage run {id}")))?;
    let stored: StoredSpawn = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("spawn request of stage run {id}: {e}")))?;
    assert_eq!(
        stored.request.idempotency_key,
        id.as_str(),
        "spawn request stored under another stage run"
    );
    Ok(stored)
}

pub(super) fn save_spawn(
    store: &mut Store,
    id: &StageRunId,
    spawn: &StoredSpawn,
) -> Result<(), ArchonError> {
    assert_eq!(spawn.request.idempotency_key, id.as_str());
    store.kv_set(&request_key(id), &serde_json::to_string(spawn)?)
}

fn record_host_context(
    store: &mut Store,
    id: &StageRunId,
    host_context: archon_core::config::HostContext,
) -> Result<(), ArchonError> {
    let mut stage = store.stage_run(id)?;
    assert_eq!(&stage.id, id, "the stored stage matches its key");
    if stage.host_context == host_context {
        return Ok(());
    }
    if stage.aether_agent.is_some() {
        return Err(ArchonError::Invalid(format!(
            "stage run {id} cannot change its agent's host context"
        )));
    }
    stage.host_context = host_context;
    store.upsert_stage_run(&stage)
}

/// Delete the stored requests nothing reads any more: of settled stage runs
/// (a kept agent's once its run's node retired) and of ones never recorded.
pub(crate) fn prune_settled_requests(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(REQUEST_KEY_PREFIX)? {
        let raw = key
            .strip_prefix(REQUEST_KEY_PREFIX)
            .expect("ranged by prefix");
        let id = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("spawn request key {key}: {e}")))?;
        let settled = match store.stage_run(&id) {
            Ok(stage) => stage.state.is_terminal() && !resume::request_in_use(store, &stage)?,
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if settled {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Whether no `create_agent` was ever sent for stage run `id`: its spawn is
/// not bound yet, or missing, which `claim` settles without Aether.
pub(super) fn never_sent(store: &Store, id: &StageRunId) -> Result<bool, ArchonError> {
    match load_spawn(store, id) {
        Ok(spawn) => {
            let pending = spawn.parent == ParentBinding::Pending;
            assert!(
                !pending || spawn.request.parent.is_none(),
                "an unbound spawn names no node"
            );
            Ok(pending)
        }
        Err(ArchonError::NotFound(_)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// The Aether run each `spawning` stage's agent joins, for the stages
/// whose create may have reached Aether already; `None` where that run is
/// not known yet. A stage not yet bound to its node was never sent.
pub(crate) fn runs_awaiting_spawn(store: &Store) -> Result<Vec<Option<AetherRunId>>, ArchonError> {
    let mut runs = Vec::new();
    for stage in store.live_stage_runs()? {
        if stage.state != StageState::Spawning {
            continue;
        }
        let spawn = match load_spawn(store, &stage.id) {
            Ok(spawn) => spawn,
            // `claim` fails such a stage before anything is sent.
            Err(ArchonError::NotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        if spawn.parent == ParentBinding::Pending {
            assert!(spawn.request.parent.is_none(), "unbound yet names a node");
            continue;
        }
        let joins = match &spawn.request.parent {
            // Its node's run, recorded with the node before it was bound.
            Some(_) => store.run(&stage.run)?.aether_run,
            // A root from before run nodes: the run it attached to, if any.
            None => spawn.request.attach_run.clone(),
        };
        runs.push(joins);
    }
    Ok(runs)
}

/// The stage deadline: exactly `timeout_minutes` after `start`. Pipelines
/// load only with `1..=MAX_TIMEOUT_MINUTES`, so nothing is clamped here.
pub(crate) fn deadline(start: Timestamp, timeout_minutes: u32) -> Timestamp {
    assert!(
        (1..=MAX_TIMEOUT_MINUTES).contains(&timeout_minutes),
        "pipeline validation bounds timeout_minutes, got {timeout_minutes}"
    );
    let timeout = chrono::Duration::minutes(i64::from(timeout_minutes));
    let deadline = start
        .checked_add_signed(timeout)
        .expect("30 days past a stage's start is a representable time");
    debug_assert!(deadline > start);
    deadline
}

/// The item's budget (its own, else the per-item cap) and its spend;
/// `None` when neither is set or budgets are switched off.
pub(crate) fn item_budget(
    ctx: &DaemonContext,
    store: &Store,
    item: &QueueItem,
    estimates: bool,
) -> Result<Option<ItemBudget>, ArchonError> {
    let caps = resolve::item_caps(ctx, item.repo.as_ref());
    if !caps.budgets_enabled {
        return Ok(None);
    }
    let budget = item.budget_usd.unwrap_or(caps.budget_usd_per_item);
    assert!(budget.is_finite() && budget >= 0.0, "budgets are validated");
    if budget == 0.0 {
        return Ok(None);
    }
    let spent = store.spend_for_item(&item.id, estimates)?;
    Ok(Some(ItemBudget {
        cap_usd: budget,
        spent_usd: spent,
    }))
}

/// Why the run cannot take more agents.
struct Exhausted {
    stage: String,
    error: String,
}

/// What became of a run's pending spawns.
enum Reserved {
    Spawned(Vec<StageRunId>),
    /// Every provider a stage may use is out of quota: they stay pending.
    Waiting(Vec<SpawnPlan>),
    Blocked(Exhausted),
}

/// Move up to `slots` pending spawns of `run` into `spawning` stage runs.
pub(super) async fn reserve(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: &PipelineRunId,
    slots: u32,
) -> Result<FollowUp, ArchonError> {
    assert!(slots > 0, "the planner grants at least one slot");
    let Some(queue) = table.pending.get_mut(run) else {
        return Ok(FollowUp::default());
    };
    let take = queue.len().min(slots as usize);
    if queue
        .iter()
        .take(take)
        .any(|plan| plan.reviewers.as_ref().is_some_and(Vec::is_empty))
    {
        learned::catalog(ctx).await?;
    }
    let plans: Vec<SpawnPlan> = queue.drain(..take).collect();
    if queue.is_empty() {
        table.pending.remove(run);
    }
    let spec = table.drivers.get(run).map(|driver| driver.spec().clone());
    let reserved = {
        let mut store = ctx.store.lock().await;
        reserve_plans(ctx, &mut store, (run, spec.as_deref()), plans)?
    };
    match reserved {
        Reserved::Spawned(ids) => Ok(FollowUp {
            issue: ids,
            ..FollowUp::default()
        }),
        Reserved::Waiting(plans) => {
            let queue = table.pending.entry(run.clone()).or_default();
            queue.splice(0..0, plans);
            Ok(FollowUp::default())
        }
        Reserved::Blocked(exhausted) => {
            let run = ctx.store.lock().await.run(run)?;
            finish::fail_run(ctx, table, run, exhausted.stage, exhausted.error, true).await
        }
    }
}

/// `spec` is the run's pipeline, `None` once its driver is gone; with it,
/// stages are routed by quota first (`quota::reroute_spawns`).
fn reserve_plans(
    ctx: &DaemonContext,
    store: &mut Store,
    (run_id, spec): (&PipelineRunId, Option<&PipelineSpec>),
    mut plans: Vec<SpawnPlan>,
) -> Result<Reserved, ArchonError> {
    let run = store.run(run_id)?;
    if run.state.is_terminal() || plans.is_empty() {
        return Ok(Reserved::Spawned(Vec::new()));
    }
    let mut item = store.item(&run.item)?;
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&resolve::repo_ref(id)))
        .transpose()?;
    if let Some(spec) = spec {
        let target = (&mut item, spec);
        let routed = crate::quota::reroute_spawns(ctx, store, target, &mut plans, repo.as_ref())?;
        if routed == crate::quota::Reserve::Wait {
            return Ok(Reserved::Waiting(plans));
        }
    }
    let estimates = ctx.config.limits.count_estimated_cost;
    let budget = item_budget(ctx, store, &item, estimates)?;
    if let Some(used_up) = budget.filter(|budget| budget.left() <= 0.0) {
        let error = used_up.used_up(&item.id);
        let stage = plans[0].stage_id.clone();
        return Ok(Reserved::Blocked(Exhausted { stage, error }));
    }
    if budget.is_some()
        && let Some(error) = review_refusal(ctx, store, &item, repo.as_ref(), &plans)
    {
        let stage = plans[0].stage_id.clone();
        return Ok(Reserved::Blocked(Exhausted { stage, error }));
    }
    if let Some(spec) = spec {
        let cap = RepoEnv::new(repo.clone())?.agent_cap(ctx, &run.pipeline);
        for plan in &mut plans {
            super::swarm::brief(plan, spec, cap, None);
        }
    }
    let left = budget.map(|budget| budget.left());
    let ids = reserve_each(ctx, store, (&run, &item, repo.as_ref()), plans, left)?;
    Ok(Reserved::Spawned(ids))
}

/// Why a stage in `plans` owes reviewers that an item budget forbids, found
/// before any agent starts rather than after the work is committed.
fn review_refusal(
    ctx: &DaemonContext,
    store: &Store,
    item: &QueueItem,
    repo: Option<&Repo>,
    plans: &[SpawnPlan],
) -> Option<String> {
    let policy = RepoEnv::new(repo.cloned()).ok()?.policy(&ctx.config);
    let catalog = store.model_catalog();
    plans.iter().find_map(|plan| {
        let named = plan.reviewers.as_deref()?;
        let why = contract::governed_review_refusal(
            Some(named),
            &plan.choice.model,
            &policy,
            (&ctx.config.review, &catalog),
        )?;
        Some(format!(
            "BLOCKED: item {} has review {} and a budget: {why}",
            item.id, item.autonomy.review
        ))
    })
}

/// Reserve each of `plans` of `run` as a `spawning` stage run with its
/// share of `budget`; a slot already filled is skipped.
fn reserve_each(
    ctx: &DaemonContext,
    store: &mut Store,
    (run, item, repo): (&PipelineRun, &QueueItem, Option<&Repo>),
    plans: Vec<SpawnPlan>,
    budget: Option<f64>,
) -> Result<Vec<StageRunId>, ArchonError> {
    assert_eq!(run.item, item.id, "the run is the item's");
    assert!(!plans.is_empty(), "a reservation has plans");
    let sections = run_sections(ctx, store, run, item, repo, plans[0].workspace)?;
    let experiments = experiments::item_dir(&ctx.paths, &item.id);
    let existing = store.stage_runs(&run.id)?;
    let catalog = store.model_catalog();
    let mut ids = Vec::new();
    for plan in plans {
        // The driver only emits empty slots; a filled one means a bug upstream.
        if existing.iter().any(|s| same_slot(s, &plan)) {
            tracing::error!(run = %run.id, stage = plan.stage_id, fan = plan.fan_index, "slot already has a stage run; not spawning it twice");
            continue;
        }
        let target = Target {
            run,
            item,
            repo,
            config: &ctx.config,
            catalog: &catalog,
            governed: budget.is_some(),
            sections: &sections,
            experiments: &experiments,
        };
        let share = slot_budget(budget, &plan);
        let now = archon_core::now();
        ids.push(reserve_one(store, &target, &plan, share, now, None)?);
    }
    Ok(ids)
}

/// What the store adds to the contract of every agent of `run`: the pull
/// request or other item's branch it delivers into, where the item's
/// dependencies left their work, its initiative, and the run's lessons,
/// then the repository's checks, the build cache and the artifact store.
fn run_sections(
    ctx: &DaemonContext,
    store: &Store,
    run: &PipelineRun,
    item: &QueueItem,
    repo: Option<&Repo>,
    mode: WorkspaceMode,
) -> Result<String, ArchonError> {
    assert_eq!(run.item, item.id, "the run is the item's");
    let artifacts = ctx.paths.artifacts();
    let onto = match mode {
        WorkspaceMode::Worktree if item.onto.is_some() => {
            super::onto::section(item.onto.as_deref())
        }
        WorkspaceMode::Worktree => super::onto::local::section(store, &item.id)?,
        _ => String::new(),
    };
    let dependencies = dependencies::section(store, run, item, mode, &artifacts)?;
    let initiative = dependencies::initiative_brief(store, item)?;
    let lessons = learned::lessons_for_run(store, run, item)?;
    assert!(
        lessons.len() <= CONTRACT_LESSONS,
        "a contract carries at most {CONTRACT_LESSONS} lessons"
    );
    let initiative = contract::initiative_rules(initiative.as_ref());
    let lessons = contract::lesson_rules(&lessons)?;
    let env = RepoEnv::new(repo.cloned())?;
    let tiers = env.tiers()?;
    let (affected, gated) = fast_check_now(store, run, &env, &tiers, mode)?;
    let fast = contract::FastCheck {
        affected: affected.as_ref(),
        gated,
    };
    let checks = contract::check_rules(&tiers.checks, fast);
    let cache = build_cache_section(ctx, &env, &item.id);
    let storage = contract::artifact_rules(&ctx.config.artifacts);
    Ok([
        onto,
        dependencies,
        initiative,
        lessons,
        checks,
        cache,
        storage,
    ]
    .concat())
}

/// What the run's change as committed affects, where its fast check selects
/// units and the run has a worktree, and whether ship runs that check again.
/// The selection only informs the agent: when git cannot read it, the
/// contract states the rule without it.
fn fast_check_now(
    store: &Store,
    run: &PipelineRun,
    env: &RepoEnv,
    tiers: &RepoTiers,
    mode: WorkspaceMode,
) -> Result<(Option<Affected>, bool), ArchonError> {
    let (Some(repo), Some(template)) = (&env.repo, tiers.checks.fast.as_deref()) else {
        return Ok((None, false));
    };
    assert!(run.workspace.is_absolute(), "run workspaces are absolute");
    let gated = fast_check::trusted(repo)?.is_some();
    if mode != WorkspaceMode::Worktree || !selects(template) {
        return Ok((None, gated));
    }
    let vars = super::recover::load_run_vars(store, &run.id)?;
    let base = super::ship::base_of(store, run, &vars)?;
    match fast_check::affected_by(&run.workspace, &base) {
        Ok(affected) => Ok((Some(affected), gated)),
        Err(error) => {
            tracing::warn!(run = %run.id, %error, "the change's packages are unknown; the contract omits them");
            Ok((None, gated))
        }
    }
}

/// The Build cache section, unless the repository (or none) builds into
/// its worktrees.
fn build_cache_section(ctx: &DaemonContext, env: &RepoEnv, item: &QueueItemId) -> String {
    let Some(repo) = &env.repo else {
        return String::new();
    };
    let mode = env.config.build_cache(&ctx.config.worktrees);
    let target = crate::build_cache::target_dir(&ctx.paths, &repo.name, item, mode);
    contract::build_cache_rules(target.as_deref(), mode == BuildCache::Shared)
}

fn same_slot(stage: &StageRun, plan: &SpawnPlan) -> bool {
    stage.stage_id == plan.stage_id
        && stage.attempt == plan.attempt
        && stage.fan_index == plan.fan_index
}

pub(super) fn same_slot_as(a: &StageRun, b: &StageRun) -> bool {
    a.run == b.run
        && a.stage_id == b.stage_id
        && a.attempt == b.attempt
        && a.fan_index == b.fan_index
}

/// The run a spawn belongs to.
struct Target<'a> {
    run: &'a PipelineRun,
    item: &'a QueueItem,
    repo: Option<&'a Repo>,
    /// The global `[models]` policy, which the repository's narrows.
    config: &'a Config,
    catalog: &'a archon_core::model_tier::ModelCatalog,
    /// Whether a dollar budget bounds the item, so reviewers must report cost.
    governed: bool,
    /// The contract's sections on the run itself ([`run_sections`]).
    sections: &'a str,
    /// The item's directory for reviewers' experiment copies.
    experiments: &'a Path,
}

impl Target<'_> {
    /// Whether the agent of `plan` gets Aether's `delegate`: its stage
    /// allows it, or the item's `tools = yolo` withholds nothing.
    fn delegates(&self, plan: &SpawnPlan) -> bool {
        plan.allow_delegation || self.item.autonomy.tools == ToolsMode::Yolo
    }

    /// Minutes the stage of `plan` may run: the item's override, else the pipeline's.
    fn timeout_minutes(&self, plan: &SpawnPlan) -> u32 {
        let own = self.item.stage_timeouts.get(&plan.stage_id);
        own.copied().unwrap_or(plan.timeout_minutes)
    }

    /// The stage contract, with its reviewers' models, the run's own sections
    /// and, when the agent can delegate, the rules and model policy for that.
    fn instructions(&self, plan: &SpawnPlan) -> String {
        assert!(!plan.instructions.is_empty(), "every stage has a contract");
        // An unreadable policy refuses this spawn when it is issued (`claim`).
        let policy = RepoEnv::new(self.repo.cloned()).map(|env| env.policy(self.config));
        let reviewers = reviewer_rules(plan, &policy, self);
        let contract = format!("{}{reviewers}{}", plan.instructions, self.sections);
        if !self.delegates(plan) {
            return contract;
        }
        let models = match &policy {
            Ok(policy) => policy.describe(),
            Err(error) => format!("none: the repository's model policy cannot be read ({error})"),
        };
        let depth = self.config.limits.max_delegation_depth;
        let rules =
            contract::delegation_rules(plan.workspace, &models, depth, Some(self.experiments));
        format!("{contract}{rules}")
    }
}

/// The models `plan`'s agent may delegate the reviewers it owes to, within
/// `policy` as it stands at this spawn rather than when the run began.
fn reviewer_rules(
    plan: &SpawnPlan,
    policy: &Result<ModelPolicy, ArchonError>,
    target: &Target<'_>,
) -> String {
    let Some(named) = plan.reviewers.as_deref() else {
        return String::new();
    };
    assert!(
        plan.allow_delegation,
        "an agent owing reviewers delegates them"
    );
    let rules = match policy {
        Ok(policy) => contract::governed_reviewer_rules(
            Some(named),
            &plan.choice.model,
            policy,
            (&target.config.review, target.catalog),
            target.governed,
        ),
        Err(error) => format!(
            "- Delegate no reviewer: the repository's model policy cannot be read ({error}).\n"
        ),
    };
    assert!(
        rules.ends_with('\n'),
        "contract sections end with a newline"
    );
    rules
}

/// Persist the exact request, then the `spawning` stage run that owns it,
/// with the slot's `budget`. `retry` numbers an infrastructure replacement
/// of the slot.
fn reserve_one(
    store: &mut Store,
    target: &Target<'_>,
    plan: &SpawnPlan,
    budget: Option<f64>,
    now: Timestamp,
    retry: Option<usize>,
) -> Result<StageRunId, ArchonError> {
    let id = StageRunId::new();
    let request = build_request(target, plan, &id, budget, now, retry);
    // An infrastructure replacement is a fresh agent: the kept one was lost.
    let (resume, replaces) = match resume::takeover(store, &target.run.id, plan, retry.is_some())? {
        Takeover::Fresh => (None, None),
        Takeover::Resume(kept) => (Some(kept), None),
        Takeover::Replace(agent) => (None, Some(agent)),
    };
    let spawn = StoredSpawn {
        request,
        timeout_minutes: target.timeout_minutes(plan),
        parent: ParentBinding::Pending,
        team: plan.team.as_ref().map(|slot| StoredWorker {
            role: slot.role.clone(),
            branch: slot.branch.clone(),
        }),
        resume,
        replaces,
    };
    save_spawn(store, &id, &spawn)?;
    let stage = StageRun {
        id: id.clone(),
        run: target.run.id.clone(),
        stage_id: plan.stage_id.clone(),
        kind: Some(plan.kind),
        attempt: plan.attempt,
        fan_index: plan.fan_index,
        model_segments: Vec::new(),
        infrastructure_failure: false,
        host_context: Default::default(),
        choice: plan.choice.clone(),
        aether_agent: None,
        aether_task: None,
        state: StageState::Spawning,
        verdict: None,
        summary: None,
        structured: None,
        cost_usd: 0.0,
        started_at: now,
        finished_at: None,
    };
    store.upsert_stage_run(&stage)?;
    Ok(id)
}

/// A team slot's share of what is left: the remainder split evenly across
/// the stage's slots, so the roles together cannot overspend it.
fn slot_budget(budget: Option<f64>, plan: &SpawnPlan) -> Option<f64> {
    assert!(plan.fan_count >= 1, "a stage has at least one slot");
    assert!(
        budget.is_none_or(f64::is_finite),
        "budgets are finite amounts"
    );
    match plan.team {
        Some(_) => budget.map(|left| left / f64::from(plan.fan_count)),
        None => budget,
    }
}

/// Headless stage agents use ungated approvals; delegation follows the stage
/// flag or yolo tools. Labels carry identity independently of the display name.
fn build_request(
    target: &Target<'_>,
    plan: &SpawnPlan,
    id: &StageRunId,
    budget: Option<f64>,
    now: Timestamp,
    retry: Option<usize>,
) -> SpawnRequest {
    let (run, item) = (target.run, target.item);
    assert!(run.workspace.is_absolute(), "run workspaces are absolute");
    let workspace = match &plan.team {
        Some(slot) => slot.workspace.clone(),
        None => run.workspace.clone(),
    };
    assert!(
        run.team_root
            .as_ref()
            .is_none_or(|root| workspace.starts_with(root)),
        "a team run's agents work under its team directory"
    );
    SpawnRequest {
        host_context: Default::default(),
        idempotency_key: id.as_str().to_owned(),
        role: format!("archon-stage:{}/{}", run.pipeline, plan.stage_id),
        // Every reserved spawn is bound to the run's node before it is sent.
        name: naming::stage_agent_name(naming::Placement::UnderRunNode, plan, retry),
        objective: plan.prompt.clone(),
        instructions: target.instructions(plan),
        choice: plan.choice.clone(),
        workspace,
        repository: target.repo.map(|repo| repo.name.clone()),
        repository_root: None,
        labels: labels(target, plan, id),
        hidden_tools: hidden_tools(item.autonomy.tools, plan.workspace),
        leaf: !target.delegates(plan),
        cost_budget_usd: budget,
        aether_cost_budget: None,
        deadline: Some(deadline(now, target.timeout_minutes(plan))),
        attach_run: run.aether_run.clone(),
        parent: None,
        network: None,
        keep: plan.keep,
    }
}

/// What ties the agent, and through Aether its children, to this stage run.
fn labels(target: &Target<'_>, plan: &SpawnPlan, id: &StageRunId) -> BTreeMap<String, String> {
    [
        ("archon.item", target.item.id.to_string()),
        ("archon.run", target.run.id.to_string()),
        ("archon.stage", plan.stage_id.clone()),
        ("archon.stage_run", id.to_string()),
        ("archon.attempt", plan.attempt.to_string()),
        ("archon.fan", plan.fan_index.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

/// Aether tools no stage agent gets: a stage is bounded work, and a
/// schedule would outlive it (ARCHITECTURE §18.4).
const HIDDEN_NATIVE_TOOLS: [&str; 3] = ["create_schedule", "update_schedule", "list_schedules"];
/// Hidden too where the workspace is read-only: nothing is integrated into it.
const INTEGRATE_TOOL: &str = "integrate_workspace";

/// What a stage agent does not see: Archon's tools that act for the
/// orchestrator (but for [`STAGE_TOOLS`](archon_core::mcp_tools::STAGE_TOOLS)), and the Aether tools above. A child inherits the list.
fn hidden_tools(tools: ToolsMode, workspace: WorkspaceMode) -> Vec<String> {
    let hidden = match tools {
        ToolsMode::Guarded => archon_core::mcp_tools::qualified_names(),
        ToolsMode::Yolo => archon_core::mcp_tools::mutating_names(),
    };
    assert!(
        !hidden.is_empty(),
        "stage agents never act for the orchestrator"
    );
    let read_only = workspace == WorkspaceMode::RepoReadonly;
    let native = HIDDEN_NATIVE_TOOLS
        .into_iter()
        .chain(read_only.then_some(INTEGRATE_TOOL));
    hidden
        .into_iter()
        .filter(|name| {
            !archon_core::mcp_tools::STAGE_TOOLS
                .iter()
                .any(|tool| name == &archon_core::mcp_tools::qualified(tool))
        })
        .chain(native.map(str::to_owned))
        .collect()
}

pub(super) fn reserve_replacement(
    ctx: &DaemonContext,
    store: &mut Store,
    failed: &StageRun,
    plan: &SpawnPlan,
    retry: usize,
) -> Result<Option<StageRunId>, ArchonError> {
    assert!(
        same_slot(failed, plan),
        "a replacement fills the failed slot"
    );
    assert!(retry >= 1, "retries are counted from one");
    let run = store.run(&failed.run)?;
    assert!(!run.state.is_terminal(), "replacements join live runs only");
    let item = store.item(&run.item)?;
    let estimates = ctx.config.limits.count_estimated_cost;
    let budget = item_budget(ctx, store, &item, estimates)?.map(ItemBudget::left);
    if budget.is_some_and(|left| left <= 0.0) {
        return Ok(None);
    }
    let repo = item
        .repo
        .as_ref()
        .map(|id| store.repo(&resolve::repo_ref(id)))
        .transpose()?;
    let sections = run_sections(ctx, store, &run, &item, repo.as_ref(), plan.workspace)?;
    let experiments = experiments::item_dir(&ctx.paths, &item.id);
    let target = Target {
        run: &run,
        item: &item,
        repo: repo.as_ref(),
        config: &ctx.config,
        catalog: &store.model_catalog(),
        governed: false,
        sections: &sections,
        experiments: &experiments,
    };
    // An earlier grant is not enforced once budgets are off.
    let budget = match resolve::item_caps(ctx, item.repo.as_ref()).budgets_enabled {
        true => replacement_budget(store, failed, plan, budget, estimates)?,
        false => Some(None),
    };
    let Some(budget) = budget else {
        return Ok(None);
    };
    let target = Target {
        governed: budget.is_some(),
        ..target
    };
    let started = archon_core::now().max(failed.started_at + chrono::Duration::microseconds(1));
    let id = reserve_one(store, &target, plan, budget, started, Some(retry))?;
    Ok(Some(id))
}

/// A replacement's budget, from what is `left` of the item's: a team slot
/// keeps what its failed agent left of its share, since its siblings hold
/// theirs. `None` when that share is used up.
fn replacement_budget(
    store: &Store,
    failed: &StageRun,
    plan: &SpawnPlan,
    left: Option<f64>,
    estimates: bool,
) -> Result<Option<Option<f64>>, ArchonError> {
    assert!(
        left.is_none_or(|left| left > 0.0),
        "only a live budget is split"
    );
    if plan.team.is_none() {
        return Ok(Some(left));
    }
    let Some(granted) = load_spawn(store, &failed.id)?.request.cost_budget_usd else {
        return Ok(Some(slot_budget(left, plan)));
    };
    assert!(granted.is_finite(), "budgets are finite amounts");
    let spent = match &failed.aether_agent {
        Some(agent) => store.spend_for_agent(agent, estimates)?,
        None => 0.0,
    };
    let unspent = granted - spent.max(failed.cost_usd);
    if unspent <= 0.0 {
        return Ok(None);
    }
    let share = left.map_or(unspent, |left| left.min(unspent));
    debug_assert!(share > 0.0 && share <= granted);
    Ok(Some(Some(share)))
}

/// A stage run claimed for one `create_agent`.
struct Claim {
    stage: StageRun,
    spawn: StoredSpawn,
    /// The effective model policy's verdict on the request, taken now.
    allowed: Result<(), ArchonError>,
    run_finished: bool,
    /// A team slot's worktree, made ready before its agent is created.
    worker: Option<team::Worker>,
    /// The run's item, whose experiments directory a reviewing agent uses.
    item: QueueItemId,
}

impl Claim {
    /// Nothing was ever sent for it and its run is over: settle it without
    /// Aether instead of creating an agent only to cancel it.
    fn unsent_for_finished_run(&self) -> bool {
        self.run_finished && self.spawn.parent == ParentBinding::Pending
    }
}

/// How a claim attempt ended.
enum Claimed {
    /// Another task is issuing it, or it is no longer `spawning`.
    Skip,
    Issue(Box<Claim>),
    /// It could not be issued and was settled as failed instead.
    Settled(Box<FollowUp>),
}

/// Issue `create_agent` for one `spawning` stage run unless another task
/// already is. Safe to repeat: the stage run id is the idempotency key.
pub(super) async fn issue(ctx: &DaemonContext, id: &StageRunId) -> Result<FollowUp, ArchonError> {
    let issued = issue_claimed(ctx, id).await;
    // The new agent's first events can arrive before its record does.
    ctx.spawn_recorded.notify_waiters();
    issued
}

async fn issue_claimed(ctx: &DaemonContext, id: &StageRunId) -> Result<FollowUp, ArchonError> {
    let claim = match claim(ctx, id).await? {
        Claimed::Skip => return Ok(FollowUp::default()),
        Claimed::Settled(follow) => return Ok(*follow),
        Claimed::Issue(claim) => claim,
    };
    let unsent = claim.unsent_for_finished_run();
    let Claim {
        stage,
        spawn,
        allowed,
        worker,
        item,
        ..
    } = *claim;
    // Only a spawn never sent prepares its worktree: once its request may
    // have reached Aether, a live agent may be working in it.
    let prepare = allowed.is_ok() && !unsent && spawn.parent == ParentBinding::Pending;
    let reviews = allowed.is_ok() && !unsent && can_review(&stage, &spawn);
    let workspace = prepare.then_some(spawn.request.workspace.as_path());
    let worker = worker.filter(|_| prepare);
    let ready = ready_to_spawn(ctx, id, (worker, workspace), reviews, &item).await;
    if let Err(error) = ready {
        let mut table = ctx.runs.lock().await;
        let was_claimed = table.in_flight.remove(id);
        assert!(was_claimed, "stage run {id} was prepared without a claim");
        return refuse(ctx, &mut table, &stage, error).await;
    }
    let result = match allowed {
        Ok(()) if unsent => None,
        Ok(()) => Some(resume_or_spawn(ctx, &stage, spawn).await),
        Err(refusal) => Some(Err(refusal)),
    };
    let mut table = ctx.runs.lock().await;
    let was_claimed = table.in_flight.remove(id);
    assert!(was_claimed, "stage run {id} was issued without a claim");
    match result {
        Some(result) => record(ctx, &mut table, &stage, result).await,
        None => cancel_unsent(ctx, &mut table, &stage).await,
    }
}

/// Whether the agent reviews, or is told how its reviewers work: a review
/// stage's, or one that can delegate.
fn can_review(stage: &StageRun, spawn: &StoredSpawn) -> bool {
    stage.kind == Some(StageKind::Review) || !spawn.request.leaf
}

/// What must be on disk before claimed stage run `id`'s agent is created: a team
/// slot's worktree, the `workspace` the agent works in, and the item's
/// experiments directory when the agent `reviews`.
async fn ready_to_spawn(
    ctx: &DaemonContext,
    id: &StageRunId,
    (worker, workspace): (Option<team::Worker>, Option<&Path>),
    reviews: bool,
    item: &QueueItemId,
) -> Result<(), String> {
    if let Some(worker) = worker {
        assert_eq!(&worker.item, item, "a team slot works for its run's item");
        team::ready_worker(ctx, worker)
            .await
            .map_err(|e| format!("preparing the role's worktree failed: {e}"))?;
    }
    // An engine started in a directory that is gone says something else.
    if let Some(workspace) = workspace.filter(|path| !path.is_dir()) {
        assert!(workspace.is_absolute(), "run workspaces are absolute");
        return Err(format!("{WORKSPACE_MISSING} {}", workspace.display()));
    }
    if !reviews {
        return Ok(());
    }
    // Made under the table while the claim keeps the item from being
    // discarded, so a discard never comes between the check and the create.
    let table = ctx.runs.lock().await;
    assert!(table.in_flight.contains(id), "prepared under its claim");
    experiments::prepare(&ctx.paths, item)
        .map_err(|e| format!("preparing the experiments directory failed: {e}"))
}

/// What issuing a stage run gave it: a new agent with its task, or the
/// agent kept from the slot's previous attempt, which has no new task.
pub(super) enum Issued {
    Spawned(SpawnedAgent),
    Resumed(AetherAgentId),
}

/// Resume the slot's kept agent when there is one and it takes the message;
/// otherwise the stored request creates a fresh agent, and stays that way.
async fn resume_or_spawn(
    ctx: &DaemonContext,
    stage: &StageRun,
    mut spawn: StoredSpawn,
) -> Result<Issued, ArchonError> {
    // A grant reserved while budgets were on is dropped once they are off.
    let granted = spawn.request.cost_budget_usd.is_some();
    let repo = {
        let store = ctx.store.lock().await;
        let run = store.run(&stage.run)?;
        store.item(&run.item)?.repo
    };
    if granted
        && spawn.parent == ParentBinding::Pending
        && !resolve::item_caps(ctx, repo.as_ref()).budgets_enabled
    {
        spawn.request.cost_budget_usd = None;
        spawn.request.aether_cost_budget = None;
        save_spawn(&mut *ctx.store.lock().await, &stage.id, &spawn)?;
    }
    if let Some(kept) = spawn.resume.take() {
        assert_eq!(
            spawn.parent,
            ParentBinding::Pending,
            "a resume sends no create"
        );
        if resume::hand_over(ctx, stage, &kept, &spawn.request).await? {
            return Ok(Issued::Resumed(kept.agent));
        }
        save_spawn(&mut *ctx.store.lock().await, &stage.id, &spawn)?;
    }
    if let Some(agent) = spawn.replaces.take() {
        resume::replace(ctx, &agent).await?;
        save_spawn(&mut *ctx.store.lock().await, &stage.id, &spawn)?;
    }
    spawn_bound(ctx, stage, spawn).await.map(Issued::Spawned)
}

/// Bind a new stage to its run node and freeze its parent, network and context.
/// Replays preserve these inputs except for a fallback from unsupported lean.
async fn spawn_bound(
    ctx: &DaemonContext,
    stage: &StageRun,
    mut spawn: StoredSpawn,
) -> Result<SpawnedAgent, ArchonError> {
    if spawn.request.cost_budget_usd.is_some() {
        let unresolved = spawn.request.aether_cost_budget.is_none();
        ctx.backend
            .prepare_spawn(&mut spawn.request, ctx.config.limits.count_estimated_cost)
            .await?;
        if unresolved {
            save_spawn(&mut *ctx.store.lock().await, &stage.id, &spawn)?;
        }
    }
    if spawn.parent == ParentBinding::Pending {
        assert!(
            spawn.request.parent.is_none() && spawn.request.network.is_none(),
            "a pending binding names no parent and no network"
        );
        let default = ctx.backend.network_default().await?;
        let model = &spawn.request.choice.model;
        let node = run_node::ensure(ctx, &stage.run, model, default).await?;
        let network = node.network.narrower(default);
        spawn.request.host_context = ctx
            .backend
            .host_context(ctx.config.aether.host_context.for_model(model))
            .await?;
        (spawn.request.repository, spawn.request.repository_root) = node.repository.parts();
        spawn.request.parent = Some(node.agent);
        spawn.request.network = Some(network);
        spawn.parent = ParentBinding::Bound;
        save_spawn(&mut *ctx.store.lock().await, &stage.id, &spawn)?;
    }
    let effective = ctx
        .backend
        .host_context(
            spawn
                .request
                .host_context
                .for_model(&spawn.request.choice.model),
        )
        .await?;
    let mut store = ctx.store.lock().await;
    record_host_context(&mut store, &stage.id, effective)?;
    if effective != spawn.request.host_context {
        spawn.request.host_context = effective;
        save_spawn(&mut store, &stage.id, &spawn)?;
        tracing::warn!(stage = %stage.id, "lean host context unavailable for this engine or Aether; frozen stage request falls back to full");
    }
    drop(store);
    ctx.backend.spawn(spawn.request).await
}

/// A `spawning` stage of a finished run with no agent: its create was
/// never sent, or Aether refused it.
async fn cancel_unsent(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
) -> Result<FollowUp, ArchonError> {
    assert_eq!(
        stage.state,
        StageState::Spawning,
        "only a spawn is cancelled unsent"
    );
    tracing::info!(stage = %stage.id, "run finished before its stage agent was created; not creating it");
    let outcome = StageOutcome::Cancelled {
        reason: Some(format!("pipeline run {} finished", stage.run)),
    };
    let mut follow = settle::settle_locked(ctx, table, &stage.id, outcome).await?;
    // It may have been the run's last live stage: let `stop_run` retire the node.
    follow.stop.push(stage.run.clone());
    Ok(follow)
}

async fn claim(ctx: &DaemonContext, id: &StageRunId) -> Result<Claimed, ArchonError> {
    let mut table = ctx.runs.lock().await;
    if table.in_flight.contains(id) {
        return Ok(Claimed::Skip);
    }
    let (stage, spawn, repo, run) = {
        let store = ctx.store.lock().await;
        let stage = store.stage_run(id)?;
        if stage.state != StageState::Spawning {
            return Ok(Claimed::Skip);
        }
        let run = store.run(&stage.run)?;
        // What an earlier stage agent of the run delegated is stopped first.
        if !run.state.is_terminal() && descendants::holds(&store, &run.id)? {
            return Ok(Claimed::Skip);
        }
        let item = store.item(&run.item)?;
        let repo = item
            .repo
            .as_ref()
            .map(|r| store.repo(&resolve::repo_ref(r)))
            .transpose()?;
        (stage, load_spawn(&store, id), repo, run)
    };
    let spawn = match spawn {
        Ok(spawn) => spawn,
        Err(ArchonError::NotFound(what)) => {
            let error = format!("cannot spawn: {what} is missing from the store");
            let refused = StageOutcome::Failed {
                error,
                infra: false,
            };
            let follow = settle::settle_locked(ctx, &mut table, id, refused).await?;
            return Ok(Claimed::Settled(Box::new(follow)));
        }
        Err(e) => return Err(e),
    };
    let worker = spawn.team.as_ref().map(|slot| team::Worker {
        item: run.item.clone(),
        run: run.id.clone(),
        repo_path: repo.as_ref().map(|r| r.path.clone()),
        run_tree: run.workspace.clone(),
        path: spawn.request.workspace.clone(),
        branch: slot.branch.clone(),
        start: run.branch.clone(),
    });
    let allowed = RepoEnv::new(repo)
        .map_err(|e| ArchonError::Policy(format!("repository model policy unreadable: {e}")))
        .and_then(|env| env.policy(&ctx.config).check(&spawn.request.choice));
    table.in_flight.insert(id.clone());
    Ok(Claimed::Issue(Box::new(Claim {
        stage,
        spawn,
        allowed,
        run_finished: run.state.is_terminal(),
        worker,
        item: run.item,
    })))
}

async fn record(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    result: Result<Issued, ArchonError>,
) -> Result<FollowUp, ArchonError> {
    let error = match result {
        Ok(Issued::Spawned(spawned)) => {
            table.spawn_errors.remove(&stage.id);
            ctx.disk.agent_created();
            return spawned_ok(ctx, stage, spawned).await;
        }
        Ok(Issued::Resumed(agent)) => {
            table.spawn_errors.remove(&stage.id);
            return resume::resumed_ok(ctx, stage, agent).await;
        }
        Err(error) => error,
    };
    match error {
        ArchonError::Unavailable(why) => {
            tracing::info!(stage = %stage.id, %why, "create_agent unanswered; retrying under the same key");
            Ok(FollowUp::default())
        }
        error if error.is_store() => Err(error),
        error if disk::is_disk_full(&error.to_string()) => {
            disk_full(ctx, table, stage, error).await
        }
        ArchonError::Protocol(_) | ArchonError::Io { .. } => {
            retry_or_refuse(ctx, table, stage, error).await
        }
        error if wire::is_internal(&error) => retry_or_refuse(ctx, table, stage, error).await,
        ArchonError::Policy(_) => policy_refusal(ctx, table, stage, error).await,
        other => refuse(ctx, table, stage, format!("cannot create agent: {other}")).await,
    }
}

/// A transport failure, or a failure inside Aether (`internal`): retried
/// under the same key up to [`MAX_TRANSPORT_ERRORS`] times, then refused.
async fn retry_or_refuse(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    error: ArchonError,
) -> Result<FollowUp, ArchonError> {
    assert!(!error.is_store(), "a store failure is never retried here");
    assert_eq!(stage.state, StageState::Spawning, "only a spawn is retried");
    let errors = table.spawn_errors.entry(stage.id.clone()).or_default();
    errors.transport += 1;
    if errors.transport < MAX_TRANSPORT_ERRORS {
        tracing::warn!(stage = %stage.id, %error, "create_agent failed; retrying under the same key");
        return Ok(FollowUp::default());
    }
    table.spawn_errors.remove(&stage.id);
    refuse(
        ctx,
        table,
        stage,
        format!("create_agent kept failing: {error}"),
    )
    .await
}

/// `create_agent` failed on a full disk: dispatch holds until space is
/// free, and the sweep then issues it again under the same key, up to
/// [`MAX_DISK_FULL_SPAWNS`] times before the stage fails with a `Blocker`.
/// A finished run needs no agent: its stage is settled `cancelled` at once,
/// which is safe as Aether refused the create and it is never sent again.
async fn disk_full(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    error: ArchonError,
) -> Result<FollowUp, ArchonError> {
    assert!(!error.is_store(), "a store failure is never retried here");
    assert_eq!(stage.state, StageState::Spawning, "only a spawn is retried");
    let error = error.to_string();
    ctx.disk
        .report_full(&error, FullDisk::Aether, Instant::now());
    if ctx.store.lock().await.run(&stage.run)?.state.is_terminal() {
        tracing::info!(stage = %stage.id, %error, "create_agent of a finished run hit a full disk; not retrying it");
        table.spawn_errors.remove(&stage.id);
        return cancel_unsent(ctx, table, stage).await;
    }
    let errors = table.spawn_errors.entry(stage.id.clone()).or_default();
    errors.disk_full += 1;
    let failures = errors.disk_full;
    if failures < MAX_DISK_FULL_SPAWNS {
        tracing::warn!(stage = %stage.id, %error, failures, "create_agent hit a full disk; it waits for space");
        return Ok(FollowUp::poke());
    }
    table.spawn_errors.remove(&stage.id);
    let message = format!(
        "create_agent failed on a full disk {failures} times ({error}). Archon's own disk \
         had room each time it retried, so the full disk is likely Aether's: free space \
         there, then retry the item"
    );
    fail_stage_and_run(ctx, table, stage, message).await
}

/// A refused spawn fails the stage; the driver decides what that means.
async fn refuse(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    error: String,
) -> Result<FollowUp, ArchonError> {
    tracing::warn!(stage = %stage.id, %error, "stage spawn refused");
    let outcome = StageOutcome::Failed {
        error,
        infra: false,
    };
    settle::settle_locked(ctx, table, &stage.id, outcome).await
}

/// A model outside the policy never reaches Aether: the stage and its run
/// fail with a `Blocker`, since only the operator can resolve it.
async fn policy_refusal(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    error: ArchonError,
) -> Result<FollowUp, ArchonError> {
    assert!(matches!(error, ArchonError::Policy(_)));
    let message = format!("stage {} refused at spawn: {error}", stage.stage_id);
    fail_stage_and_run(ctx, table, stage, message).await
}

/// Fail a spawning stage and its run with a `Blocker`, as only the operator
/// can resolve why its agent cannot be created. A run already finished is
/// stopped instead, which retires it once no stage is left.
async fn fail_stage_and_run(
    ctx: &DaemonContext,
    table: &mut RunTable,
    stage: &StageRun,
    message: String,
) -> Result<FollowUp, ArchonError> {
    assert!(!message.trim().is_empty(), "a failed stage says why");
    let run = {
        let mut store = ctx.store.lock().await;
        let mut failed = store.stage_run(&stage.id)?;
        if failed.state.is_terminal() {
            return Ok(FollowUp::default());
        }
        failed.state = StageState::Failed {
            error: message.clone(),
        };
        failed.finished_at = Some(archon_core::now());
        store.upsert_stage_run(&failed)?;
        store.run(&stage.run)?
    };
    assert_eq!(run.id, stage.run, "the stage run belongs to its run");
    if run.state.is_terminal() {
        return Ok(FollowUp {
            stop: vec![run.id],
            poke: true,
            ..FollowUp::default()
        });
    }
    finish::fail_run(ctx, table, run, stage.stage_id.clone(), message, true).await
}

/// Record the agent and task; a finished run stops the new agent at once.
async fn spawned_ok(
    ctx: &DaemonContext,
    stage: &StageRun,
    spawned: SpawnedAgent,
) -> Result<FollowUp, ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut current = store.stage_run(&stage.id)?;
    if current.state != StageState::Spawning {
        tracing::error!(stage = %stage.id, agent = %spawned.agent, "spawned agent for a settled stage run; cancelling it");
        let reason = "stage run already settled".to_owned();
        return Ok(FollowUp {
            cancel_agents: vec![(spawned.agent, reason)],
            ..FollowUp::default()
        });
    }
    current.aether_agent = Some(spawned.agent);
    current.aether_task = Some(spawned.task.clone());
    current.state = StageState::Running;
    store.upsert_stage_run(&current)?;
    let mut run = store.run(&stage.run)?;
    if run.state.is_terminal() {
        return Ok(FollowUp {
            stop: vec![run.id],
            ..FollowUp::default()
        });
    }
    if run.aether_run.is_none() {
        run.aether_run = Some(spawned.run);
        store.update_run(&run)?;
    }
    Ok(FollowUp {
        check: vec![(current.id, spawned.task)],
        roster: vec![run.id],
        ..FollowUp::default()
    })
}
