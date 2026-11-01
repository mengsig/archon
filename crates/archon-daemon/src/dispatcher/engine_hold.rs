//! The engine-login hold's probes (ARCHITECTURE §7.3): each held engine is
//! probed when due; one found logged in is lifted, with one line saying so,
//! and the agents that waited on it are woken by the sweep that follows.

use std::time::{Duration, Instant};

use archon_core::error::ArchonError;

use crate::context::DaemonContext;
use crate::executor::engine_wait;
use crate::feed;

/// The longest a probe of one engine's login takes before it counts as failed.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Probe every held engine whose probe is due. A probe that fails, or that
/// Aether does not answer, leaves the engine held, probed later.
pub(crate) async fn check(ctx: &DaemonContext) -> Result<(), ArchonError> {
    for engine in ctx.engines.due(Instant::now()) {
        assert!(!engine.is_empty(), "a held engine has a name");
        match login_problem(ctx, &engine).await {
            Some(problem) => {
                tracing::info!(%engine, %problem, "engine login still fails; probed again later");
                ctx.engines.still_out(&engine, Instant::now());
            }
            None => lift(ctx, &engine).await?,
        }
    }
    Ok(())
}

/// Why `engine` cannot log in now; `None` once it can.
async fn login_problem(ctx: &DaemonContext, engine: &str) -> Option<String> {
    match tokio::time::timeout(PROBE_TIMEOUT, ctx.backend.engine_login(engine)).await {
        Ok(Ok(problem)) => problem,
        Ok(Err(error)) => Some(format!("the probe failed: {error}")),
        Err(_) => Some(format!("the probe took over {}s", PROBE_TIMEOUT.as_secs())),
    }
}

/// Lift `engine`'s hold: its waiting spawns first get back the time they
/// waited, then the hold ends and one line tells the operator.
async fn lift(ctx: &DaemonContext, engine: &str) -> Result<(), ArchonError> {
    let Some(hold) = ctx.engines.holds().into_iter().find(|h| h.engine == engine) else {
        return Ok(());
    };
    let waiting = engine_wait::release(ctx, &hold).await?;
    let lifted = ctx.engines.lift(engine, Instant::now());
    assert!(lifted.is_some(), "only the dispatcher lifts a hold");
    ctx.engines.save(&mut *ctx.store.lock().await)?;
    assert!(!ctx.engines.is_held(engine), "the engine is free again");
    let text = format!(
        "Engine `{engine}` logs in again; {waiting} stage(s) that waited on it resume, woken \
         agents with their context."
    );
    feed::system(ctx, text).await?;
    ctx.dispatch.notify_one();
    Ok(())
}
