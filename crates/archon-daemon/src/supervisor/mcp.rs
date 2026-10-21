//! Keeping the `archon` MCP server alive in Aether (ARCHITECTURE §16).
//! Aether does not reconnect a server whose process died, and an agent
//! keeps the tool list it started with. So archond checks the server on
//! every connect to Aether and every minute. When Aether reports it failed,
//! archond refreshes it; once it is ready again, archond restarts the
//! orchestrator so it gets the tools back. Refreshes and restarts are paced
//! like orchestrator revivals, and ones that keep failing post one `Blocker`.

use std::sync::Arc;
use std::time::Duration;

use archon_aether::AetherClient;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::mcp_tools::SERVER_NAME;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{McpHealth, McpStatus};

use super::revival::{Backoff, REVIVAL_BACKOFF};
use crate::context::DaemonContext;
use crate::feed;

/// How often archond checks the server between connects to Aether.
pub const MCP_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Failed refreshes, or failed orchestrator restarts, in a row after which
/// the operator gets a `Blocker`.
pub const HEALS_BEFORE_BLOCKER: u32 = 3;
/// What the operator can do when archond keeps failing to heal.
const BY_HAND: &str = "To heal it by hand, run `archon aether mcp refresh archon`, then `archon orchestrator restart`.";

/// The supervisor's record of its heals. In memory: after an archond
/// restart the first check reads the server's state afresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealState {
    /// Refreshes of the current streak, paced as orchestrator revivals are.
    pacing: Backoff,
    /// Refreshes that failed in a row.
    failed_refreshes: u32,
    /// Orchestrator restarts that failed in a row, and when the latest ran;
    /// the next one waits as a revival would.
    failed_relinks: Backoff,
    /// When a refresh last brought the server back.
    last_heal: Option<Timestamp>,
    /// The server failed since the orchestrator started, so the
    /// orchestrator needs a restart once the server is ready.
    relink_pending: bool,
    /// A check saw the server failed and none has seen it ready since.
    server_down: bool,
}

impl HealState {
    /// The server is ready at `now`. Returns whether a failure `Blocker`
    /// was standing. A streak whose last refresh is the longest pause old
    /// closes, so a later failure is refreshed at once.
    fn ready(&mut self, now: Timestamp) -> bool {
        let blocked = self.failed_refreshes >= HEALS_BEFORE_BLOCKER;
        self.failed_refreshes = 0;
        self.server_down = false;
        let longest = chrono::TimeDelta::from_std(REVIVAL_BACKOFF[REVIVAL_BACKOFF.len() - 1])
            .expect("backoff pauses fit a TimeDelta");
        if self.pacing.last.is_some_and(|last| now - last >= longest) {
            self.pacing = Backoff::default();
        }
        self.assert_consistent();
        blocked
    }

    /// A check saw the server failed: whatever orchestrator runs now, or
    /// starts before the server is ready, owes a restart.
    fn down(&mut self) {
        self.server_down = true;
        self.relink_pending = true;
        self.assert_consistent();
    }

    /// The orchestrator started against the server as it is now, so no
    /// restart is owed. Returns whether a failed-restart `Blocker` was standing.
    fn linked(&mut self) -> bool {
        assert!(
            !self.server_down,
            "an agent started on a down server lacks the tools"
        );
        let blocked = self.end_failed_relinks();
        self.relink_pending = false;
        self.assert_consistent();
        blocked
    }

    /// The orchestrator was replaced. On a server that is still down the
    /// new agent lacks the tools too, so its restart stays owed.
    /// Returns whether a failed-restart `Blocker` was standing.
    fn replaced(&mut self) -> bool {
        if !self.server_down {
            return self.linked();
        }
        let blocked = self.end_failed_relinks();
        assert!(self.relink_pending, "a down server owes a restart");
        self.assert_consistent();
        blocked
    }

    /// End the streak of failed restarts; whether it had reached a `Blocker`.
    fn end_failed_relinks(&mut self) -> bool {
        let blocked = self.failed_relinks.revivals >= HEALS_BEFORE_BLOCKER;
        self.failed_relinks = Backoff::default();
        blocked
    }

    fn assert_consistent(&self) {
        assert!(
            self.failed_refreshes <= self.pacing.revivals,
            "every failed refresh counts in the pacing"
        );
        assert_eq!(
            self.pacing.last.is_some(),
            self.pacing.revivals > 0,
            "refreshes and their time agree"
        );
        assert_eq!(
            self.failed_relinks.last.is_some(),
            self.failed_relinks.revivals > 0,
            "failed restarts and their time agree"
        );
        assert!(
            self.failed_relinks.revivals == 0 || self.relink_pending,
            "a failed restart stays owed"
        );
        assert!(
            !self.server_down || self.relink_pending,
            "a down server owes a restart"
        );
    }
}

/// The orchestrator was replaced (a revival, a successor, the operator's
/// restart or a relink): the new agent has the server's current tools, so
/// it owes a restart only if a check saw the server failed and none has
/// seen it ready since.
pub(super) async fn orchestrator_replaced(ctx: &DaemonContext) {
    let mut state = ctx.supervisor.mcp.lock().await;
    let pending = state.relink_pending;
    if state.replaced() {
        tracing::info!("the orchestrator was replaced; its failed restarts are over");
    }
    if state.server_down {
        tracing::info!(
            "the orchestrator was replaced while the archon MCP server is down; it is restarted once the server is ready"
        );
    } else if pending {
        tracing::info!("the orchestrator was replaced; it needs no restart for Archon's tools");
    }
}

/// The server's process was replaced (an Archon upgrade): whatever
/// orchestrator runs now holds the old tools, so it owes a restart.
pub async fn owe_orchestrator_restart(ctx: &DaemonContext) {
    let mut state = ctx.supervisor.mcp.lock().await;
    state.relink_pending = true;
    state.assert_consistent();
}

/// Check the server now rather than at the next interval: the pump calls
/// this on every connect to Aether.
pub fn check_mcp_soon(ctx: &DaemonContext) {
    ctx.supervisor.mcp_check.notify_one();
}

/// Checks the server on every [`check_mcp_soon`] and every
/// [`MCP_CHECK_INTERVAL`]. Only a store failure ends it; any other failure
/// is logged and the next check tries again.
pub async fn watch_mcp(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        tokio::select! {
            () = ctx.supervisor.mcp_check.notified() => {}
            () = tokio::time::sleep(MCP_CHECK_INTERVAL) => {}
        }
        match check_mcp(&ctx, archon_core::now()).await {
            Ok(health) => tracing::debug!(?health, "checked the archon MCP server"),
            Err(error) if error.is_store() => return Err(error),
            Err(error) if !ctx.aether.is_connected() => {
                tracing::debug!(%error, "archon MCP check skipped: Aether is not connected");
            }
            Err(error) => tracing::warn!(%error, "checking the archon MCP server failed"),
        }
    }
}

/// One check as of `now`: refresh a failed server when its pacing allows,
/// then restart the orchestrator once the server is ready again. Returns
/// the server's health after any refresh.
pub async fn check_mcp(ctx: &DaemonContext, now: Timestamp) -> Result<McpHealth, ArchonError> {
    let client = ctx.aether.client()?;
    let health = match client.mcp_health(SERVER_NAME).await? {
        McpHealth::Failed { error } => refresh(ctx, &client, &error, now).await?,
        other => other,
    };
    assert!(
        !matches!(health, McpHealth::Unreachable { .. }),
        "Aether answered, so it was reachable"
    );
    if matches!(health, McpHealth::Ready { .. }) {
        recovered(ctx, now).await?;
    }
    Ok(health)
}

/// Refresh the failed server unless the streak's pacing defers it.
async fn refresh(
    ctx: &DaemonContext,
    client: &AetherClient,
    error: &str,
    now: Timestamp,
) -> Result<McpHealth, ArchonError> {
    assert!(!error.is_empty(), "a failed server says why");
    let pacing = {
        let mut state = ctx.supervisor.mcp.lock().await;
        state.down();
        state.pacing
    };
    let failed = McpHealth::Failed {
        error: error.to_owned(),
    };
    if let Some(due) = pacing.deferred_until(now) {
        tracing::debug!(%error, %due, "the archon MCP server is down; its next refresh waits");
        return Ok(failed);
    }
    tracing::warn!(%error, "the archon MCP server failed in aetherd; refreshing it");
    let after = match client.refresh_mcp(SERVER_NAME).await {
        Ok(after) => after,
        // Losing Aether mid-refresh says nothing about the server.
        Err(lost) if !client.is_connected() => return Err(lost),
        Err(refused) => McpHealth::Failed {
            error: format!("the refresh failed: {refused}"),
        },
    };
    match &after {
        McpHealth::Ready { tools } => refreshed(ctx, error, *tools, now).await?,
        other => refresh_failed(ctx, error, other, now).await?,
    }
    Ok(after)
}

/// The refresh worked. Whatever orchestrator runs now may have started
/// while the server was down, so it owes a restart.
async fn refreshed(
    ctx: &DaemonContext,
    error: &str,
    tools: u32,
    now: Timestamp,
) -> Result<(), ArchonError> {
    assert!(!error.is_empty(), "a failed server says why");
    {
        let mut state = ctx.supervisor.mcp.lock().await;
        state.pacing = state.pacing.revived(now);
        state.last_heal = Some(now);
        state.relink_pending = true;
        state.assert_consistent();
    }
    tracing::info!(tools, "refreshed the archon MCP server");
    let text = format!(
        "The archon MCP server failed in Aether ({error}); Archon refreshed it and it is ready ({tools} tools)."
    );
    feed::system(ctx, text).await.map(drop)
}

async fn refresh_failed(
    ctx: &DaemonContext,
    error: &str,
    after: &McpHealth,
    now: Timestamp,
) -> Result<(), ArchonError> {
    assert!(
        !matches!(after, McpHealth::Ready { .. }),
        "a ready server was refreshed"
    );
    let (failures, due) = {
        let mut state = ctx.supervisor.mcp.lock().await;
        state.pacing = state.pacing.revived(now);
        state.failed_refreshes = state.failed_refreshes.saturating_add(1);
        state.assert_consistent();
        (state.failed_refreshes, state.pacing.deferred_until(now))
    };
    let why = describe(after);
    tracing::warn!(%error, %why, failures, "refreshing the archon MCP server failed");
    let text = format!(
        "The archon MCP server failed in Aether ({error}) and refreshing it did not help ({why}).{}",
        retry_note(due)
    );
    feed::system(ctx, text).await?;
    if failures == HEALS_BEFORE_BLOCKER {
        let problem = format!(
            "Archon could not bring back the archon MCP server in Aether: {failures} refreshes failed in a row (last: {why}). Until it recovers the orchestrator has none of Archon's tools; `archon doctor` shows the server's state."
        );
        keeps_failing(ctx, failures, &problem).await?;
    }
    Ok(())
}

/// The one `Blocker` of a streak of failed heals; `problem` says what failed.
async fn keeps_failing(
    ctx: &DaemonContext,
    failures: u32,
    problem: &str,
) -> Result<(), ArchonError> {
    assert_eq!(failures, HEALS_BEFORE_BLOCKER, "told once per streak");
    assert!(!problem.is_empty(), "the operator is told what failed");
    let most = REVIVAL_BACKOFF[REVIVAL_BACKOFF.len() - 1].as_secs() / 60;
    let text = format!(
        "{problem} Archon keeps trying, waiting longer each time and then every {most} minutes. {BY_HAND}"
    );
    feed::notify(ctx, NotificationKind::Blocker, text, None, None)
        .await
        .map(drop)
}

/// The server is ready: close the failure streak, and restart the
/// orchestrator if the server failed since it started.
async fn recovered(ctx: &DaemonContext, now: Timestamp) -> Result<(), ArchonError> {
    let (blocked, pending, failed) = {
        let mut state = ctx.supervisor.mcp.lock().await;
        let blocked = state.ready(now);
        assert_eq!(
            state.failed_refreshes, 0,
            "a ready server ends the failed refreshes"
        );
        (blocked, state.relink_pending, state.failed_relinks)
    };
    if blocked {
        feed::system(ctx, "The archon MCP server is ready again.").await?;
    }
    if pending {
        relink(ctx, failed, now).await?;
    }
    Ok(())
}

/// Restart the orchestrator so it gets the tools back, unless the last of
/// the `failed` restarts is too recent. A failed restart stays owed.
async fn relink(ctx: &DaemonContext, failed: Backoff, now: Timestamp) -> Result<(), ArchonError> {
    if let Some(due) = failed.deferred_until(now) {
        tracing::debug!(%due, "the orchestrator lacks Archon's tools; its next restart waits");
        return Ok(());
    }
    tracing::info!(
        failed = failed.revivals,
        "restarting the orchestrator to give it Archon's tools back"
    );
    match super::relink_orchestrator(ctx).await {
        Ok(relinked) => relinked_ok(ctx, relinked.as_ref(), failed.revivals).await,
        Err(error) if error.is_store() => Err(error),
        // Losing Aether mid-restart says nothing about the restart.
        Err(lost) if !ctx.aether.is_connected() => Err(lost),
        Err(error) => relink_failed(ctx, &error, now).await,
    }
}

/// The restart worked (or there was no orchestrator to restart) after
/// `failures` failed ones.
async fn relinked_ok(
    ctx: &DaemonContext,
    relinked: Option<&AetherAgentId>,
    failures: u32,
) -> Result<(), ArchonError> {
    {
        let mut state = ctx.supervisor.mcp.lock().await;
        state.replaced();
        assert!(
            state.server_down || !state.relink_pending,
            "nothing is owed after a restart on a ready server"
        );
        assert_eq!(state.failed_relinks, Backoff::default(), "the streak ended");
    }
    let text = match relinked {
        Some(id) => {
            tracing::info!(%id, "restarted the orchestrator to get Archon's tools back");
            "The orchestrator has Archon's tools again."
        }
        None => {
            tracing::info!("the archon MCP server is back; no orchestrator to restart");
            "The orchestrator Archon kept failing to restart is gone; the next one starts with Archon's tools."
        }
    };
    if failures >= HEALS_BEFORE_BLOCKER {
        feed::system(ctx, text).await?;
    }
    Ok(())
}

async fn relink_failed(
    ctx: &DaemonContext,
    error: &ArchonError,
    now: Timestamp,
) -> Result<(), ArchonError> {
    assert!(!error.is_store(), "a store failure is not a failed restart");
    let counted = {
        let mut state = ctx.supervisor.mcp.lock().await;
        // Another path may have replaced the orchestrator meanwhile.
        if state.relink_pending {
            state.failed_relinks = state.failed_relinks.revived(now);
            state.assert_consistent();
            let failed = state.failed_relinks;
            Some((failed.revivals, failed.deferred_until(now)))
        } else {
            None
        }
    };
    let Some((failures, due)) = counted else {
        tracing::info!(%error, "restarting the orchestrator failed, but it was replaced meanwhile");
        return Ok(());
    };
    tracing::warn!(%error, failures, "restarting the orchestrator for Archon's tools failed");
    let text = format!(
        "The archon MCP server is ready, but restarting the orchestrator so it gets Archon's tools back failed ({error}).{}",
        retry_note(due)
    );
    feed::system(ctx, text).await?;
    if failures == HEALS_BEFORE_BLOCKER {
        let problem = format!(
            "The archon MCP server is ready again, but Archon could not restart the orchestrator to give it Archon's tools back: {failures} restarts failed in a row (last: {error}). Until then the orchestrator has none of Archon's tools."
        );
        keeps_failing(ctx, failures, &problem).await?;
    }
    Ok(())
}

/// Aether's view of the server now, asked live, and the latest heal.
pub async fn mcp_status(ctx: &DaemonContext) -> McpStatus {
    let health = match ctx.aether.client() {
        Ok(client) => client.mcp_health(SERVER_NAME).await,
        Err(error) => Err(error),
    };
    let health = health.unwrap_or_else(|error| McpHealth::Unreachable {
        error: error.to_string(),
    });
    heal_status(ctx, health).await
}

/// `DaemonStatus::mcp`: `health` as last read, and the latest heal.
pub async fn heal_status(ctx: &DaemonContext, health: McpHealth) -> McpStatus {
    let state = ctx.supervisor.mcp.lock().await;
    state.assert_consistent();
    McpStatus {
        health,
        last_heal: state.last_heal,
        failed_refreshes: state.failed_refreshes,
        failed_relinks: state.failed_relinks.revivals,
    }
}

/// When Archon tries again, for a feed note.
fn retry_note(due: Option<Timestamp>) -> String {
    due.map_or_else(String::new, |at| {
        let at = at.with_timezone(&chrono::Local).format("%H:%M:%S");
        format!(" Archon tries again at {at}.")
    })
}

fn describe(health: &McpHealth) -> String {
    match health {
        McpHealth::Ready { tools } => format!("ready with {tools} tools"),
        McpHealth::Failed { error } => error.clone(),
        McpHealth::Unknown => "Aether has not contacted it".to_owned(),
        McpHealth::NotLoaded => "Aether has not loaded it".to_owned(),
        McpHealth::Unreachable { error } => error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn a_ready_server_closes_a_streak_only_once_it_stayed_quiet() {
        let mut state = HealState {
            pacing: Backoff::default().revived(at(0)),
            failed_refreshes: HEALS_BEFORE_BLOCKER,
            ..HealState::default()
        };
        assert!(state.ready(at(60)), "the standing blocker is reported");
        assert_eq!(state.failed_refreshes, 0);
        assert_eq!(state.pacing.revivals, 1, "a minute is not quiet enough");
        assert!(!state.ready(at(600)), "reported once");
        assert_eq!(state.pacing, Backoff::default());
    }

    #[test]
    fn a_new_orchestrator_owes_no_restart_and_ends_the_failed_ones() {
        let failed =
            (0..HEALS_BEFORE_BLOCKER).fold(Backoff::default(), |b, n| b.revived(at(n.into())));
        let mut state = HealState {
            failed_relinks: failed,
            relink_pending: true,
            ..HealState::default()
        };
        assert!(state.linked(), "the standing blocker is reported");
        assert!(!state.relink_pending);
        assert_eq!(state.failed_relinks, Backoff::default());
        assert!(!state.linked(), "reported once");
    }

    #[test]
    fn a_new_orchestrator_on_a_down_server_still_owes_a_restart() {
        let mut state = HealState::default();
        state.down();
        state.failed_relinks = Backoff::default().revived(at(0));
        assert!(!state.replaced(), "no blocker was standing");
        assert!(state.relink_pending, "the new agent lacks the tools too");
        assert_eq!(state.failed_relinks, Backoff::default());
        assert!(!state.ready(at(60)));
        assert!(!state.server_down);
        assert!(!state.replaced());
        assert!(!state.relink_pending, "a ready server owes nothing");
    }

    #[test]
    #[should_panic(expected = "a failed restart stays owed")]
    fn a_failed_restart_that_is_not_owed_is_caught() {
        let state = HealState {
            failed_relinks: Backoff::default().revived(at(0)),
            ..HealState::default()
        };
        state.assert_consistent();
    }
}
