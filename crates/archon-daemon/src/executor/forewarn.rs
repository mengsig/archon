//! The deadline warning: once a running stage has used about 80% of its time,
//! the sweep tells its agent, once per stage run, to commit what works and
//! report. A run woken at its deadline was told so already and is not warned.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::run::{StageRun, StageState};

use super::{resume, spawn};
use crate::context::DaemonContext;

/// The share of a stage's time left when its agent is warned, in percent.
const WARN_LEFT_PERCENT: u32 = 20;

/// When `deadline`, of a stage given `timeout_minutes`, is near enough to warn.
fn warn_at(deadline: Timestamp, timeout_minutes: u32) -> Timestamp {
    assert!(timeout_minutes >= 1, "a stage has at least a minute");
    let left = chrono::Duration::seconds(i64::from(timeout_minutes * 60 * WARN_LEFT_PERCENT / 100));
    assert!(
        left > chrono::Duration::zero(),
        "a warning comes before the deadline"
    );
    deadline - left
}

/// What the agent is told; the same words each time, so a repeated send
/// under the same key is one message.
fn warning(deadline: Timestamp, timeout_minutes: u32) -> String {
    let left = (timeout_minutes * WARN_LEFT_PERCENT).div_ceil(100);
    let text = format!(
        "Archon: your stage deadline is {} (about {left} min from now), when Archon stops you \
         and every agent you delegated to. Start nothing new: commit what works, run no check \
         you cannot wait for in the foreground, and report as your instructions ask before then.",
        deadline.format("%Y-%m-%d %H:%M UTC"),
    );
    assert!(
        text.contains("commit what works"),
        "the warning says what to do"
    );
    text
}

/// Warn each of the `running` stages whose deadline nears at `now` and that
/// was not warned yet. One Aether did not answer for is warned next sweep.
pub(super) async fn sweep(
    ctx: &DaemonContext,
    running: &[StageRun],
    now: Timestamp,
) -> Result<(), ArchonError> {
    for stage in running {
        assert_eq!(
            stage.state,
            StageState::Running,
            "only a running stage is warned"
        );
        let Some(agent) = &stage.aether_agent else {
            continue;
        };
        let Some(text) = owed(&*ctx.store.lock().await, stage, now)? else {
            continue;
        };
        let key = format!("{}:deadline-warning", stage.id);
        match ctx.backend.send_message(agent, &text, &key).await {
            Ok(()) => {}
            Err(error) if resume::unanswered(&error) => {
                tracing::info!(stage = %stage.id, %error, "deadline warning unanswered; sent again next sweep");
                continue;
            }
            Err(error) if error.is_store() => return Err(error),
            // Most likely the agent ended; it is not asked again.
            Err(refusal) => {
                tracing::warn!(stage = %stage.id, %agent, %refusal, "deadline warning refused");
            }
        }
        mark(&mut *ctx.store.lock().await, stage)?;
    }
    Ok(())
}

/// The warning `stage` is owed at `now`; `None` before its time, once it
/// was warned, for a deadline wake and for a stage without a deadline.
fn owed(
    store: &archon_queue::Store,
    stage: &StageRun,
    now: Timestamp,
) -> Result<Option<String>, ArchonError> {
    let stored = match spawn::load_spawn(store, &stage.id) {
        Ok(stored) => stored,
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(deadline) = stored.request.deadline else {
        return Ok(None);
    };
    if stored.deadline_warned || stored.deadline_wake || stored.timeout_minutes == 0 {
        return Ok(None);
    }
    if now < warn_at(deadline, stored.timeout_minutes) {
        return Ok(None);
    }
    Ok(Some(warning(deadline, stored.timeout_minutes)))
}

/// Record that `stage`'s agent was warned, so no later sweep warns it again.
fn mark(store: &mut archon_queue::Store, stage: &StageRun) -> Result<(), ArchonError> {
    let mut stored = spawn::load_spawn(store, &stage.id)?;
    assert!(!stored.deadline_wake, "a deadline wake is not warned");
    stored.deadline_warned = true;
    spawn::save_spawn(store, &stage.id, &stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warning_comes_with_a_fifth_of_the_stage_time_left() {
        let deadline = archon_core::now();
        assert_eq!(
            deadline - warn_at(deadline, 60),
            chrono::Duration::minutes(12)
        );
        assert_eq!(
            deadline - warn_at(deadline, 150),
            chrono::Duration::minutes(30)
        );
        assert_eq!(
            deadline - warn_at(deadline, 1),
            chrono::Duration::seconds(12)
        );
    }

    #[test]
    fn the_warning_names_the_deadline_and_the_minutes_left() {
        let deadline = "2026-11-01T14:30:00Z".parse::<Timestamp>().unwrap();
        let text = warning(deadline, 150);
        assert!(text.contains("2026-11-01 14:30 UTC"), "{text}");
        assert!(text.contains("about 30 min"), "{text}");
        assert_eq!(text, warning(deadline, 150), "the same words each time");
    }
}
