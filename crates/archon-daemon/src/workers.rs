//! The daemon's background workers, each supervised: one that fails, panics
//! or stops is restarted with a growing pause, and one that keeps failing is
//! switched off with a notice, so no single worker stops the daemon.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use archon_core::error::ArchonError;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

type Work = Pin<Box<dyn Future<Output = Result<(), ArchonError>> + Send>>;
type Factory = Box<dyn Fn() -> Work + Send + Sync>;
type TaskResult = (&'static str, Result<(), ArchonError>);

/// How a failing worker is paused and when it is given up on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    /// The pause after a worker's first failure; it doubles up to `longest`.
    pub first: Duration,
    pub longest: Duration,
    /// A worker that ran this long before failing starts its failures over.
    pub healthy_for: Duration,
    /// Failures in a row after which a worker is switched off.
    pub give_up_after: u32,
    /// Failures in a row at which the operator is told it keeps failing.
    pub tell_at: u32,
}

impl Policy {
    pub(crate) const DAEMON: Self = Self {
        first: Duration::from_secs(2),
        longest: Duration::from_secs(120),
        healthy_for: Duration::from_secs(300),
        give_up_after: 12,
        tell_at: 3,
    };

    fn pause(&self, failures: u32) -> Duration {
        assert!(failures >= 1, "a pause follows a failure");
        let doublings = (failures - 1).min(16);
        self.first.saturating_mul(1 << doublings).min(self.longest)
    }
}

/// What the daemon must do about a worker that ended.
#[derive(Debug)]
pub(crate) enum Ended {
    /// The worker cannot go on: the daemon stops, or turns to standby.
    Fatal(ArchonError),
    /// The worker will run again after `pause`; `tell` when the operator should hear.
    Restarting {
        name: &'static str,
        error: String,
        failures: u32,
        pause: Duration,
        tell: bool,
    },
    /// The worker failed too often in a row and stays off until archond restarts.
    Disabled {
        name: &'static str,
        error: String,
        failures: u32,
    },
}

struct Spec {
    factory: Factory,
    failures: u32,
    running_since: Instant,
}

pub(crate) struct Workers {
    tasks: JoinSet<TaskResult>,
    names: HashMap<tokio::task::Id, &'static str>,
    specs: HashMap<&'static str, Spec>,
    policy: Policy,
}

impl Workers {
    pub(crate) fn new(policy: Policy) -> Self {
        Self {
            tasks: JoinSet::new(),
            names: HashMap::new(),
            specs: HashMap::new(),
            policy,
        }
    }

    /// Run `make()` as the worker `name`, and again whenever it ends.
    pub(crate) fn spawn<F, Fut>(&mut self, name: &'static str, make: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), ArchonError>> + Send + 'static,
    {
        let factory: Factory = Box::new(move || Box::pin(make()));
        let spec = Spec {
            factory,
            failures: 0,
            running_since: Instant::now(),
        };
        let earlier = self.specs.insert(name, spec);
        assert!(earlier.is_none(), "worker names are unique");
        self.start(name, Duration::ZERO);
    }

    pub(crate) fn len(&self) -> usize {
        self.specs.len()
    }

    fn start(&mut self, name: &'static str, pause: Duration) {
        let spec = self
            .specs
            .get_mut(name)
            .expect("a spawned worker has a spec");
        spec.running_since = Instant::now() + pause;
        let work = (spec.factory)();
        let handle = self.tasks.spawn(async move {
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            (name, work.await)
        });
        let earlier = self.names.insert(handle.id(), name);
        assert!(earlier.is_none(), "task ids are unique");
    }

    /// The next worker to end and what to do about it; `None` once no worker is left.
    /// A switched-off worker no longer counts as running.
    pub(crate) async fn next_ended(&mut self) -> Option<Ended> {
        let done = self.tasks.join_next_with_id().await?;
        let id = match &done {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        let name = self.names.remove(&id).expect("a worker ends once");
        Some(self.judge(name, done.map(|(_, result)| result)))
    }

    fn judge(&mut self, name: &'static str, done: Result<TaskResult, JoinError>) -> Ended {
        let error = match done {
            Ok((_, Err(error @ ArchonError::DiskFull(_)))) => return Ended::Fatal(error),
            Ok((_, Err(error))) => error.to_string(),
            Ok((_, Ok(()))) => "it stopped, though it should run as long as archond does".into(),
            Err(error) => format!("it panicked or was cancelled: {error}"),
        };
        let policy = self.policy;
        let spec = self
            .specs
            .get_mut(name)
            .expect("a spawned worker has a spec");
        if spec.running_since.elapsed() >= policy.healthy_for {
            spec.failures = 0;
        }
        spec.failures += 1;
        let failures = spec.failures;
        if failures >= policy.give_up_after {
            return Ended::Disabled {
                name,
                error,
                failures,
            };
        }
        let pause = policy.pause(failures);
        self.start(name, pause);
        Ended::Restarting {
            name,
            error,
            failures,
            pause,
            tell: failures == policy.tell_at,
        }
    }

    /// Abort every worker and wait up to `limit` for them to end. One busy
    /// outside any `.await` cannot be aborted: it is named, left behind and
    /// returned.
    pub(crate) async fn stop(mut self, limit: Duration) -> Vec<&'static str> {
        self.tasks.abort_all();
        let deadline = tokio::time::Instant::now() + limit;
        while let Ok(Some(done)) =
            tokio::time::timeout_at(deadline, self.tasks.join_next_with_id()).await
        {
            let id = match &done {
                Ok((id, _)) => *id,
                Err(error) => error.id(),
            };
            self.names.remove(&id);
        }
        let left: Vec<&'static str> = self.names.into_values().collect();
        for task in &left {
            tracing::warn!(
                task,
                ?limit,
                "daemon task still running after abort; exiting without it"
            );
        }
        assert_eq!(left.len(), self.tasks.len(), "every running task is named");
        self.tasks.detach_all();
        left
    }
}

#[cfg(test)]
mod tests;
