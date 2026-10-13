//! The periodic pass that deletes `kv` entries nothing reads any more
//! (ARCHITECTURE §16), so the store follows live work plus a retention
//! window rather than every run, child and report since installation.

use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;

use crate::context::DaemonContext;
use crate::{executor, pump, reports, supervisor};

/// How often the pass runs; the first runs at startup.
const INTERVAL: Duration = Duration::from_secs(10 * 60);

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
    pub team_rosters: usize,
    pub node_agents: usize,
    pub children: usize,
    pub stage_agents: usize,
    pub reports: usize,
    pub retired_keys: usize,
    pub retired_orchestrators: usize,
}

impl Pruned {
    pub fn total(&self) -> usize {
        self.spawn_requests
            + self.run_vars
            + self.team_merges
            + self.team_rosters
            + self.node_agents
            + self.children
            + self.stage_agents
            + self.reports
            + self.retired_keys
            + self.retired_orchestrators
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

/// One pass as of `now`. Entries of settled stages and finished runs, and
/// keys no version reads any more, go at once; node agents, watched
/// children, stage agents, replaced orchestrators and settled reports go
/// once older than [`retention`]. The store is locked per kind, not for the
/// whole pass.
pub async fn prune_once(ctx: &DaemonContext, now: Timestamp) -> Result<Pruned, ArchonError> {
    let cutoff = now - retention();
    assert!(cutoff < now, "the retention is positive");
    // One statement per lock: a guard lives until its statement ends.
    let spawn_requests = executor::prune_settled_requests(&mut *ctx.store.lock().await)?;
    let run_vars = executor::prune_finished_run_vars(&mut *ctx.store.lock().await)?;
    let team_merges = executor::prune_finished_merges(&mut *ctx.store.lock().await)?;
    let team_rosters = executor::prune_settled_rosters(&mut *ctx.store.lock().await)?;
    let node_agents = executor::prune_retired_node_agents(&mut *ctx.store.lock().await, cutoff)?;
    let reports = {
        let _files = ctx.report_files.lock().await;
        reports::prune(&mut *ctx.store.lock().await, &ctx.paths.reports, cutoff)?
    };
    let retired_keys = supervisor::prune_retired_keys(&mut *ctx.store.lock().await)?;
    let children = pump::prune_children(ctx, now, cutoff).await?;
    let stage_agents = pump::prune_stage_agents(ctx, cutoff).await?;
    let retired_orchestrators = supervisor::prune_retired_orchestrators(ctx, cutoff).await?;
    Ok(Pruned {
        spawn_requests,
        run_vars,
        team_merges,
        team_rosters,
        node_agents,
        children,
        stage_agents,
        reports,
        retired_keys,
        retired_orchestrators,
    })
}
