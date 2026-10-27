//! Supervising Archon's own aetherd (dedicated mode): when nothing listens
//! on its socket, archond starts it again instead of waiting for a command.

use std::sync::Arc;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use tokio::time::Instant;

use crate::context::DaemonContext;
use crate::feed;

/// How long nothing listens before archond starts aetherd: a command that
/// restarts it itself (`archon setup`) is back within this.
pub const START_GRACE: Duration = Duration::from_secs(2);
/// The wait after a failed start, doubled up to [`RETRY_MAX`].
pub const RETRY_MIN: Duration = Duration::from_secs(10);
pub const RETRY_MAX: Duration = Duration::from_secs(300);

/// What starts Archon's own aetherd; the `archon` binary does it through the
/// `aether` CLI, under the lock every archon command takes for it.
pub trait AetherStarter: std::fmt::Debug + Send + Sync + 'static {
    /// Start aetherd unless it answers, and return once it listens. Blocking.
    fn ensure_running(&self) -> Result<(), ArchonError>;
}

/// One archond's supervision of its aetherd, driven by the link loop.
pub(super) struct OwnAetherd {
    /// `None` in shared mode: the operator's aetherd is theirs to start.
    starter: Option<Arc<dyn AetherStarter>>,
    absent_since: Option<Instant>,
    next_start: Option<Instant>,
    retry: Duration,
    /// Why the last start failed; the operator was told once per outage.
    failure: Option<String>,
    /// The restart archond planned that stopped aetherd, read as the outage
    /// begins: the next archond's report removes its record within a minute.
    planned: Option<String>,
}

impl OwnAetherd {
    pub(super) fn new(starter: Option<Arc<dyn AetherStarter>>) -> Self {
        Self {
            starter,
            absent_since: None,
            next_start: None,
            retry: RETRY_MIN,
            failure: None,
            planned: None,
        }
    }

    /// archond connected: the next absence is a new outage.
    pub(super) fn answered(&mut self) {
        *self = Self::new(self.starter.take());
    }

    /// Something listens, though it did not connect: it is not missing, and
    /// `status` says why archond cannot connect instead of an absence.
    pub(super) fn listening(
        &mut self,
        ctx: &DaemonContext,
        error: &ArchonError,
    ) -> Result<(), ArchonError> {
        self.absent_since = None;
        if self.starter.is_none() {
            return Ok(());
        }
        let why = format!("Archon's own aetherd answers, but archond cannot connect: {error}");
        ctx.aether.mark_down(why)
    }

    /// Nothing listens: once the grace is over, start aetherd, waiting
    /// longer after each failure. Only a store failure is an `Err`.
    pub(super) async fn absent(&mut self, ctx: &DaemonContext) -> Result<(), ArchonError> {
        let Some(starter) = self.starter.clone() else {
            return Ok(());
        };
        let now = Instant::now();
        if self.absent_since.is_none() {
            self.planned = crate::ops::planned_aether_restart(ctx).await?;
        }
        let since = *self.absent_since.get_or_insert(now);
        ctx.aether.mark_down(self.down_text())?;
        if now - since < START_GRACE || self.next_start.is_some_and(|at| now < at) {
            return Ok(());
        }
        tracing::warn!("Archon's own aetherd is not running; starting it");
        let started = tokio::task::spawn_blocking(move || starter.ensure_running())
            .await
            .unwrap_or_else(|panic| {
                Err(ArchonError::Unavailable(format!(
                    "the start panicked: {panic}"
                )))
            });
        match started {
            Ok(()) => self.started(ctx).await,
            Err(error) => self.failed(ctx, &error).await,
        }
    }

    /// What `status` says of the link meanwhile, with what brings it back.
    fn down_text(&self) -> String {
        match &self.failure {
            Some(failure) => failure.clone(),
            None => "Archon's own aetherd is not running; archond starts it again within \
                     seconds"
                .to_owned(),
        }
    }

    async fn started(&mut self, ctx: &DaemonContext) -> Result<(), ArchonError> {
        assert!(self.starter.is_some(), "only a supervisor that starts it");
        let told = self.failure.is_some();
        let planned = self.planned.take();
        self.absent_since = None;
        self.next_start = None;
        self.retry = RETRY_MIN;
        self.failure = None;
        tracing::info!("started Archon's own aetherd again");
        // A restart archond asked for stops aetherd on purpose: no crash to report.
        let mut text = match planned {
            Some(what) => format!("Archon's own aetherd restarted, as planned ({what})"),
            None => "Archon's own aetherd had stopped; archond started it again".to_owned(),
        };
        text.push_str(match told {
            true => " after the failures it reported.",
            false => ".",
        });
        feed::system(ctx, text).await.map(drop)
    }

    async fn failed(
        &mut self,
        ctx: &DaemonContext,
        error: &ArchonError,
    ) -> Result<(), ArchonError> {
        assert!(
            (RETRY_MIN..=RETRY_MAX).contains(&self.retry),
            "the wait stays in its bounds"
        );
        tracing::warn!(%error, retry = ?self.retry, "could not start Archon's own aetherd");
        let text = format!(
            "Archon's own aetherd is not running, and archond could not start it: {error}. \
             archond tries again every few minutes; `archon daemon start` tries at once and \
             says what fails."
        );
        let first = self.failure.is_none();
        self.failure = Some(text.clone());
        self.next_start = Some(Instant::now() + self.retry);
        self.retry = (self.retry * 2).min(RETRY_MAX);
        ctx.aether.mark_down(text.clone())?;
        if first {
            feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
        }
        Ok(())
    }
}
