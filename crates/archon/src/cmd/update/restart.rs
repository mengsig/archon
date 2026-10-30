//! What follows an install, so that the update takes effect now: update
//! Aether, restart archond and Archon's own aetherd through the drained
//! restart archond already has, then give the orchestrator its new tools.
//! The sequence runs against [`Steps`], so tests drive it with a fake.
//! Every step is said on stderr and bounded, and a signal ends any of them.

use std::cell::Cell;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use archon_core::error::ArchonError;
use archon_core::ops::{UpgradeRequest, bare_version};
use archon_core::protocol::{ErrorCode, OrchestratorInfo, Reply, Request};

use super::interrupt::Interrupt;
use super::tools;
use crate::cmd::doctor::find_program;
use crate::connect::{connect_existing, connect_running, unexpected};
use crate::instance::AETHER_PROGRAM;
use crate::views::fetch_status;

const POLL: Duration = Duration::from_secs(1);
/// After the drain: the helper waits for archond to exit, then both restart.
const RESTART_MARGIN: Duration = Duration::from_secs(300);
const AETHER_UPDATE_TIMEOUT: Duration = Duration::from_secs(600);
/// Polls in a row that find archond not upgrading before the restart is
/// taken as given up.
const GIVE_UP_AFTER: u32 = 3;
const ORCHESTRATOR_TRIES: u32 = 60;
/// The whole orchestrator restart, retries included: archond's link to
/// Aether may still be coming up when it is first asked.
pub(super) const ORCHESTRATOR_DEADLINE: Duration = Duration::from_secs(180);
/// Asking archond which archon created the running orchestrator.
const CHECK_DEADLINE: Duration = Duration::from_secs(60);
/// The creator of a generation an archon made before it recorded one.
const UNRECORDED: &str = "an archon that did not record its version";

/// A step of the restart, named when a signal or a deadline ends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Preparing,
    Asking,
    UpdatingAether,
    FindingArchond,
    RestartingArchond,
    CheckingOrchestrator,
    RefreshingTools,
    RestartingOrchestrator,
}

impl Phase {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Preparing => "preparing the restart",
            Self::Asking => "waiting for the answer",
            Self::UpdatingAether => "updating Aether (`aether update`)",
            Self::FindingArchond => "asking whether archond runs",
            Self::RestartingArchond => "restarting archond",
            Self::CheckingOrchestrator => "asking which archon created the orchestrator",
            Self::RefreshingTools => "refreshing Archon's tools in your aetherd",
            Self::RestartingOrchestrator => "restarting the orchestrator",
        }
    }

    /// What finishes the restart by hand once this step was cut short.
    pub(super) fn remedy(self) -> &'static str {
        match self {
            Self::Preparing | Self::Asking | Self::UpdatingAether | Self::FindingArchond => {
                "nothing was restarted; `archon update --yes` runs the restart"
            }
            Self::RestartingArchond => {
                "archond restarts on its own as it was asked; `archon status` shows when it \
                 runs the new version, then run `archon orchestrator restart`"
            }
            Self::CheckingOrchestrator => {
                "`archon orchestrator status` names the archon that created it; if that is not \
                 the installed one, run `archon orchestrator restart`"
            }
            Self::RefreshingTools => {
                "run `archon aether mcp refresh archon`, then `archon orchestrator restart`"
            }
            Self::RestartingOrchestrator => "run `archon orchestrator restart`",
        }
    }
}

/// The step under way, kept apart from the sequence so that whoever ends
/// it can still name where it stopped.
#[derive(Debug)]
pub(super) struct Progress(Cell<Phase>);

impl Progress {
    pub(super) fn new() -> Self {
        Self(Cell::new(Phase::Preparing))
    }

    pub(super) fn current(&self) -> Phase {
        self.0.get()
    }

    /// Record `phase` without a word: the operator sees what it waits for.
    pub(super) fn mark(&self, phase: Phase) {
        self.0.set(phase);
    }

    /// Record `phase` and say it on stderr, so a stall shows where it is.
    fn enter(&self, phase: Phase, detail: &str) {
        assert_ne!(phase, Phase::Asking, "the question speaks for itself");
        self.mark(phase);
        match detail.is_empty() {
            true => eprintln!("archon: update: {}.", phase.describe()),
            false => eprintln!("archon: update: {} ({detail}).", phase.describe()),
        }
    }
}

/// How work that a signal may end has ended.
#[derive(Debug)]
pub(super) enum Ended<T> {
    Done(T),
    Interrupted { by: Interrupt, during: Phase },
}

/// Run `work` until it ends or `interrupt` arrives; a signal drops it where
/// it waits, which kills a child it runs, and names the step it was at.
pub(super) async fn until_interrupted<T>(
    work: impl Future<Output = T>,
    progress: &Progress,
    interrupt: impl Future<Output = Interrupt>,
) -> Ended<T> {
    tokio::select! {
        biased;
        by = interrupt => Ended::Interrupted { by, during: progress.current() },
        done = work => Ended::Done(done),
    }
}

/// What `archon update` decided before the restart.
#[derive(Debug, Clone)]
pub(super) struct Plan {
    /// The version of the archon that is installed now.
    pub installed: String,
    pub archon_changed: bool,
    /// `[aether] mode = "dedicated"`: Archon runs its own aetherd.
    pub dedicated: bool,
    pub drain_secs: u64,
    pub requeue: bool,
}

/// archond as `observe` sees it while it restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Seen {
    Down(Option<String>),
    /// The archond that was asked, still running; `upgrading` is whether it
    /// still holds an upgrade.
    Old {
        upgrading: bool,
    },
    New,
}

/// A running archond: its pid and the version it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Running {
    pub pid: u32,
    pub version: String,
}

/// The effect of `aether update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AetherUpdate {
    pub changed: bool,
    pub line: String,
}

/// Every effect of the restart, so tests can stand in for the machine.
pub(super) trait Steps {
    async fn update_aether(&self) -> anyhow::Result<AetherUpdate>;
    async fn running_archond(&self) -> anyhow::Result<Option<Running>>;
    async fn request_restart(&self, plan: &Plan) -> anyhow::Result<()>;
    async fn observe(&self, old_pid: u32) -> Seen;
    async fn refresh_mcp(&self) -> anyhow::Result<()>;
    /// The Archon version that created the orchestrator generation now
    /// running, [`UNRECORDED`] if it was not kept; `None` when none runs.
    async fn orchestrator_creator(&self) -> anyhow::Result<Option<String>>;
    /// The Archon version that created the generation now running.
    async fn restart_orchestrator(&self) -> anyhow::Result<String>;
    async fn pause(&self);
}

/// What the sequence did, line by line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Outcome {
    pub lines: Vec<String>,
    /// The restart ran to its end.
    pub restarted: bool,
    /// A step that failed but stopped nothing; the update then exits 1.
    pub warnings: Vec<String>,
}

/// The lines that say what is about to be updated and restarted.
pub(super) fn announce(plan: &Plan, running: Option<&Running>) -> String {
    let mut lines = vec!["The update takes effect once these restart:".to_owned()];
    match running {
        None => lines.push(
            "  - nothing: archond is not running, and the next command that needs it starts the \
             new version."
                .to_owned(),
        ),
        Some(archond) => {
            lines.push(format!(
                "  - archond (pid {}), after the running stage agents finish (at most {}s{})",
                archond.pid,
                plan.drain_secs,
                match plan.requeue {
                    true => ", then regardless: stopped stages are retried",
                    false => "; the restart is given up if they do not",
                }
            ));
            if plan.dedicated {
                lines.push("  - Archon's own aetherd; running items resume".to_owned());
            }
            lines.push(
                "  - the orchestrator agent, which is recreated with the new tools".to_owned(),
            );
        }
    }
    lines.push("Aether is updated first (`aether update`).".to_owned());
    lines.push(match plan.dedicated {
        true => "Your own aetherd is not touched.".to_owned(),
        false => "Your aetherd is not restarted: [aether] mode is \"shared\".".to_owned(),
    });
    lines.join("\n")
}

/// Run the sequence. A failure of the restart itself is an error; a failed
/// `aether update` is a warning, since archond restarts on what is installed.
pub(super) async fn apply(
    steps: &impl Steps,
    plan: &Plan,
    progress: &Progress,
) -> anyhow::Result<Outcome> {
    assert!(plan.drain_secs > 0, "a restart waits at least a second");
    let mut outcome = Outcome::default();
    let mut aether_changed = false;
    progress.enter(Phase::UpdatingAether, "");
    match steps.update_aether().await {
        Ok(update) => {
            aether_changed = update.changed;
            outcome.lines.push(update.line);
        }
        Err(error) => outcome
            .warnings
            .push(format!("aether was not updated: {error:#}")),
    }
    progress.enter(Phase::FindingArchond, "");
    let Some(old) = steps.running_archond().await? else {
        outcome
            .lines
            .push("archond is not running; nothing to restart.".to_owned());
        return Ok(outcome);
    };
    let current = bare_version(&old.version) == bare_version(&plan.installed);
    if !plan.archon_changed && !aether_changed && current {
        let line = format!("archond already runs archon {}.", plan.installed);
        outcome.lines.push(line);
        return catch_up(steps, plan, outcome, progress).await;
    }
    let limit = Duration::from_secs(plan.drain_secs) + RESTART_MARGIN;
    let detail = format!("pid {}, for at most {}s", old.pid, limit.as_secs());
    progress.enter(Phase::RestartingArchond, &detail);
    restart(steps, plan, &old, limit).await?;
    outcome.lines.push(match plan.dedicated {
        true => "Restarted archond and Archon's own aetherd.".to_owned(),
        false => "Restarted archond.".to_owned(),
    });
    renew_orchestrator(steps, plan, &mut outcome, progress).await?;
    outcome.restarted = true;
    Ok(outcome)
}

/// archond already runs the installed archon, yet an update cut short after
/// its restart may have left the orchestrator on the old one: renew it then.
async fn catch_up(
    steps: &impl Steps,
    plan: &Plan,
    mut outcome: Outcome,
    progress: &Progress,
) -> anyhow::Result<Outcome> {
    assert!(!plan.archon_changed, "only a current archond is caught up");
    progress.enter(Phase::CheckingOrchestrator, "");
    let creator = tokio::time::timeout(CHECK_DEADLINE, steps.orchestrator_creator())
        .await
        .unwrap_or_else(|_| {
            let waited = CHECK_DEADLINE.as_secs();
            Err(anyhow::anyhow!("archond did not answer within {waited}s"))
        })
        .context("archond could not say which archon created the orchestrator")?;
    let line = match creator {
        None => "No orchestrator runs; archond creates it on the installed archon.".to_owned(),
        Some(by) if bare_version(&by) == bare_version(&plan.installed) => {
            format!(
                "The orchestrator already runs on archon {}.",
                plan.installed
            )
        }
        Some(by) => {
            let line = format!(
                "The orchestrator was created by {by}, not {}.",
                plan.installed
            );
            outcome.lines.push(line);
            renew_orchestrator(steps, plan, &mut outcome, progress).await?;
            outcome.restarted = true;
            return Ok(outcome);
        }
    };
    outcome.lines.push(line);
    Ok(outcome)
}

/// Ask archond to restart and poll until a new one answers, for `limit` at
/// most: a poll that does not answer is cut off by the same deadline.
async fn restart(
    steps: &impl Steps,
    plan: &Plan,
    old: &Running,
    limit: Duration,
) -> anyhow::Result<()> {
    assert!(limit >= POLL, "the wait allows a poll");
    let deadline = tokio::time::Instant::now() + limit;
    tokio::time::timeout_at(deadline, steps.request_restart(plan))
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("archond did not answer the request")))
        .with_context(|| {
            format!(
                "archond (pid {}) was not asked to restart within {}s; `archon daemon status` \
                 shows whether it restarts",
                old.pid,
                limit.as_secs()
            )
        })?;
    let polls = limit.as_secs() / POLL.as_secs();
    let (mut idle, mut last_down) = (0, None);
    for _ in 0..polls {
        steps.pause().await;
        let Ok(seen) = tokio::time::timeout_at(deadline, steps.observe(old.pid)).await else {
            last_down = Some("the last poll got no answer".to_owned());
            break;
        };
        match seen {
            Seen::New => return Ok(()),
            Seen::Down(why) => {
                idle = 0;
                last_down = why.or(last_down);
            }
            Seen::Old { upgrading: true } => idle = 0,
            Seen::Old { upgrading: false } => idle += 1,
        }
        anyhow::ensure!(
            idle < GIVE_UP_AFTER,
            "archond (pid {}) gave up the restart: stage agents still ran after {}s. Nothing was \
             restarted; `archon log` has the reason. Ask again with `archon update --wait SECS`, \
             or `--requeue` to restart anyway.",
            old.pid,
            plan.drain_secs
        );
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    let why = last_down.map_or(String::new(), |why| format!(" ({why})"));
    anyhow::bail!(
        "archond (pid {}) was asked to restart, and no new one answered within {}s{why}; \
         see its startup log with `archon daemon status`",
        old.pid,
        limit.as_secs()
    )
}

/// Give the orchestrator a generation the installed archon creates, with
/// its tools refreshed first in a shared aetherd.
async fn renew_orchestrator(
    steps: &impl Steps,
    plan: &Plan,
    outcome: &mut Outcome,
    progress: &Progress,
) -> anyhow::Result<()> {
    if !plan.dedicated {
        progress.enter(Phase::RefreshingTools, "");
        steps
            .refresh_mcp()
            .await
            .context("your aetherd could not refresh Archon's tools")?;
        outcome
            .lines
            .push("Refreshed Archon's tools in your aetherd.".to_owned());
    }
    let detail = format!("for at most {}s", ORCHESTRATOR_DEADLINE.as_secs());
    progress.enter(Phase::RestartingOrchestrator, &detail);
    let created_by = tokio::time::timeout(ORCHESTRATOR_DEADLINE, steps.restart_orchestrator())
        .await
        .unwrap_or_else(|_| {
            let waited = ORCHESTRATOR_DEADLINE.as_secs();
            Err(anyhow::anyhow!(
                "archond did not finish it within {waited}s"
            ))
        })
        .context("the orchestrator did not restart (`archon orchestrator restart`)")?;
    anyhow::ensure!(
        bare_version(&created_by) == bare_version(&plan.installed),
        "the orchestrator restarted, but archon {created_by} created its new generation, not \
         {}: archond does not run the installed archon (`archon status`)",
        plan.installed
    );
    outcome.lines.push(format!(
        "Restarted the orchestrator on archon {}.",
        plan.installed
    ));
    Ok(())
}

/// The steps on this machine. `archon` is the installed archon, which asks
/// for what this one cannot once archond restarts on another protocol.
pub(super) struct Live {
    pub archon: PathBuf,
}

impl Steps for Live {
    async fn update_aether(&self) -> anyhow::Result<AetherUpdate> {
        let aether = find_program(Path::new(AETHER_PROGRAM))
            .context("no `aether` was found on PATH to update")?;
        let before = aether_version(&aether).await?;
        let args = [OsStr::new("update"), OsStr::new("--yes")];
        let ran = tools::run(&aether, &args, AETHER_UPDATE_TIMEOUT).await?;
        let after = aether_version(&aether).await?;
        let said = String::from_utf8_lossy(&ran.stdout);
        let line = match before == after {
            true => format!("Aether is {after}; `aether update` changed nothing."),
            false => format!("Updated Aether from {before} to {after}."),
        };
        tracing::info!(output = %said.trim(), "aether update");
        Ok(AetherUpdate {
            changed: before != after,
            line,
        })
    }

    async fn running_archond(&self) -> anyhow::Result<Option<Running>> {
        Ok(connect_existing().await?.map(|client| {
            let welcome = client.welcome();
            Running {
                pid: welcome.pid,
                version: welcome.daemon_version.clone(),
            }
        }))
    }

    async fn request_restart(&self, plan: &Plan) -> anyhow::Result<()> {
        let request = UpgradeRequest {
            archon: None,
            aether: None,
            drain_secs: plan.drain_secs,
            requeue: plan.requeue,
            keep_aether: false,
        };
        request.check()?;
        let client = connect_running().await?;
        match client.call(Request::Upgrade(request)).await? {
            Ok(Reply::UpgradeStarted { .. }) => Ok(()),
            Ok(other) => Err(unexpected(&other, "upgrade_started")),
            Err(refusal) => Err(archon_client::refusal_error(refusal).into()),
        }
    }

    async fn observe(&self, old_pid: u32) -> Seen {
        let client = match connect_existing().await {
            Ok(Some(client)) => client,
            Ok(None) => return Seen::Down(None),
            // The archond asked to restart spoke this protocol, so one that does not is new.
            Err(error) if speaks_another_protocol(&error) => return Seen::New,
            Err(error) => return Seen::Down(Some(format!("{error:#}"))),
        };
        if client.welcome().pid != old_pid {
            return Seen::New;
        }
        match fetch_status(&client).await {
            Ok(status) => Seen::Old {
                upgrading: status.upgrade.is_some(),
            },
            Err(error) => Seen::Down(Some(format!("{error:#}"))),
        }
    }

    async fn refresh_mcp(&self) -> anyhow::Result<()> {
        let aether = find_program(Path::new(AETHER_PROGRAM))
            .context("no `aether` was found on PATH to refresh Archon's tools")?;
        let args = ["mcp", "refresh", "archon"].map(OsStr::new);
        tools::run(&aether, &args, AETHER_UPDATE_TIMEOUT).await?;
        Ok(())
    }

    async fn orchestrator_creator(&self) -> anyhow::Result<Option<String>> {
        let client = connect_running().await?;
        match client.call(Request::OrchestratorStatus).await? {
            Ok(Reply::Orchestrator(info)) => Ok(Some(
                info.freshness
                    .map_or_else(|| UNRECORDED.to_owned(), |fresh| fresh.created_by),
            )),
            Ok(other) => Err(unexpected(&other, "orchestrator")),
            Err(refusal) if refusal.code == ErrorCode::NotFound => Ok(None),
            Err(refusal) => Err(archon_client::refusal_error(refusal).into()),
        }
    }

    async fn restart_orchestrator(&self) -> anyhow::Result<String> {
        // The new archond answers before it reaches Aether, which it is told to do within seconds.
        let mut last = None;
        for _ in 0..ORCHESTRATOR_TRIES {
            let refusal = match self.ask_orchestrator_restart().await? {
                Ok(created_by) => return Ok(created_by),
                Err(refusal) => refusal,
            };
            if last.is_none() {
                eprintln!(
                    "archon: update: archond refused the orchestrator restart ({refusal:#}); \
                     asking again every second."
                );
            }
            last = Some(refusal);
            tokio::time::sleep(POLL).await;
        }
        Err(last.context("the orchestrator was never asked")?)
    }

    async fn pause(&self) {
        tokio::time::sleep(POLL).await;
    }
}

impl Live {
    /// Ask once for an orchestrator restart. `Ok(Err)` is a refusal worth
    /// asking again; an archond on another protocol is asked by `archon`.
    async fn ask_orchestrator_restart(&self) -> anyhow::Result<anyhow::Result<String>> {
        let client = match connect_running().await {
            Ok(client) => client,
            Err(error) if speaks_another_protocol(&error) => {
                return self.installed_restarts_orchestrator().await;
            }
            Err(error) => return Err(error),
        };
        match client.call(Request::OrchestratorRestart).await? {
            Ok(Reply::Orchestrator(info)) => created_by(&info).map(Ok),
            Ok(other) => Err(unexpected(&other, "orchestrator")),
            Err(refusal) => Ok(Err(archon_client::refusal_error(refusal).into())),
        }
    }

    /// `archon orchestrator restart` run by the installed archon. Its
    /// failure is a refusal to ask again; an answer without a creator is not.
    async fn installed_restarts_orchestrator(&self) -> anyhow::Result<anyhow::Result<String>> {
        assert!(
            self.archon.is_absolute(),
            "the installed archon is named whole"
        );
        let args = ["--json", "orchestrator", "restart"].map(OsStr::new);
        let ran = match tools::run(&self.archon, &args, ORCHESTRATOR_DEADLINE).await {
            Ok(ran) => ran,
            Err(refused) => return Ok(Err(refused)),
        };
        let shown = self.archon.display();
        let info: serde_json::Value = serde_json::from_slice(&ran.stdout)
            .with_context(|| format!("`{shown} --json orchestrator restart` printed no JSON"))?;
        let creator = info["freshness"]["created_by"]
            .as_str()
            .filter(|by| !by.is_empty())
            .with_context(|| {
                format!("`{shown}` restarted the orchestrator, but named no creator: {info}")
            })?;
        Ok(Ok(creator.to_owned()))
    }
}

/// Whether connecting failed because archond speaks another protocol.
fn speaks_another_protocol(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ArchonError>()
        .is_some_and(archon_client::is_incompatible)
}

/// The version that created the generation `info` describes.
fn created_by(info: &OrchestratorInfo) -> anyhow::Result<String> {
    let fresh = info.freshness.as_ref().with_context(|| {
        format!(
            "archond restarted the orchestrator, but recorded no creator for generation {}",
            info.generation
        )
    })?;
    assert!(
        !fresh.created_by.is_empty(),
        "a recorded creator has a version"
    );
    Ok(fresh.created_by.clone())
}

async fn aether_version(aether: &Path) -> anyhow::Result<String> {
    let args = [OsStr::new("--version")];
    let output = tools::run(aether, &args, Duration::from_secs(20)).await?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    anyhow::ensure!(
        !version.is_empty(),
        "`{} --version` printed nothing",
        aether.display()
    );
    Ok(version)
}

/// Whether a typed answer to `[Y/n]` is a yes: empty is, as in `install.sh`.
pub(super) fn is_yes(answer: &str) -> bool {
    !answer.trim().to_ascii_lowercase().starts_with('n')
}

#[cfg(test)]
mod tests;
