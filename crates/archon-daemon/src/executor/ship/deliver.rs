//! The delivery archond makes itself (ARCHITECTURE §8.7) once a driver asks
//! for it: prove the run worktree, push its branch, find or open the pull
//! request, then let the driver finish the run with how that ended.
//!
//! Git and `gh` run without any lock. The outcome is persisted in `kv`
//! before the driver acts on it, so a rebuilt driver replays it; a crash
//! before that repeats the delivery, which pushes nothing twice and reuses
//! the pull request GitHub lists. An auto-merge it owes is recorded first.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::pull_request::OntoPullRequest;
use archon_core::repo::Repo;
use archon_core::run::{PipelineRun, StageState};
use archon_pipeline::{RunDriver, ShipOutcome, ShipPlan};
use archon_queue::Store;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::{
    FollowUp, RunTable, ShipRetry, descendants, fast_check, initiative_ship, recover,
    repo_identity, restack, settle,
};
use super::{auto_merge, checks, existing, gate, pr_checks, remote};
use crate::context::DaemonContext;
use crate::resolve;

/// `kv`: `ship:<run>` → the JSON [`ShipOutcome`].
const SHIP_KEY_PREFIX: &str = "ship:";
/// `kv`: `ship_retry:<run>` → the JSON [`PersistedRetry`] of a failing delivery.
const RETRY_KEY_PREFIX: &str = "ship_retry:";
/// Deliveries tried before a failure another try might pass fails the run.
const MAX_ATTEMPTS: u32 = 5;
/// The wait before the second try; each later one waits twice as long.
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(15);

fn ship_key(run: &PipelineRunId) -> String {
    format!("{SHIP_KEY_PREFIX}{run}")
}

fn retry_key(run: &PipelineRunId) -> String {
    format!("{RETRY_KEY_PREFIX}{run}")
}

/// A [`ShipRetry`] as it outlives a restart: the wait is a time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedRetry {
    failures: u32,
    next_at: DateTime<Utc>,
}

/// A driver asked for `plan`: remember it until it was delivered.
pub(in crate::executor) fn queue(
    table: &mut RunTable,
    run: &PipelineRun,
    plan: ShipPlan,
) -> FollowUp {
    assert!(!run.state.is_terminal(), "only a live run is delivered");
    assert!(!run.branch.is_empty(), "a delivered run has a branch");
    table.ships.insert(run.id.clone(), plan);
    FollowUp {
        ship: vec![run.id.clone()],
        ..FollowUp::default()
    }
}

/// A claimed delivery: what to ship, from where and how.
struct Claimed {
    plan: ShipPlan,
    run: PipelineRun,
    /// `None` when the item's repository is no longer registered.
    repo: Option<Repo>,
    /// The existing pull request the item delivers into, if any.
    onto: Option<Box<OntoPullRequest>>,
    vars: BTreeMap<String, String>,
    /// The outcome persisted already, if any.
    recorded: Option<ShipOutcome>,
    /// The fast check's verdict or failure, once it finished.
    gate: Option<Result<gate::Verdict, ArchonError>>,
}

/// How far one delivery got: done, or waiting for its fast check to run.
#[derive(Debug)]
enum Carried {
    Done(ShipOutcome),
    Gate(gate::Check),
}

/// Run `run`'s queued delivery once and let its driver finish the run; a
/// transient failure stays queued for the sweep, a persisted outcome replays.
pub(in crate::executor) async fn deliver(
    ctx: &DaemonContext,
    run_id: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    let Some(mut claimed) = claim(ctx, run_id).await? else {
        return Ok(FollowUp::default());
    };
    let gate = claimed.gate.take();
    let delivered = match claimed.recorded.clone() {
        Some(outcome) => Ok(Carried::Done(outcome)),
        None => fresh(ctx, &claimed, gate).await,
    };
    let Claimed { plan, run, .. } = claimed;
    let mut table = ctx.runs.lock().await;
    let released = table.shipping.remove(run_id);
    assert!(released, "a claimed delivery is in flight until it ends");
    if table.ships.get(run_id) != Some(&plan) {
        // The run finished or was resumed meanwhile; its replay owns it now.
        return Ok(FollowUp::poke());
    }
    let delivered = match delivered {
        Ok(Carried::Gate(check)) => {
            // Still queued: the gate's verdict wakes the sweep that delivers it.
            table.gates.start(run_id, check, Arc::clone(&ctx.dispatch));
            return Ok(FollowUp::default());
        }
        Ok(Carried::Done(outcome)) => Ok(outcome),
        Err(error) => Err(error),
    };
    table.ships.remove(run_id);
    let result = match delivered {
        Ok(outcome) => apply(ctx, &mut table, run, outcome).await,
        Err(error) => retry_or_fail(ctx, &mut table, run, &plan, error).await,
    };
    if result.is_err() {
        requeue(&mut table, run_id, plan);
    }
    result
}

/// The live run's queued delivery, marked in flight; `None` when none is due
/// to run here now or the run's delegated agents are still to be stopped.
async fn claim(
    ctx: &DaemonContext,
    run_id: &PipelineRunId,
) -> Result<Option<Claimed>, ArchonError> {
    let mut table = ctx.runs.lock().await;
    let Some(plan) = table.ships.get(run_id).cloned() else {
        return Ok(None);
    };
    let store = ctx.store.lock().await;
    if !table.ship_retries.contains_key(run_id)
        && let Some(retry) = load_retry(&store, run_id)?
    {
        table.ship_retries.insert(run_id.clone(), retry);
    }
    if table.shipping.contains(run_id) || !due(&table, run_id, Instant::now()) {
        return Ok(None);
    }
    let run = store.run(run_id)?;
    if run.state.is_terminal() {
        table.ships.remove(run_id);
        return Ok(None);
    }
    // An agent one of the run's stages delegated to may still write the worktree.
    if descendants::holds(&store, run_id)? {
        return Ok(None);
    }
    let gate = match table.gates.status(run_id) {
        gate::Status::Running => return Ok(None),
        gate::Status::Idle => None,
        gate::Status::Finished(verdict) => Some(verdict),
    };
    let recorded = store
        .kv_get(&ship_key(run_id))?
        .map(|raw| serde_json::from_str::<ShipOutcome>(&raw))
        .transpose()
        .map_err(|e| ArchonError::Store(format!("delivery outcome of run {run_id}: {e}")))?;
    let claimed = Claimed {
        plan,
        repo: repo_of(&store, &run)?,
        onto: store.item(&run.item)?.onto,
        vars: recover::load_run_vars(&store, run_id)?,
        recorded,
        gate,
        run,
    };
    assert!(
        claimed.run.workspace.is_absolute(),
        "a delivered run has a worktree"
    );
    let unclaimed = table.shipping.insert(run_id.clone());
    assert!(unclaimed, "a run has one delivery in flight");
    Ok(Some(claimed))
}

/// The repository of `run`'s item; `None` once it is unregistered.
fn repo_of(store: &Store, run: &PipelineRun) -> Result<Option<Repo>, ArchonError> {
    let Some(repo) = store.item(&run.item)?.repo else {
        return Ok(None);
    };
    match store.repo(&resolve::repo_ref(&repo)) {
        Ok(found) => {
            assert!(found.path.is_absolute(), "repository paths are canonical");
            Ok(Some(found))
        }
        Err(ArchonError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Deliver `claimed` now and persist how it ended, before anything acts on
/// it, unless its fast check is to run first. `Err` is a failure another
/// try may pass; nothing is persisted then.
async fn fresh(
    ctx: &DaemonContext,
    claimed: &Claimed,
    gate: Option<Result<gate::Verdict, ArchonError>>,
) -> Result<Carried, ArchonError> {
    assert!(
        claimed.recorded.is_none(),
        "a recorded delivery is not repeated"
    );
    let outcome = match carry_out(ctx, claimed, gate).await? {
        Carried::Done(outcome) => outcome,
        Carried::Gate(check) => return Ok(Carried::Gate(check)),
    };
    let (run, plan) = (&claimed.run, &claimed.plan);
    let mut store = ctx.store.lock().await;
    // Delivery into an existing pull request never merges it.
    let owes_merge = claimed.vars.get("ship.auto_merge").map(String::as_str) == Some("true")
        && claimed.onto.is_none();
    if let ShipOutcome::Shipped {
        pr_url: Some(url), ..
    } = &outcome
    {
        let repo = claimed
            .repo
            .as_ref()
            .expect("a pull request is of a registered repository");
        pr_checks::owe(&mut store, &run.id, &run.item, &repo.id, url)?;
        if owes_merge {
            auto_merge::owe(&mut store, &run.id, &run.item, &repo.id, url)?;
            ctx.coordination.changed.notify_one();
        }
    }
    persist(&mut store, run, plan, &outcome)?;
    store.kv_delete(&retry_key(&run.id))?;
    Ok(Carried::Done(outcome))
}

/// Prove the worktree and pass its fast check, then in `pr` mode push the
/// checked commit and find or open its pull request. A refusal is the
/// `Failed` outcome, never an `Err`.
async fn carry_out(
    ctx: &DaemonContext,
    claimed: &Claimed,
    gate: Option<Result<gate::Verdict, ArchonError>>,
) -> Result<Carried, ArchonError> {
    let refused = |error: String| Ok(Carried::Done(ShipOutcome::Failed { error }));
    let (run, plan) = (&claimed.run, &claimed.plan);
    let Some(repo) = claimed.repo.clone() else {
        return refused("the item's repository is no longer registered with Archon".into());
    };
    let pushes = pushes(&claimed.vars)?;
    let (base, draft) = {
        let store = ctx.store.lock().await;
        let base = base_of(&store, run, &claimed.vars)?;
        (base, initiative_ship::opens_as_draft(&store, &run.item)?)
    };
    // Proved again after a fast check ran: it may have changed the worktree.
    let commit = match prove(ctx, claimed, &repo, &base).await? {
        Ok(commit) => commit,
        Err(refusal) => return refused(refusal),
    };
    match gate_step(ctx, claimed, &repo, &base, gate)? {
        GateStep::Pass => {}
        GateStep::Refuse(refusal) => return refused(refusal),
        GateStep::Run(check) => return Ok(Carried::Gate(check)),
    }
    if !pushes {
        // `pr` mode verifies in `deliver_pr`, before its push.
        match initiative_ship::verify_stack(ctx, run, &commit).await {
            Ok(()) => {}
            Err(ArchonError::Invalid(why)) => return refused(why),
            Err(error) => return Err(error),
        }
        let shipped = ShipOutcome::Shipped {
            commit,
            pr_url: None,
            updated: false,
        };
        return Ok(Carried::Done(shipped));
    }
    if let Some(onto) = &claimed.onto {
        return into_existing(ctx, &repo, onto, commit).await;
    }
    // The operator's policy as it stands at delivery, as the checks applied it.
    let body = archon_pipeline::pr_body(&plan.body, run.item.as_str(), repo.attribution);
    let request = remote::Request {
        branch: &run.branch,
        base: &base,
        title: &plan.record.title,
        body: &body,
        draft,
    };
    match deliver_pr(ctx, run, &repo, &commit, request).await? {
        Ok(url) => Ok(Carried::Done(ShipOutcome::Shipped {
            commit,
            pr_url: Some(url),
            updated: false,
        })),
        Err(refusal) => refused(refusal),
    }
}

/// Push `commit` into the existing pull request `onto` names, by fast-forward,
/// leaving its title, body and base as they are.
async fn into_existing(
    ctx: &DaemonContext,
    repo: &Repo,
    onto: &OntoPullRequest,
    commit: String,
) -> Result<Carried, ArchonError> {
    assert!(
        crate::worktree::stack::is_commit_id(&commit),
        "the checked commit"
    );
    let outcome = match existing::deliver(ctx, repo, onto, &commit).await? {
        Ok(url) => ShipOutcome::Shipped {
            commit,
            pr_url: Some(url),
            updated: true,
        },
        Err(error) => ShipOutcome::Failed { error },
    };
    assert!(
        !matches!(&outcome, ShipOutcome::Shipped { pr_url: Some(url), .. } if *url != onto.pr.url()),
        "only the frozen pull request is updated"
    );
    Ok(Carried::Done(outcome))
}

/// The run branch's head once the worktree holds what the record names
/// ([`checks::check`]), or why it does not.
async fn prove(
    ctx: &DaemonContext,
    claimed: &Claimed,
    repo: &Repo,
    base: &str,
) -> Result<Result<String, String>, ArchonError> {
    let identity = repo_identity::of(ctx, repo).await?;
    let run = &claimed.run;
    let target = checks::Target {
        data_dir: ctx.paths.data_dir.clone(),
        repo_path: repo.path.clone(),
        workspace: run.workspace.clone(),
        branch: run.branch.clone(),
        base: base.to_owned(),
        since: claimed.onto.as_ref().map(|onto| onto.head_sha.clone()),
        allowed: repo_identity::RepoIdentity::allowed(identity.as_ref()),
        attribution: repo.attribution,
    };
    let own = repo_identity::DAEMON_EMAIL;
    assert!(
        target.allowed.iter().any(|e| e == own),
        "archond's own merges always ship"
    );
    let record = claimed.plan.record.clone();
    let checked = tokio::task::spawn_blocking(move || checks::check(&target, &record))
        .await
        .map_err(|e| ArchonError::io("delivery check task", std::io::Error::other(e)))??;
    let commit = match checked {
        Ok(checked) => checked.commit,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let full = crate::worktree::stack::is_commit_id(&commit);
    assert!(full, "the checks prove a full commit id: {commit:?}");
    Ok(Ok(commit))
}

/// What the ship gate says of the proved tree.
#[derive(Debug)]
enum GateStep {
    /// Its check passed, or the repository has no trusted one.
    Pass,
    Refuse(String),
    /// Its check is to run before the delivery goes on.
    Run(gate::Check),
}

/// The ship gate's say on the tree `claimed` records: the verdict on that
/// tree, else the trusted fast check to run on it, if the repository has one.
fn gate_step(
    ctx: &DaemonContext,
    claimed: &Claimed,
    repo: &Repo,
    base: &str,
    gate: Option<Result<gate::Verdict, ArchonError>>,
) -> Result<GateStep, ArchonError> {
    let tree = &claimed.plan.record.tree;
    assert!(!tree.is_empty() && !base.is_empty(), "a proved delivery");
    match gate {
        Some(Err(error)) => return Err(error),
        Some(Ok(verdict)) if verdict.tree == *tree => {
            return Ok(verdict.refusal.map_or(GateStep::Pass, GateStep::Refuse));
        }
        Some(Ok(_)) | None => {}
    }
    let Some(template) = fast_check::trusted(repo)? else {
        return Ok(GateStep::Pass);
    };
    let env = crate::resolve::RepoEnv::new(Some(repo.clone()))?;
    let mode = env.config.build_cache(&ctx.config.worktrees);
    let run = &claimed.run;
    let target_dir = crate::build_cache::target_dir(&ctx.paths, &repo.name, &run.item, mode);
    Ok(GateStep::Run(gate::Check {
        tree: tree.clone(),
        workspace: run.workspace.clone(),
        base: base.to_owned(),
        template,
        target_dir,
    }))
}

/// Push `commit` once it holds the stack it is on, then find or open its
/// pull request and settle the stack and initiative it belongs to.
async fn deliver_pr(
    ctx: &DaemonContext,
    run: &PipelineRun,
    repo: &Repo,
    commit: &str,
    request: remote::Request<'_>,
) -> Result<Result<String, String>, ArchonError> {
    assert_eq!(request.branch, run.branch, "the run branch is delivered");
    assert!(
        crate::worktree::stack::is_commit_id(commit),
        "the checked commit"
    );
    match initiative_ship::verify_stack(ctx, run, commit).await {
        Ok(()) => {}
        Err(ArchonError::Invalid(why)) => return Ok(Err(why)),
        Err(error) => return Err(error),
    }
    let data_dir = &ctx.paths.data_dir;
    if let Err(refusal) = remote::push(data_dir, &repo.path, &run.branch, commit).await? {
        return Ok(Err(refusal));
    }
    let gh = &ctx.config.github.binary;
    let url = match remote::pull_request(gh, &repo.path, request).await? {
        Ok(url) => url,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match initiative_ship::record(ctx, run, &url).await {
        Ok(()) => Ok(Ok(url)),
        Err(ArchonError::Invalid(why)) => Ok(Err(why)),
        Err(error) => Err(error),
    }
}

/// Whether the run's ship mode pushes and opens a pull request (`pr`) or
/// leaves the commits on the run branch (`local`).
fn pushes(vars: &BTreeMap<String, String>) -> Result<bool, ArchonError> {
    match vars.get("ship.mode").map(String::as_str) {
        Some("pr") => Ok(true),
        Some("local") => Ok(false),
        mode => Err(ArchonError::Protocol(format!(
            "invalid recorded ship mode: {mode:?}"
        ))),
    }
}

/// The branch the run's change is for: the one its item is stacked on now
/// (§8.8), which moves as parents merge, else the base its worktree forked from.
pub(in crate::executor) fn base_of(
    store: &Store,
    run: &PipelineRun,
    vars: &BTreeMap<String, String>,
) -> Result<String, ArchonError> {
    if let Some(stacked) = restack::target_base(store, &run.item)? {
        assert_ne!(stacked, run.branch, "an item is never stacked on itself");
        return Ok(stacked);
    }
    let forked = vars.get("base_branch").map(|base| base.trim());
    match forked.filter(|base| !base.is_empty()) {
        Some(base) => Ok(base.to_owned()),
        None => Err(ArchonError::Protocol(
            "the run's variables name no base branch".into(),
        )),
    }
}

/// Persist `outcome` for a rebuilt driver to replay. A pull request's URL
/// goes on the stage run that named the change, its identity on the run.
fn persist(
    store: &mut Store,
    run: &PipelineRun,
    plan: &ShipPlan,
    outcome: &ShipOutcome,
) -> Result<(), ArchonError> {
    if let ShipOutcome::Shipped {
        pr_url: Some(url), ..
    } = outcome
    {
        assert!(url.starts_with("https://"), "a pull request GitHub listed");
        let stages = store.stage_runs(&run.id)?;
        let wrote = |s: &&archon_core::run::StageRun| {
            s.stage_id == plan.record_stage && s.state == StageState::Completed
        };
        let Some(record) = stages.iter().rev().find(wrote) else {
            return Err(ArchonError::Protocol(format!(
                "run {} ships a record no completed `{}` stage wrote",
                run.id, plan.record_stage
            )));
        };
        let mut stage = record.clone();
        let data = stage.structured.take();
        let mut data = data
            .and_then(|d| d.as_object().cloned())
            .unwrap_or_default();
        data.insert("pr_url".into(), url.clone().into());
        stage.structured = Some(data.into());
        store.upsert_stage_run(&stage)?;
        super::super::pull_requests::record_delivery(store, &run.id, url)?;
    }
    store.kv_set(&ship_key(&run.id), &serde_json::to_string(outcome)?)
}

/// Let the driver finish the run with `outcome`, which is persisted.
async fn apply(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    outcome: ShipOutcome,
) -> Result<FollowUp, ArchonError> {
    assert!(!table.shipping.contains(&run.id), "the delivery has ended");
    match &outcome {
        ShipOutcome::Shipped { commit, pr_url, .. } => {
            tracing::info!(run = %run.id, %commit, pr = ?pr_url, "run delivered");
        }
        ShipOutcome::Failed { error } => {
            tracing::info!(run = %run.id, %error, "run not delivered");
        }
    }
    let all = ctx.store.lock().await.stage_runs(&run.id)?;
    let Some(driver) = table.drivers.get_mut(&run.id) else {
        // An earlier rebuild failed; retry it, which replays the outcome.
        return recover::resume_run(ctx, table, run, &all).await;
    };
    let actions = driver.on_shipped(outcome, &all)?;
    settle::apply(ctx, table, run, actions).await
}

/// A delivery failed in a way another try may pass: queue it again for the
/// sweep, to run after a wait, or fail the run once the tries are used up.
async fn retry_or_fail(
    ctx: &DaemonContext,
    table: &mut RunTable,
    run: PipelineRun,
    plan: &ShipPlan,
    error: ArchonError,
) -> Result<FollowUp, ArchonError> {
    let failures = table.ship_retries.get(&run.id).map_or(0, |r| r.failures) + 1;
    assert!(
        failures <= MAX_ATTEMPTS,
        "a failed run is delivered no more"
    );
    if failures < MAX_ATTEMPTS {
        let wait = FIRST_RETRY_WAIT * 2u32.pow(failures - 1);
        tracing::warn!(run = %run.id, %error, failures, ?wait, "delivery failed; retrying");
        let not_before = Instant::now() + wait;
        let retry = ShipRetry {
            failures,
            not_before,
        };
        save_retry(&mut *ctx.store.lock().await, &run.id, retry)?;
        table.ship_retries.insert(run.id.clone(), retry);
        table.ships.insert(run.id.clone(), plan.clone());
        return Ok(FollowUp::default());
    }
    let outcome = ShipOutcome::Failed {
        error: format!("delivery failed {failures} times, last with: {error}"),
    };
    persist(&mut *ctx.store.lock().await, &run, plan, &outcome)?;
    apply(ctx, table, run, outcome).await
}

/// Persist `retry`, so a restart neither forgets the failed tries nor the wait.
fn save_retry(store: &mut Store, run: &PipelineRunId, retry: ShipRetry) -> Result<(), ArchonError> {
    assert!(
        retry.failures > 0 && retry.failures < MAX_ATTEMPTS,
        "a retry is owed"
    );
    let wait = retry.not_before.saturating_duration_since(Instant::now());
    let wait = chrono::Duration::from_std(wait)
        .map_err(|e| ArchonError::Invalid(format!("delivery retry wait {wait:?}: {e}")))?;
    let persisted = PersistedRetry {
        failures: retry.failures,
        next_at: archon_core::now() + wait,
    };
    store.kv_set(&retry_key(run), &serde_json::to_string(&persisted)?)
}

/// The persisted retry of `run`'s delivery, its wait counted from now.
fn load_retry(store: &Store, run: &PipelineRunId) -> Result<Option<ShipRetry>, ArchonError> {
    let key = retry_key(run);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let persisted: PersistedRetry =
        serde_json::from_str(&raw).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
    if persisted.failures == 0 || persisted.failures >= MAX_ATTEMPTS {
        return Err(ArchonError::Store(format!(
            "kv {key}: {} failures",
            persisted.failures
        )));
    }
    let left = (persisted.next_at - archon_core::now())
        .to_std()
        .unwrap_or_default();
    Ok(Some(ShipRetry {
        failures: persisted.failures,
        not_before: Instant::now() + left,
    }))
}

/// Queue a delivery whose outcome was half acted on again, dropping the
/// driver so the retry rebuilds it from the store and replays the outcome.
fn requeue(table: &mut RunTable, run_id: &PipelineRunId, plan: ShipPlan) {
    assert!(
        !table.shipping.contains(run_id),
        "the failed delivery is not in flight"
    );
    tracing::warn!(run = %run_id, "delivery outcome not applied; retrying");
    table.drivers.remove(run_id);
    table.ships.entry(run_id.clone()).or_insert(plan);
    debug_assert!(table.ships.contains_key(run_id), "the sweep retries it");
}

/// Whether `run` has a delivery queued, running or recorded: its stages may
/// no longer change, or it would ship what they replaced.
pub(in crate::executor) fn delivering(
    table: &RunTable,
    store: &Store,
    run: &PipelineRunId,
) -> Result<bool, ArchonError> {
    let live = table.ships.contains_key(run) || table.shipping.contains(run);
    assert!(
        !table.shipping.contains(run) || table.ships.contains_key(run),
        "in flight is queued"
    );
    Ok(live || store.kv_get(&ship_key(run))?.is_some())
}

/// Feed `driver` the delivery outcome persisted for `run`, if any.
pub(in crate::executor) fn replay(
    store: &Store,
    run: &PipelineRunId,
    driver: &mut RunDriver,
) -> Result<(), ArchonError> {
    let key = ship_key(run);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(());
    };
    let outcome: ShipOutcome =
        serde_json::from_str(&raw).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
    driver.record_ship(outcome)
}

/// The commit `run` shipped, while its delivery outcome is still kept.
pub(crate) fn shipped_commit(
    store: &Store,
    run: &PipelineRunId,
) -> Result<Option<String>, ArchonError> {
    let key = ship_key(run);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let outcome: ShipOutcome =
        serde_json::from_str(&raw).map_err(|e| ArchonError::Store(format!("kv {key}: {e}")))?;
    Ok(match outcome {
        ShipOutcome::Shipped { commit, .. } => {
            assert!(!commit.is_empty(), "a shipped outcome names its commit");
            Some(commit)
        }
        ShipOutcome::Failed { .. } => None,
    })
}

/// Delete the delivery outcomes of finished runs, which only rebuild a live
/// run's driver, and of runs that no longer exist. Returns how many went.
pub(crate) fn prune_finished_ships(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for prefix in [SHIP_KEY_PREFIX, RETRY_KEY_PREFIX] {
        pruned += prune_finished(store, prefix)?;
    }
    Ok(pruned)
}

fn prune_finished(store: &mut Store, prefix: &str) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(prefix)? {
        let raw_run = key.strip_prefix(prefix).expect("ranged by prefix");
        let run = PipelineRunId::parse(raw_run)
            .map_err(|e| ArchonError::Store(format!("delivery key {key}: {e}")))?;
        let finished = match store.run(&run) {
            Ok(found) => found.state.is_terminal(),
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if finished {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Whether `run`'s queued delivery may be tried at `now`.
fn due(table: &RunTable, run: &PipelineRunId, now: Instant) -> bool {
    table
        .ship_retries
        .get(run)
        .is_none_or(|retry| retry.not_before <= now)
}

/// Runs whose delivery is queued, not running and due: the sweep tries them.
pub(in crate::executor) fn stalled(table: &RunTable, now: Instant) -> Vec<PipelineRunId> {
    let waiting = |run: &&PipelineRunId| !table.shipping.contains(*run) && due(table, run, now);
    table.ships.keys().filter(waiting).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let owned = |(k, v): &(&str, &str)| ((*k).to_owned(), (*v).to_owned());
        pairs.iter().map(owned).collect()
    }

    #[test]
    fn the_ship_mode_decides_whether_anything_leaves_the_machine() {
        assert!(pushes(&vars(&[("ship.mode", "pr")])).unwrap());
        assert!(!pushes(&vars(&[("ship.mode", "local")])).unwrap());
        assert!(pushes(&vars(&[("ship.mode", "force")])).is_err());
        assert!(pushes(&vars(&[])).is_err());
    }

    /// A failed delivery waits for its next try; one in flight is left alone.
    #[test]
    fn the_sweep_tries_a_queued_delivery_only_once_it_is_due() {
        let now = Instant::now();
        let (waiting, ready, running) = (
            PipelineRunId::new(),
            PipelineRunId::new(),
            PipelineRunId::new(),
        );
        let plan = ShipPlan {
            record_stage: "implement".into(),
            record: archon_pipeline::ShipRecord {
                commit: "1".repeat(40),
                tree: "2".repeat(40),
                title: "t".into(),
                body: "b".into(),
                checks: Vec::new(),
            },
            body: "b".into(),
            summary: "s".into(),
        };
        let mut table = RunTable::default();
        for run in [&waiting, &ready, &running] {
            table.ships.insert(run.clone(), plan.clone());
        }
        table.shipping.insert(running.clone());
        let later = ShipRetry {
            failures: 1,
            not_before: now + FIRST_RETRY_WAIT,
        };
        table.ship_retries.insert(waiting.clone(), later);
        assert_eq!(stalled(&table, now), std::slice::from_ref(&ready));
        let mut due_now = stalled(&table, now + FIRST_RETRY_WAIT);
        due_now.sort();
        let mut expected = vec![waiting, ready];
        expected.sort();
        assert_eq!(due_now, expected, "the wait is over");
    }
}
