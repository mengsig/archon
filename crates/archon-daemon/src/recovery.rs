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

/// How far start-up recovery is; nothing once it is done, or in a context
/// that never recovers.
#[derive(Debug)]
pub struct Recovery {
    progress: watch::Sender<Option<RecoveryProgress>>,
}

impl Default for Recovery {
    fn default() -> Self {
        Self {
            progress: watch::Sender::new(None),
        }
    }
}

impl Recovery {
    /// Recovery starts: changes are refused and dispatch held until
    /// [`Recovery::finish`].
    pub fn begin(&self) {
        let started = RecoveryProgress {
            runs_done: 0,
            runs_total: None,
        };
        let earlier = self.progress.send_replace(Some(started));
        assert!(earlier.is_none(), "recovery begins once");
    }

    /// `total` live runs are to be recovered; begins recovery if nobody has.
    pub fn count(&self, total: usize) {
        let total = u32::try_from(total).expect("live runs fit in a u32");
        self.progress.send_modify(|progress| {
            let counted = progress.get_or_insert(RecoveryProgress {
                runs_done: 0,
                runs_total: None,
            });
            assert_eq!(counted.runs_total, None, "runs are counted once");
            assert_eq!(counted.runs_done, 0, "no run is done before the count");
            counted.runs_total = Some(total);
        });
    }

    /// One more counted run is recovered.
    pub fn advance(&self) {
        self.progress.send_modify(|progress| {
            let counted = progress.as_mut().expect("runs recover while recovery runs");
            let total = counted.runs_total.expect("runs are counted first");
            assert!(counted.runs_done < total, "no run is recovered twice");
            counted.runs_done += 1;
        });
    }

    /// Recovery is done, or failed into standby or an exit.
    pub fn finish(&self) {
        self.progress.send_replace(None);
    }

    /// Recovery under way, `None` once done.
    pub fn progress(&self) -> Option<RecoveryProgress> {
        *self.progress.borrow()
    }

    /// Recovery under way after waiting up to `limit` for it to end.
    async fn progress_after(&self, limit: Duration) -> Option<RecoveryProgress> {
        let mut changes = self.progress.subscribe();
        let ended = tokio::time::timeout(limit, changes.wait_for(Option::is_none)).await;
        match ended {
            Ok(Ok(_)) => None,
            // The sender lives in the context this borrows, so it cannot close.
            Ok(Err(_)) => unreachable!("the recovery sender outlives its readers"),
            Err(_) => self.progress(),
        }
    }
}

/// The refusal of `request` while recovery runs, which serves reads only. A
/// change waits up to [`RECOVERY_GRACE`] for recovery to end first.
pub(crate) async fn refusal(ctx: &DaemonContext, request: &Request) -> Option<ArchonError> {
    if request.is_read_only() {
        return None;
    }
    ctx.recovery.progress()?;
    let progress = ctx.recovery.progress_after(RECOVERY_GRACE).await?;
    let command = crate::handlers::command_name(request);
    Some(ArchonError::Recovering(format!(
        "archond is {} and refuses {command} until it is done; retry in a few seconds \
         (`archon status` shows the progress)",
        progress.describe()
    )))
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
        assert!(
            recovery.progress_after(short).await.is_some(),
            "still running"
        );

        let finishing = recovery.clone();
        let finisher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            finishing.finish();
        });
        let left = recovery.progress_after(Duration::from_secs(5)).await;
        assert_eq!(left, None, "the wait ends when recovery does");
        finisher.await.expect("finisher");
    }
}
