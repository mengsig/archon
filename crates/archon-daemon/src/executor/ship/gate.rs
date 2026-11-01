//! The ship gate (issue #82, task AR-I2): before archond delivers a run it
//! runs the repository's trusted fast check on the committed change, for
//! the units the change affects. The check runs off the executor's path,
//! one at a time on this host; the delivery waits for its verdict, then
//! proves the worktree again before anything leaves it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::repo::affected::{self, Affected};
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio::task::AbortHandle;

use super::super::fast_check;

/// What one run's gate checks: the committed `tree` in `workspace`, as it
/// differs from `base`, with the trusted `template`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Check {
    pub tree: String,
    pub workspace: PathBuf,
    pub base: String,
    pub template: String,
    /// The `CARGO_TARGET_DIR` the item's stages build into, if any.
    pub target_dir: Option<PathBuf>,
}

/// How a gate ended for one tree; `refusal` quotes a failed check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verdict {
    pub tree: String,
    pub refusal: Option<String>,
}

/// Where a run's gate stands, for its delivery.
#[derive(Debug)]
pub(super) enum Status {
    /// No gate ran, or its failure was taken and it runs again.
    Idle,
    Running,
    Finished(Result<Verdict, ArchonError>),
}

/// What a passing check held for: the same tree, base and command pass again.
type Passed = (String, String, String);

/// Every run's gate, the one slot their checks share, and the checks that
/// passed, so a later run of the same tree reuses the pass.
#[derive(Debug)]
pub struct Gates {
    by_run: BTreeMap<PipelineRunId, Gate>,
    slot: Arc<Semaphore>,
    passed: BTreeSet<Passed>,
}

impl Default for Gates {
    fn default() -> Self {
        Self {
            by_run: BTreeMap::new(),
            slot: Arc::new(Semaphore::new(1)),
            passed: BTreeSet::new(),
        }
    }
}

fn passed_key(check: &Check) -> Passed {
    (
        check.tree.clone(),
        check.base.clone(),
        check.template.clone(),
    )
}

#[derive(Debug)]
enum Gate {
    Running(Running),
    Done(Verdict),
}

/// A check in flight; dropping it aborts the task, which kills the check.
#[derive(Debug)]
struct Running {
    result: oneshot::Receiver<Result<Verdict, ArchonError>>,
    task: AbortHandle,
    key: Passed,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Gates {
    /// Start `check` for `run`, replacing any gate it had, and poke `wake`
    /// once its verdict can be taken.
    /// A check that already passed for the same tree, base and command is
    /// not run again: its pass is the verdict at once.
    pub(super) fn start(&mut self, run: &PipelineRunId, check: Check, wake: Arc<Notify>) {
        assert!(check.workspace.is_absolute(), "run worktrees are absolute");
        assert!(
            !check.template.trim().is_empty(),
            "a trusted check is a command"
        );
        let key = passed_key(&check);
        if self.passed.contains(&key) {
            tracing::info!(%run, tree = %check.tree, "the fast check passed on this tree before; reused");
            let verdict = Verdict {
                tree: check.tree,
                refusal: None,
            };
            self.by_run.insert(run.clone(), Gate::Done(verdict));
            wake.notify_one();
            return;
        }
        let (sender, result) = oneshot::channel();
        let slot = Arc::clone(&self.slot);
        let id = run.clone();
        let task = tokio::spawn(async move {
            let verdict = gated(&id, check, slot).await;
            // A gate forgotten meanwhile dropped the receiver: nobody waits.
            let _ = sender.send(verdict);
            wake.notify_one();
        });
        let running = Running {
            result,
            task: task.abort_handle(),
            key,
        };
        self.by_run.insert(run.clone(), Gate::Running(running));
        assert!(self.by_run.contains_key(run), "the gate is tracked");
    }

    /// Where `run`'s gate stands. A failure to check is taken once, so the
    /// next delivery runs the gate again; a verdict is kept for the run.
    pub(super) fn status(&mut self, run: &PipelineRunId) -> Status {
        let (finished, key) = match self.by_run.get_mut(run) {
            None => return Status::Idle,
            Some(Gate::Done(verdict)) => return Status::Finished(Ok(verdict.clone())),
            Some(Gate::Running(running)) => match running.result.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => return Status::Running,
                Err(oneshot::error::TryRecvError::Closed) => (
                    Err(ArchonError::Unavailable(
                        "the fast check's task ended without a verdict".into(),
                    )),
                    running.key.clone(),
                ),
                Ok(result) => (result, running.key.clone()),
            },
        };
        match finished {
            Ok(verdict) => {
                assert_eq!(verdict.tree, key.0, "a verdict is on the tree checked");
                if verdict.refusal.is_none() {
                    self.passed.insert(key);
                }
                self.by_run.insert(run.clone(), Gate::Done(verdict.clone()));
                Status::Finished(Ok(verdict))
            }
            Err(error) => {
                self.by_run.remove(run);
                Status::Finished(Err(error))
            }
        }
    }

    /// Drop `run`'s gate, killing a check still running.
    pub(crate) fn forget(&mut self, run: &PipelineRunId) {
        self.by_run.remove(run);
        debug_assert!(!self.by_run.contains_key(run));
    }
}

/// Run `check` once the host's slot is free.
async fn gated(
    run: &PipelineRunId,
    check: Check,
    slot: Arc<Semaphore>,
) -> Result<Verdict, ArchonError> {
    let _permit = slot
        .acquire_owned()
        .await
        .map_err(|_| ArchonError::Unavailable("the fast check slot closed".into()))?;
    let affected = selected(&check).await?;
    let command = match affected::expand(&check.template, &affected.units) {
        Ok(Some(command)) => command,
        Ok(None) => {
            tracing::info!(%run, "the change affects no unit; the fast check has nothing to run");
            return Ok(Verdict {
                tree: check.tree,
                refusal: None,
            });
        }
        Err(why) => {
            let refusal = format!(
                "the fast check `{}` cannot run for the committed change: {why}; rename it, \
                 commit, and retry the item.",
                check.template
            );
            return Ok(Verdict {
                tree: check.tree,
                refusal: Some(refusal),
            });
        }
    };
    let envs: Vec<(&str, &std::path::Path)> = check
        .target_dir
        .as_deref()
        .map(|dir| ("CARGO_TARGET_DIR", dir))
        .into_iter()
        .collect();
    tracing::info!(%run, %command, units = ?affected.units, "running the fast check");
    let ran = fast_check::run(&command, &check.workspace, &envs, fast_check::RUN_LIMIT).await?;
    let refusal = match &ran.ended {
        fast_check::Ended::Passed => None,
        fast_check::Ended::Failed(why) => Some(format!(
            "the fast check `{command}` failed on the committed change ({why}); fix what it \
             reports, commit, and retry the item.\n{}",
            ran.shown()
        )),
    };
    tracing::info!(%run, %command, passed = refusal.is_none(), "fast check ended");
    Ok(Verdict {
        tree: check.tree,
        refusal,
    })
}

/// What the committed change affects, read only when the check selects.
async fn selected(check: &Check) -> Result<Affected, ArchonError> {
    if !affected::selects(&check.template) {
        return Ok(Affected::default());
    }
    let (workspace, base) = (check.workspace.clone(), check.base.clone());
    let read = move || fast_check::affected_by(&workspace, &base);
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|e| ArchonError::io("fast check selection task", std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests;
