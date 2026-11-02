//! Spawns that wait on an engine held for a failed login (ARCHITECTURE
//! §7.3): held back while it is, and released with the time they waited.

use std::collections::BTreeSet;
use std::time::Instant;

use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::notify::NotificationKind;
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;

use super::resume;
use super::spawn::{self, ParentBinding, StoredSpawn};
use crate::context::DaemonContext;
use crate::engine_login::EngineHold;

/// Hold the engine `stage`'s agent ran on, which failed on its login with
/// `error`. The hold that starts is announced with one `Blocker`, unless it
/// relapses right after the last one lifted.
pub(super) async fn hold(
    ctx: &DaemonContext,
    stage: &StageRun,
    error: &str,
) -> Result<(), ArchonError> {
    assert!(
        !stage.state.is_terminal(),
        "a live stage run failed on its login"
    );
    assert!(!error.trim().is_empty(), "a login failure says what failed");
    let engine = stage.current_choice().model.provider.clone();
    let Some(hold) = ctx.engines.report(&engine, error, Instant::now()) else {
        return Ok(());
    };
    ctx.engines.save(&mut *ctx.store.lock().await)?;
    tracing::warn!(%engine, %error, relapse = hold.relapse, "engine login failed; holding the engine");
    if hold.relapse {
        return Ok(());
    }
    let text = blocker_text(&hold);
    super::notify(ctx, NotificationKind::Blocker, &text, None, None).await
}

/// What the operator reads when an engine is held: what failed, what waits,
/// and how to bring the engine back.
fn blocker_text(hold: &EngineHold) -> String {
    let engine = &hold.engine;
    let text = format!(
        "Engine `{engine}` cannot log in ({}). Its stages wait instead of failing, and no \
         new agent starts on it; other engines go on. Archon checks its login again, waiting \
         longer each time up to {} minutes, and wakes the waiting agents once it works. \
         Renew the login (`archon aether login {engine}`, or the engine's own CLI) if it does \
         not come back by itself.",
        hold.error,
        crate::engine_login::MAX_PROBE.as_secs() / 60,
    );
    assert!(
        text.contains(engine.as_str()),
        "the blocker names the engine"
    );
    text
}

/// The spawns of `issue` no engine hold keeps back. A spawn that may have
/// reached Aether goes on, as does one of a finished run, which settles.
pub(super) async fn ready(
    ctx: &DaemonContext,
    issue: BTreeSet<StageRunId>,
) -> Result<BTreeSet<StageRunId>, ArchonError> {
    if issue.is_empty() || !ctx.engines.any_held() {
        return Ok(issue);
    }
    let store = ctx.store.lock().await;
    let mut ready = BTreeSet::new();
    for id in &issue {
        let waits = waiting_spawn(&store, id)?
            .is_some_and(|spawn| ctx.engines.is_held(engine_of(&spawn)))
            && !store.run(&store.stage_run(id)?.run)?.state.is_terminal();
        if !waits {
            ready.insert(id.clone());
        }
    }
    let held = issue.len() - ready.len();
    if held > 0 {
        tracing::debug!(spawns = held, "spawns wait on a held engine");
    }
    debug_assert!(ready.is_subset(&issue));
    Ok(ready)
}

/// The stored spawn of `id` when it was never sent, so it may still wait.
fn waiting_spawn(store: &Store, id: &StageRunId) -> Result<Option<StoredSpawn>, ArchonError> {
    match spawn::load_spawn(store, id) {
        Ok(stored) if stored.parent == ParentBinding::Pending => Ok(Some(stored)),
        Ok(_) | Err(ArchonError::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn engine_of(spawn: &StoredSpawn) -> &str {
    let engine = spawn.request.choice.model.provider.as_str();
    assert!(!engine.is_empty(), "a model names its engine");
    engine
}

/// Give each spawn that waited on `hold`'s engine the time it waited, so its
/// stage deadline counts from now; returns how many waited. Called before
/// the hold lifts, so none is issued with the deadline it was reserved with.
pub(crate) async fn release(ctx: &DaemonContext, hold: &EngineHold) -> Result<usize, ArchonError> {
    // A clock stepped back past `held_at` moves no deadline: `waited` is then negative.
    let now = archon_core::now();
    assert!(!hold.engine.is_empty(), "a hold names its engine");
    let mut store = ctx.store.lock().await;
    let live = store.live_stage_runs()?;
    let mut released = 0;
    for stage in &live {
        if stage.state != StageState::Spawning {
            continue;
        }
        let Some(mut stored) = waiting_spawn(&store, &stage.id)? else {
            continue;
        };
        if engine_of(&stored) != hold.engine {
            continue;
        }
        released += 1;
        let waited = now - stage.started_at.max(hold.held_at);
        let Some(deadline) = stored
            .request
            .deadline
            .filter(|_| waited > chrono::Duration::zero())
        else {
            continue;
        };
        let moved = deadline + waited;
        stored.request.deadline = Some(moved);
        stored.resume = stored
            .resume
            .map(|kept| resume::with_moved_deadline(kept, moved));
        assert!(
            moved > deadline,
            "a waiting spawn's deadline only moves later"
        );
        spawn::save_spawn(&mut store, &stage.id, &stored)?;
    }
    assert!(
        released <= live.len(),
        "each waiting spawn is released once"
    );
    Ok(released)
}
