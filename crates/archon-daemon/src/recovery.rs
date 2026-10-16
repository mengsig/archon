//! Start-up recovery (ARCHITECTURE §16) as the socket sees it: archond
//! serves reads, `status` with the progress among them, from right after it
//! takes its locks, and refuses every change, after a short grace, until
//! recovery is done.

use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::protocol::{RecoveryProgress, Request};
use tokio::sync::watch;

use crate::context::DaemonContext;

/// How long a change waits for recovery to end before it is refused: a
/// client that has just started archond is not refused for a short one.
pub const RECOVERY_GRACE: Duration = Duration::from_secs(2);

/// Where start-up recovery is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Done, or never run in this context: changes are served.
    Recovered,
    Running(RecoveryProgress),
    /// It failed: archond stops, or serves in standby, but never recovered.
    Failed,
}

/// Start-up recovery, which every change and dispatch pass waits on.
#[derive(Debug)]
pub struct Recovery {
    phase: watch::Sender<Phase>,
}

impl Default for Recovery {
    fn default() -> Self {
        Self {
            phase: watch::Sender::new(Phase::Recovered),
        }
    }
}

const STARTED: RecoveryProgress = RecoveryProgress {
    runs_done: 0,
    runs_total: None,
};

impl Recovery {
    /// Recovery starts: changes are refused and dispatch held until
    /// [`Recovery::finish`].
    pub fn begin(&self) {
        let earlier = self.phase.send_replace(Phase::Running(STARTED));
        assert_eq!(earlier, Phase::Recovered, "recovery begins once");
    }

    /// `total` live runs are to be recovered; begins recovery if nobody has.
    pub fn count(&self, total: usize) {
        let total = u32::try_from(total).expect("live runs fit in a u32");
        self.phase.send_modify(|phase| {
            let mut counted = match *phase {
                Phase::Recovered => STARTED,
                Phase::Running(progress) => progress,
                Phase::Failed => panic!("a failed recovery counts no runs"),
            };
            assert_eq!(counted.runs_total, None, "runs are counted once");
            assert_eq!(counted.runs_done, 0, "no run is done before the count");
            counted.runs_total = Some(total);
            *phase = Phase::Running(counted);
        });
    }

    /// One more counted run is recovered.
    pub fn advance(&self) {
        self.phase.send_modify(|phase| {
            let Phase::Running(counted) = phase else {
                panic!("runs recover while recovery runs, not {phase:?}");
            };
            let total = counted.runs_total.expect("runs are counted first");
            assert!(counted.runs_done < total, "no run is recovered twice");
            counted.runs_done += 1;
        });
    }

    /// Recovery is done: changes and dispatch go ahead.
    pub fn finish(&self) {
        let earlier = self.phase.send_replace(Phase::Recovered);
        assert_ne!(earlier, Phase::Failed, "a failed recovery stays failed");
    }

    /// Recovery failed. On a full disk, enter standby first: the changes
    /// that waited for recovery are released into standby's refusal.
    pub fn fail(&self) {
        let earlier = self.phase.send_replace(Phase::Failed);
        assert_ne!(earlier, Phase::Recovered, "only a recovery under way fails");
    }

    pub fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    pub fn is_recovered(&self) -> bool {
        self.phase() == Phase::Recovered
    }

    /// Recovery under way, `None` once it ended.
    pub fn progress(&self) -> Option<RecoveryProgress> {
        match self.phase() {
            Phase::Running(progress) => Some(progress),
            Phase::Recovered | Phase::Failed => None,
        }
    }

    /// The phase after waiting up to `limit` for recovery to end.
    async fn phase_after(&self, limit: Duration) -> Phase {
        let mut changes = self.phase.subscribe();
        let ended = changes.wait_for(|phase| !matches!(phase, Phase::Running(_)));
        match tokio::time::timeout(limit, ended).await {
            Ok(Ok(phase)) => *phase,
            // The sender lives in the context this borrows, so it cannot close.
            Ok(Err(_)) => unreachable!("the recovery sender outlives its readers"),
            Err(_) => self.phase(),
        }
    }
}

/// The refusal of `request` until recovery is done, which serves reads only.
/// A change waits up to [`RECOVERY_GRACE`] for recovery to end first; check
/// standby after this, as a failed recovery may have entered it meanwhile.
pub(crate) async fn refusal(ctx: &DaemonContext, request: &Request) -> Option<ArchonError> {
    if request.is_read_only() || ctx.recovery.is_recovered() {
        return None;
    }
    let command = crate::handlers::command_name(request);
    match ctx.recovery.phase_after(RECOVERY_GRACE).await {
        Phase::Recovered => None,
        Phase::Running(progress) => Some(ArchonError::Recovering(format!(
            "archond is {} and refuses {command} until it is done; retry in a few seconds \
             (`archon status` shows the progress)",
            progress.describe()
        ))),
        // A full disk's standby refuses it next, with its own reason.
        Phase::Failed if ctx.standby.get().is_some() => None,
        Phase::Failed => Some(ArchonError::Unavailable(format!(
            "archond failed to recover its runs and refuses {command}; it is stopping, \
             and the next command restarts it"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_runs_from_counting_to_done() {
        let recovery = Recovery::default();
        assert_eq!(recovery.progress(), None, "a context starts recovered");
        recovery.begin();
        let counting = recovery.progress().expect("recovering");
        assert_eq!(counting.describe(), "recovering, counting runs");

        recovery.count(3);
        recovery.advance();
        let partway = recovery.progress().expect("still recovering");
        assert_eq!(partway.describe(), "recovering 1/3 runs");

        recovery.finish();
        assert_eq!(recovery.progress(), None, "done");
    }

    #[test]
    fn counting_without_a_begin_still_tracks_recovery() {
        let recovery = Recovery::default();
        recovery.count(1);
        recovery.advance();
        let done = recovery.progress().expect("recovering until finished");
        assert_eq!((done.runs_done, done.runs_total), (1, Some(1)));
    }

    #[tokio::test]
    async fn a_wait_ends_with_recovery_or_at_its_limit() {
        let recovery = std::sync::Arc::new(Recovery::default());
        recovery.begin();
        let short = Duration::from_millis(20);
        let still = recovery.phase_after(short).await;
        assert_eq!(still, Phase::Running(STARTED), "still running");

        let failing = recovery.clone();
        let failer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            failing.fail();
        });
        let ended = recovery.phase_after(Duration::from_secs(5)).await;
        assert_eq!(ended, Phase::Failed, "a failure ends the wait as one");
        failer.await.expect("failer");
        assert!(
            !recovery.is_recovered(),
            "a failed recovery never recovered"
        );
    }
}
