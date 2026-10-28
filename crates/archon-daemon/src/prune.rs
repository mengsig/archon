//! The periodic pass that bounds the store (ARCHITECTURE §16): it deletes
//! `kv` entries nothing reads any more, and settled history older than
//! `storage.retention_days`, then hands freed pages back and cuts the WAL back.
//! Outcomes and lessons stay for learning.

use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_queue::Checkpointer;
use archon_queue::store::HistoryPruned;

use crate::context::DaemonContext;
use crate::worktree::team::SET_ASIDE_DIR;
use crate::{executor, gc, inbox_health, pump, reports, supervisor, tracked};

/// How often the pass runs; the first runs at startup.
const INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Settled items deleted per store lock, and batches per pass: a large
/// backlog goes over several passes without holding the store long.
const HISTORY_BATCH: usize = 200;
const HISTORY_BATCHES: usize = 25;

/// How long entries that late Aether events or the operator may still
/// need are kept after their owner finished.
pub fn retention() -> chrono::Duration {
    chrono::Duration::days(7)
}

/// What one pass deleted, by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub spawn_requests: usize,
    pub run_vars: usize,
    pub team_merges: usize,
    pub ships: usize,
    pub team_rosters: usize,
    pub node_agents: usize,
    pub children: usize,
    pub stage_agents: usize,
    pub unread_inboxes: usize,
    pub reports: usize,
    pub retired_keys: usize,
    pub retired_orchestrators: usize,
    pub tracked_requests: usize,
    pub history: HistoryPruned,
}

impl Pruned {
    pub fn total(&self) -> usize {
        self.spawn_requests
            + self.run_vars
            + self.team_merges
            + self.ships
            + self.team_rosters
            + self.node_agents
            + self.children
            + self.stage_agents
            + self.unread_inboxes
            + self.reports
            + self.retired_keys
            + self.retired_orchestrators
            + self.tracked_requests
            + self.history.total()
    }
}

/// Prune every `INTERVAL` until shutdown. Only a store failure ends it;
/// any other failure is logged and the next pass tries again.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        match prune_once(&ctx, archon_core::now()).await {
            Ok(pruned) if pruned.total() > 0 => tracing::info!(?pruned, "pruned the store"),
            Ok(_) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::warn!(%error, "pruning the store failed; the next pass retries"),
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// One pass as of `now`. Entries of settled stages, finished runs and
/// items, and keys no version reads any more, go at once; node agents, watched
/// children, stage agents, replaced orchestrators and settled reports go
/// once older than [`retention`], finished tracked requests after a day,
/// once outcomes the store refused before are recorded. Settled history goes
/// last, after the reports and tracked requests that name it. The store is
/// locked per kind, not for the whole pass.
pub async fn prune_once(ctx: &DaemonContext, now: Timestamp) -> Result<Pruned, ArchonError> {
    let cutoff = now - retention();
    assert!(cutoff < now, "the retention is positive");
    // One statement per lock: a guard lives until its statement ends.
    let spawn_requests = category(
        "spawn_requests",
        executor::prune_settled_requests(&mut *ctx.store.lock().await),
    )?;
    let run_vars = category(
        "run_vars",
        executor::prune_finished_run_vars(&mut *ctx.store.lock().await),
    )?;
    let team_merges = category(
        "team_merges",
        executor::prune_finished_merges(&mut *ctx.store.lock().await),
    )?;
    let ships = category(
        "ships",
        executor::prune_finished_ships(&mut *ctx.store.lock().await),
    )?;
    let team_rosters = category(
        "team_rosters",
        executor::prune_settled_rosters(&mut *ctx.store.lock().await),
    )?;
    let node_agents = category(
        "node_agents",
        executor::prune_retired_node_agents(&mut *ctx.store.lock().await, cutoff),
    )?;
    let reports = {
        let _files = ctx.report_files.lock().await;
        let pruned = reports::prune(&mut *ctx.store.lock().await, &ctx.paths.reports, cutoff);
        category("reports", pruned)?
    };
    let retired_keys = category(
        "retired_keys",
        supervisor::prune_retired_keys(&mut *ctx.store.lock().await),
    )?;
    let children = category("children", pump::prune_children(ctx, now, cutoff).await)?;
    let stage_agents = category("stage_agents", pump::prune_stage_agents(ctx, cutoff).await)?;
    let unread_inboxes = category(
        "unread_inboxes",
        inbox_health::prune_finished(&mut *ctx.store.lock().await),
    )?;
    let retired_orchestrators = category(
        "retired_orchestrators",
        supervisor::prune_retired_orchestrators(ctx, cutoff).await,
    )?;
    tracked::record_kept(ctx).await;
    let tracked_requests = category(
        "tracked_requests",
        ctx.store.lock().await.prune_tracked(tracked_cutoff(now)),
    )?;
    let history = prune_history(ctx, now).await?;
    Ok(Pruned {
        spawn_requests,
        run_vars,
        team_merges,
        ships,
        team_rosters,
        node_agents,
        children,
        stage_agents,
        unread_inboxes,
        reports,
        retired_keys,
        retired_orchestrators,
        tracked_requests,
        history,
    })
}

/// When settled history goes: `storage.retention_days` ago, never sooner than
/// the merge watcher or artifact expiry still read it; `None` when disabled.
pub fn history_cutoff(config: &Config, now: Timestamp) -> Result<Option<Timestamp>, ArchonError> {
    let days = config.storage.retention_days;
    if days == 0 {
        return Ok(None);
    }
    let watched = i64::from(days).max(executor::pull_requests::WATCHED_DAYS);
    let mut cutoff = now - chrono::Duration::days(watched);
    let artifacts = config.artifacts.keep_after;
    if artifacts.secs() != 0 {
        let Some(expired) = now.checked_sub_signed(artifacts.chrono()?) else {
            return Ok(None);
        };
        cutoff = cutoff.min(expired);
    }
    assert!(cutoff < now, "the retention is positive");
    Ok(Some(cutoff))
}

/// Delete settled history a batch per store lock, then hand the freed pages
/// back and cut the WAL back. A run workspace on disk keeps its item for gc.
async fn prune_history(ctx: &DaemonContext, now: Timestamp) -> Result<HistoryPruned, ArchonError> {
    let Some(before) = history_cutoff(&ctx.config, now)? else {
        return Ok(HistoryPruned::default());
    };
    let mut pruned = HistoryPruned::default();
    for _ in 0..HISTORY_BATCHES {
        let batch = ctx
            .store
            .lock()
            .await
            .prune_history(before, HISTORY_BATCH, on_disk)?;
        assert!(batch.items <= HISTORY_BATCH && batch.feed <= HISTORY_BATCH);
        pruned.absorb(batch);
        if batch.items < HISTORY_BATCH && batch.feed < HISTORY_BATCH {
            break;
        }
    }
    if pruned.total() == 0 {
        return Ok(pruned);
    }
    ctx.store.lock().await.vacuum_freed()?;
    let path = ctx.paths.store.clone();
    let truncated = tokio::task::spawn_blocking(move || Checkpointer::open(&path)?.truncate())
        .await
        .map_err(|e| ArchonError::io("the WAL truncate task", std::io::Error::other(e)))??;
    if !truncated {
        tracing::debug!("a reader holds the WAL; the next pass truncates it");
    }
    Ok(pruned)
}

/// Whether gc may still find something through the run workspace `path`: it
/// or a flat copy a fresh retry set aside beside it. Unknown counts as yes.
fn on_disk(path: &Path) -> bool {
    match path.symlink_metadata() {
        Ok(_) => true,
        Err(error) if error.kind() != ErrorKind::NotFound => true,
        Err(_) => set_aside_beside(path),
    }
}

/// Whether `<parent>/.set-aside/<name>-<n>` may exist for the flat worktree
/// `workspace`, as gc's set-aside sweep looks for it.
fn set_aside_beside(workspace: &Path) -> bool {
    let (Some(parent), Some(name)) = (workspace.parent(), workspace.file_name()) else {
        return false;
    };
    let entries = match std::fs::read_dir(parent.join(SET_ASIDE_DIR)) {
        Ok(entries) => entries,
        Err(error) => return error.kind() != ErrorKind::NotFound,
    };
    entries
        .into_iter()
        .any(|entry| entry.map_or(true, |e| gc::set_aside_of(name, &e.file_name())))
}

/// Tracked requests are kept [`archon_core::tracked::RETENTION`], not [`retention`].
fn tracked_cutoff(now: Timestamp) -> Timestamp {
    let kept = chrono::Duration::from_std(archon_core::tracked::RETENTION)
        .expect("the retention fits a chrono duration");
    let cutoff = now - kept;
    assert!(
        cutoff < now && cutoff > now - retention(),
        "kept a day, less than the rest"
    );
    cutoff
}

/// One kind of record that cannot be read ends that kind's pruning for this
/// pass, with a log line, and the other kinds still go; a store failure
/// still fails the pass.
fn category(kind: &str, pruned: Result<usize, ArchonError>) -> Result<usize, ArchonError> {
    match pruned {
        Err(error) if !error.is_store() => {
            tracing::error!(%kind, %error, "pruning skipped a kind of record it cannot read; run `archon doctor`");
            Ok(0)
        }
        other => other,
    }
}
