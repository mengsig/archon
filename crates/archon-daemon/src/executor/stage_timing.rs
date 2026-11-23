use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::spawn::{self, StoredSpawn};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};

const TIMEOUT_FLOOR_MINUTES: u32 = 30;

/// Frozen with a spawn, including a receipt once its news is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StageTiming {
    pub p90_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    overrun_reported: bool,
}

impl StageTiming {
    pub fn new(p90_seconds: Option<u64>) -> Self {
        Self {
            p90_seconds,
            overrun_reported: false,
        }
    }
}

/// The learned limit, if history would shorten this configured limit.
pub(super) fn learned_minutes(configured: u32, p90_seconds: u64) -> Option<u32> {
    assert!(configured > 0, "every stage has a configured timeout");
    assert!(p90_seconds > 0, "a learned duration is positive");
    let minutes = p90_seconds
        .saturating_mul(3)
        .div_ceil(60)
        .max(u64::from(TIMEOUT_FLOOR_MINUTES));
    (minutes < u64::from(configured)).then(|| u32::try_from(minutes).expect("below configured"))
}

pub(super) fn report_overrun(
    ctx: &DaemonContext,
    store: &mut Store,
    stage: &StageRun,
    mut stored: StoredSpawn,
    now: Timestamp,
) -> Result<(), ArchonError> {
    assert_eq!(stage.state, StageState::Running);
    assert_eq!(stored.request.idempotency_key, stage.id.as_str());
    let needs_sample = stored.timing.is_none();
    if needs_sample {
        let run = store.run(&stage.run)?;
        let p90 = store.stage_p90_seconds(&run.pipeline, &stage.stage_id, stage.choice.effort)?;
        stored.timing = Some(StageTiming::new(p90));
    }
    if let Some(event) = overrun(store, stage, &mut stored, now)? {
        let key = spawn::request_key(&stage.id);
        let raw = serde_json::to_string(&stored)?;
        triggers::hold_stage_overrun(store, event, (&key, &raw))?;
        triggers::wake(ctx);
    } else if needs_sample {
        spawn::save_spawn(store, &stage.id, &stored)?;
    }
    Ok(())
}

fn overrun(
    store: &Store,
    stage: &StageRun,
    stored: &mut StoredSpawn,
    now: Timestamp,
) -> Result<Option<TriggerEvent>, ArchonError> {
    let timing = stored
        .timing
        .as_mut()
        .expect("the stage's sample was resolved");
    if timing.overrun_reported {
        return Ok(None);
    }
    let Some(p90_seconds) = timing.p90_seconds else {
        return Ok(None);
    };
    assert!(p90_seconds > 0, "a learned duration is positive");
    let started = stage
        .started_at
        .max(stored.not_before.unwrap_or(stage.started_at));
    let Ok(elapsed) = (now - started).to_std() else {
        return Ok(None);
    };
    let elapsed_seconds = elapsed.as_secs() + u64::from(elapsed.subsec_nanos() > 0);
    if elapsed_seconds <= p90_seconds.saturating_mul(2) {
        return Ok(None);
    }
    let run = store.run(&stage.run)?;
    assert!(
        !run.state.is_terminal(),
        "the sweep selects stages in live runs"
    );
    timing.overrun_reported = true;
    Ok(Some(TriggerEvent::StageOverrun {
        item: run.item,
        stage: stage.stage_id.clone(),
        stage_run: stage.id.clone(),
        elapsed_seconds,
        p90_seconds,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learned_timeouts_respect_the_floor_and_configured_cap() {
        assert_eq!(learned_minutes(120, 300), Some(30));
        assert_eq!(learned_minutes(120, 721), Some(37), "round up");
        assert_eq!(learned_minutes(30, 300), None, "equal stays configured");
        assert_eq!(
            learned_minutes(20, 300),
            None,
            "the cap wins over the floor"
        );
        assert_eq!(
            learned_minutes(120, 3_000),
            None,
            "history cannot extend a timeout"
        );
    }
}
