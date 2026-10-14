//! The delivery archond makes itself (ARCHITECTURE §8.7) once a driver asks
//! for it: prove the run worktree, push its branch, find or open the pull
//! request, then let the driver finish the run with how that ended.
//!
//! Git and `gh` run without any lock. The outcome is persisted in `kv`
//! before the driver acts on it, so a rebuilt driver replays it; a crash
//! before that repeats the delivery, which pushes nothing twice and reuses
//! the pull request GitHub lists.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::notify::NotificationKind;
use archon_core::run::{PipelineRun, StageState};
use archon_pipeline::{RunDriver, ShipOutcome, ShipPlan};
use archon_queue::Store;

use super::super::{FollowUp, RunTable, ShipRetry, descendants, finish, recover, settle};
use super::{checks, remote};
use crate::context::DaemonContext;
use crate::resolve;

/// `kv`: `ship:<run>` → the JSON [`ShipOutcome`].
const SHIP_KEY_PREFIX: &str = "ship:";
/// Deliveries tried before a failure another try might pass fails the run.
const MAX_ATTEMPTS: u32 = 5;
/// The wait before the second try; each later one waits twice as long.
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(15);

fn ship_key(run: &PipelineRunId) -> String {
    format!("{SHIP_KEY_PREFIX}{run}")
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
    repo_path: Option<PathBuf>,
    vars: BTreeMap<String, String>,
    /// The outcome persisted already, if any.
    recorded: Option<ShipOutcome>,
}

/// Run `run`'s queued delivery once and let its driver finish the run. A
/// refusal fails the run with its reason; a failure another try may pass
/// (the network, GitHub) is left queued for the sweep, a bounded number of
/// times. An outcome persisted already is never delivered again.
pub(in crate::executor) async fn deliver(
    ctx: &DaemonContext,
    run_id: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    let Some(claimed) = claim(ctx, run_id).await? else {
        return Ok(FollowUp::default());
    };
    let delivered = match claimed.recorded.clone() {
        Some(outcome) => Ok(outcome),
        None => fresh(ctx, &claimed).await,
    };
    let Claimed { plan, run, .. } = claimed;
    let mut table = ctx.runs.lock().await;
    let released = table.shipping.remove(run_id);
    assert!(released, "a claimed delivery is in flight until it ends");
    if table.ships.get(run_id) != Some(&plan) {
        // The run finished or was resumed meanwhile; its replay owns it now.
        return Ok(FollowUp::poke());
    }
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

/// The queued delivery of a live run, marked in flight; `None` when there
/// is none, another caller runs it, its next try is not due yet, or what the
/// run's agents delegated is still to be stopped.
async fn claim(
    ctx: &DaemonContext,
    run_id: &PipelineRunId,
) -> Result<Option<Claimed>, ArchonError> {
    let mut table = ctx.runs.lock().await;
    let Some(plan) = table.ships.get(run_id).cloned() else {
        return Ok(None);
    };
    if table.shipping.contains(run_id) || !due(&table, run_id, Instant::now()) {
        return Ok(None);
    }
    let store = ctx.store.lock().await;
    let run = store.run(run_id)?;
    if run.state.is_terminal() {
        table.ships.remove(run_id);
        return Ok(None);
    }
    // An agent one of the run's stages delegated to may still write the worktree.
    if descendants::holds(&store, run_id)? {
        return Ok(None);
    }
    let recorded = store
        .kv_get(&ship_key(run_id))?
        .map(|raw| serde_json::from_str::<ShipOutcome>(&raw))
        .transpose()
        .map_err(|e| ArchonError::Store(format!("delivery outcome of run {run_id}: {e}")))?;
    let claimed = Claimed {
        plan,
        repo_path: repo_path(&store, &run)?,
        vars: recover::load_run_vars(&store, run_id)?,
        recorded,
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

/// Where the repository of `run`'s item is; `None` once it is unregistered.
fn repo_path(store: &Store, run: &PipelineRun) -> Result<Option<PathBuf>, ArchonError> {
    let Some(repo) = store.item(&run.item)?.repo else {
        return Ok(None);
    };
    match store.repo(&resolve::repo_ref(&repo)) {
        Ok(found) => {
            assert!(found.path.is_absolute(), "repository paths are canonical");
            Ok(Some(found.path))
        }
        Err(ArchonError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Deliver `claimed` now and persist how it ended, before anything acts on
/// it. `Err` is a failure another try may pass; nothing is persisted then.
async fn fresh(ctx: &DaemonContext, claimed: &Claimed) -> Result<ShipOutcome, ArchonError> {
    assert!(
        claimed.recorded.is_none(),
        "a recorded delivery is not repeated"
    );
    let outcome = carry_out(ctx, claimed).await?;
    let (run, plan) = (&claimed.run, &claimed.plan);
    persist(&mut *ctx.store.lock().await, run, plan, &outcome)?;
    if let ShipOutcome::Shipped {
        pr_url: Some(url), ..
    } = &outcome
    {
        merge_when_ready(ctx, claimed, url).await;
    }
    Ok(outcome)
}

/// Prove the worktree, then in `pr` mode push the branch and find or open
/// its pull request. A refusal is the `Failed` outcome, never an `Err`.
async fn carry_out(ctx: &DaemonContext, claimed: &Claimed) -> Result<ShipOutcome, ArchonError> {
    let refused = |error: String| Ok(ShipOutcome::Failed { error });
    let (run, plan) = (&claimed.run, &claimed.plan);
    let Some(repo_path) = claimed.repo_path.clone() else {
        return refused("the item's repository is no longer registered with Archon".into());
    };
    let pushes = pushes(&claimed.vars)?;
    let target = checks::Target {
        repo_path,
        workspace: run.workspace.clone(),
        branch: run.branch.clone(),
        base: base_of(&claimed.vars)?,
    };
    let (base, record) = (target.base.clone(), plan.record.clone());
    let checked = tokio::task::spawn_blocking(move || checks::check(&target, &record))
        .await
        .map_err(|e| ArchonError::io("delivery check task", std::io::Error::other(e)))??;
    let commit = match checked {
        Ok(checked) => checked.commit,
        Err(refusal) => return refused(refusal),
    };
    if !pushes {
        return Ok(ShipOutcome::Shipped {
            commit,
            pr_url: None,
        });
    }
    if let Err(refusal) = remote::push(&run.workspace, &run.branch).await? {
        return refused(refusal);
    }
    let request = remote::Request {
        branch: &run.branch,
        base: &base,
        title: &plan.record.title,
        body: &plan.body,
    };
    let gh = &ctx.config.github.binary;
    match remote::pull_request(gh, &run.workspace, request).await? {
        Ok(url) => Ok(ShipOutcome::Shipped {
            commit,
            pr_url: Some(url),
        }),
        Err(refusal) => refused(refusal),
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

/// The branch the run's change is for: the branch of the item it is stacked
/// on, when its start recorded one, else the repository's base branch.
fn base_of(vars: &BTreeMap<String, String>) -> Result<String, ArchonError> {
    let named = |key: &str| vars.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
    let base = named("stack_parent").or_else(|| named("base_branch"));
    let Some(base) = base else {
        return Err(ArchonError::Protocol(
            "the run's variables name no base branch".into(),
        ));
    };
    Ok(base.to_owned())
}

/// Enable auto-merge where the repository asks for it. The change is
/// delivered either way, so a refusal goes to the run's progress.
async fn merge_when_ready(ctx: &DaemonContext, claimed: &Claimed, url: &str) {
    if claimed.vars.get("ship.auto_merge").map(String::as_str) != Some("true") {
        return;
    }
    let run = &claimed.run;
    let gh = &ctx.config.github.binary;
    let Err(why) = remote::auto_merge(gh, &run.workspace, url).await else {
        return;
    };
    let text = format!(
        "Run {}: {url} is open, but auto-merge could not be enabled: {why}",
        run.id
    );
    let kind = NotificationKind::Progress;
    let posted = finish::notify(ctx, kind, &text, Some(&run.item), Some(&run.id)).await;
    if let Err(error) = posted {
        tracing::error!(run = %run.id, %error, notice = %text, "auto-merge notice not posted");
    }
}

/// Persist `outcome` for a rebuilt driver to replay. A pull request's URL
/// goes on the stage run that named the change, where a run's is read from.
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
        ShipOutcome::Shipped { commit, pr_url } => {
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

/// Acting on a delivery's outcome failed part way: queue it again for the
/// sweep, and drop the driver, which may have acted already, so the retry
/// rebuilds it from the store. A persisted outcome is replayed, not redone.
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

/// Delete the delivery outcomes of finished runs, which only rebuild a live
/// run's driver, and of runs that no longer exist. Returns how many went.
pub(crate) fn prune_finished_ships(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(SHIP_KEY_PREFIX)? {
        let raw_run = key.strip_prefix(SHIP_KEY_PREFIX).expect("ranged by prefix");
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

    /// W2 records the branch an item is stacked on in the run's variables.
    #[test]
    fn a_stacked_run_is_for_its_parent_s_branch_and_any_other_for_the_base() {
        let plain = vars(&[("base_branch", "main")]);
        assert_eq!(base_of(&plain).unwrap(), "main");
        let stacked = vars(&[("base_branch", "main"), ("stack_parent", "archon/parent")]);
        assert_eq!(base_of(&stacked).unwrap(), "archon/parent");
        let blank = vars(&[("base_branch", "main"), ("stack_parent", " ")]);
        assert_eq!(base_of(&blank).unwrap(), "main", "an empty parent is none");
        assert!(base_of(&vars(&[])).is_err());
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
