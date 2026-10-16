//! What `status` shows of Aether, its `archon` MCP server and the GitHub
//! watcher. Aether is asked off the request path, so a slow aetherd never
//! holds status up. The answers are for display only: nothing may decide
//! or authorize from them.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use archon_aether::AetherClient;
use archon_aether::github::WatcherStatus;
use archon_aether::wire::Agent;
use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::mcp_tools::SERVER_NAME;
use archon_core::protocol::{McpHealth, ProbeHealth, StatusHealth};
use tokio::sync::watch;

/// How long a status request waits for Aether and the disk probes; well
/// under the MCP bridge's 15 s budget for a whole request.
pub const STATUS_WAIT: Duration = Duration::from_secs(1);
/// The deadline every ask of one refresh shares.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// What one refresh asks Aether about.
#[derive(Debug, Clone)]
pub struct Targets {
    /// The recorded orchestrator, looked up; Aether is pinged without one.
    pub orchestrator: Option<AetherAgentId>,
    /// Read the GitHub watcher: false in shared mode.
    pub github: bool,
}

/// One section's latest answer, kept until the next one.
#[derive(Debug, Clone)]
pub struct Probe<T> {
    /// The last answer; it stands while a newer ask runs.
    pub value: Option<T>,
    pub as_of: Option<Timestamp>,
    /// Why the latest ask failed; cleared by the next answer.
    pub last_error: Option<String>,
}

impl<T> Default for Probe<T> {
    fn default() -> Self {
        Self {
            value: None,
            as_of: None,
            last_error: None,
        }
    }
}

impl<T> Probe<T> {
    fn record(&mut self, (result, at): Stamped<T>) {
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.as_of = Some(at);
                self.last_error = None;
            }
            Err(error) => self.fail(error.to_string()),
        }
        assert!(
            self.value.is_some() == self.as_of.is_some(),
            "a value is kept with the time it was read"
        );
    }

    /// The latest ask failed; the last answer stands, now stale.
    fn fail(&mut self, error: String) {
        assert!(!error.is_empty(), "a failure says why");
        self.last_error = Some(error);
    }

    fn health(&self, refreshing: bool) -> ProbeHealth {
        ProbeHealth {
            as_of: self.as_of,
            last_error: self.last_error.clone(),
            stale: refreshing || self.last_error.is_some() || self.as_of.is_none(),
        }
    }
}

/// The latest answers, as one status shows them.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The orchestrator's record; `Some(None)` when Aether answered a
    /// ping because no orchestrator was recorded.
    pub aether: Probe<Option<Agent>>,
    pub mcp: Probe<McpHealth>,
    pub github: Probe<WatcherStatus>,
    /// A refresh was still running when status answered.
    pub refreshing: bool,
}

impl Snapshot {
    /// The freshness of each section; GitHub's only when it is read.
    pub fn health(&self, github: bool) -> StatusHealth {
        StatusHealth {
            aether: self.aether.health(self.refreshing),
            mcp: self.mcp.health(self.refreshing),
            github: github.then(|| self.github.health(self.refreshing)),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    answers: Snapshot,
    /// Set while a refresh runs; it turns `true` when the refresh ends.
    running: Option<watch::Receiver<bool>>,
}

/// The cache, refreshed by a task each status starts unless one runs.
#[derive(Debug, Default)]
pub struct StatusProbes {
    state: Arc<Mutex<State>>,
}

impl StatusProbes {
    /// Ask Aether through `link` (unless it is down) and wait until
    /// `deadline` for the answers; past that, the asks go on and the last
    /// answers are shown, marked stale.
    pub async fn read(
        &self,
        link: Result<Arc<AetherClient>, ArchonError>,
        targets: Targets,
        deadline: tokio::time::Instant,
    ) -> Snapshot {
        let mut done = match link {
            Ok(client) => self.refresh(client, targets),
            Err(error) => {
                let mut state = lock(&self.state);
                record_all(&mut state.answers, &error, &targets);
                return Snapshot {
                    refreshing: state.running.is_some(),
                    ..state.answers.clone()
                };
            }
        };
        // A refresh that panicked closed its channel: it is over all the same.
        let ended = tokio::time::timeout_at(deadline, done.wait_for(|ended| *ended)).await;
        if ended.is_err() {
            tracing::debug!("status answers before Aether does");
        }
        let state = lock(&self.state);
        Snapshot {
            refreshing: state.running.is_some(),
            ..state.answers.clone()
        }
    }

    /// The running refresh's end, or a new refresh's.
    fn refresh(&self, client: Arc<AetherClient>, targets: Targets) -> watch::Receiver<bool> {
        let mut state = lock(&self.state);
        // A refresh that panicked dropped its sender; it no longer runs.
        if let Some(running) = &state.running
            && running.has_changed().is_ok()
        {
            assert!(!*running.borrow(), "an ended refresh clears itself");
            return running.clone();
        }
        let (ended, done) = watch::channel(false);
        state.running = Some(done.clone());
        let cache = self.state.clone();
        tokio::spawn(async move {
            let (aether, mcp, github) = ask(&client, &targets).await;
            let mut state = lock(&cache);
            state.answers.aether.record(aether);
            state.answers.mcp.record(mcp);
            if let Some(github) = github {
                state.answers.github.record(github);
            }
            state.running = None;
            ended.send_replace(true);
        });
        done
    }
}

/// An ask's result with when it came back.
type Stamped<T> = (Result<T, ArchonError>, Timestamp);

type Answers = (
    Stamped<Option<Agent>>,
    Stamped<McpHealth>,
    Option<Stamped<WatcherStatus>>,
);

/// Every ask of one refresh, together, under one deadline.
async fn ask(client: &AetherClient, targets: &Targets) -> Answers {
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let aether = async {
        match &targets.orchestrator {
            Some(agent) => client.get_agent(agent).await.map(Some),
            None => client.ping().await.map(|()| None),
        }
    };
    let github = async {
        if !targets.github {
            return None;
        }
        Some(stamped(within(deadline, client.github_watcher())).await)
    };
    tokio::join!(
        stamped(within(deadline, aether)),
        stamped(within(deadline, client.mcp_health(SERVER_NAME))),
        github,
    )
}

async fn stamped<T>(ask: impl Future<Output = Result<T, ArchonError>>) -> Stamped<T> {
    let result = ask.await;
    (result, archon_core::now())
}

async fn within<T>(
    deadline: tokio::time::Instant,
    ask: impl Future<Output = Result<T, ArchonError>>,
) -> Result<T, ArchonError> {
    tokio::time::timeout_at(deadline, ask)
        .await
        .unwrap_or_else(|_| {
            Err(ArchonError::Unavailable(format!(
                "Aether did not answer within {}s",
                PROBE_DEADLINE.as_secs()
            )))
        })
}

/// The link is down: every section's latest ask failed with it.
fn record_all(answers: &mut Snapshot, error: &ArchonError, targets: &Targets) {
    let why = error.to_string();
    answers.aether.fail(why.clone());
    answers.mcp.fail(why.clone());
    if targets.github {
        answers.github.fail(why);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    // Only plain assignments run under the lock; a poisoned one is whole.
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
