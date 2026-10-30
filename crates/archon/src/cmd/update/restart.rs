//! What follows an install, so that the update takes effect now: update
//! Aether, restart archond at once while Aether and its stage agents keep
//! running, then give the orchestrator its new tools. A new Aether in
//! Archon's own aetherd waits for its stage agents, so archond does that
//! restart in the background. The sequence runs against [`Steps`], so tests
//! drive it with a fake. Every step is said on stderr and bounded, and a
//! signal ends any of them.

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
use super::version::Version;
use crate::cmd::doctor::find_program;
use crate::connect::{connect_existing, connect_running, unexpected};
use crate::instance::AETHER_PROGRAM;
use crate::views::fetch_status;

const POLL: Duration = Duration::from_secs(1);
/// How long the update waits for archond to come back on the installed
/// archon. One still finishing its own work restarts by itself after it.
pub(super) const SWITCH_LIMIT: Duration = Duration::from_secs(30);
/// The first archond whose restart keeping Aether leaves stage agents out of its drain.
const AGENTS_OUT_OF_DRAIN_SINCE: &str = "1.4.9";
/// How long an older archond, which waits for its stage agents, waits before
/// it restarts anyway: they keep running in Aether all the same.
const OLDER_ARCHOND_DRAIN: Duration = Duration::from_secs(10);
const AETHER_UPDATE_TIMEOUT: Duration = Duration::from_secs(600);
const MCP_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
/// Asking archond to restart Archon's own aetherd later; it answers at once.
const AETHER_RESTART_DEADLINE: Duration = Duration::from_secs(30);
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
    AskingArchond,
    RestartingArchond,
    CheckingOrchestrator,
    RefreshingTools,
    RestartingOrchestrator,
    RestartingAether,
}

impl Phase {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Preparing => "preparing the restart",
            Self::Asking => "waiting for the answer",
            Self::UpdatingAether => "updating Aether (`aether update`)",
            Self::FindingArchond => "asking whether archond runs",
            Self::AskingArchond => "asking archond to restart",
            Self::RestartingArchond => "restarting archond",
            Self::CheckingOrchestrator => "asking which archon created the orchestrator",
            Self::RefreshingTools => "refreshing Archon's tools in your aetherd",
            Self::RestartingOrchestrator => "restarting the orchestrator",
            Self::RestartingAether => {
                "asking archond to restart Archon's own aetherd once its stage agents finish"
            }
        }
    }

    /// What finishes the restart by hand once this step was cut short.
    pub(super) fn remedy(self) -> &'static str {
        match self {
            Self::Preparing | Self::Asking | Self::UpdatingAether | Self::FindingArchond => {
                "nothing was restarted; `archon update --yes` runs the restart"
            }
            Self::AskingArchond => {
                "archond may not have been asked to restart; `archon daemon restart \
                 --keep-aether` restarts it on the installed archon, and its running stage agents \
                 keep running"
            }
            Self::RestartingArchond => {
                "archond restarts on its own as it was asked, and its running stage agents keep \
                 running; `archon status` shows when it runs the new version"
            }
            Self::CheckingOrchestrator => {
                "`archon orchestrator status` names the archon that created it; if that is not \
                 the installed one, run `archon orchestrator restart`"
            }
            Self::RefreshingTools => {
                "run `archon aether mcp refresh archon`, then `archon orchestrator restart`"
            }
            Self::RestartingOrchestrator => "run `archon orchestrator restart`",
            Self::RestartingAether => {
                "archond runs the installed archon; `archon daemon restart` gives Archon's own \
                 aetherd the new Aether once its stage agents finish"
            }
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
    /// Ask archond to restart Archon's own aetherd once its stage agents
    /// finish; what archond answered.
    async fn restart_aether_later(&self, plan: &Plan) -> anyhow::Result<String>;
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
                "  - archond (pid {}), at once: running stage agents keep running in Aether",
                archond.pid
            ));
            lines.push(
                "  - the orchestrator agent, which is recreated with the new tools".to_owned(),
            );
        }
    }
    lines.push("Aether is updated first (`aether update`).".to_owned());
    lines.push(match plan.dedicated {
        true => format!(
            "A new Aether runs once Archon's own aetherd restarts, which archond does in the \
             background after its stage agents finish (at most {}s). Your own aetherd is not \
             touched.",
            plan.drain_secs
        ),
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
    assert!(
        !plan.installed.is_empty(),
        "the installed archon has a version"
    );
    let mut outcome = Outcome::default();
    let aether_changed = update_aether(steps, &mut outcome, progress).await;
    progress.enter(Phase::FindingArchond, "");
    let Some(old) = steps.running_archond().await? else {
        outcome
            .lines
            .push("archond is not running; nothing to restart.".to_owned());
        return Ok(outcome);
    };
    let current = bare_version(&old.version) == bare_version(&plan.installed);
    let switched = match plan.archon_changed || !current {
        true => switch_archond(steps, plan, &old, &mut outcome, progress).await?,
        false => {
            let line = format!("archond already runs archon {}.", plan.installed);
            outcome.lines.push(line);
            outcome = catch_up(steps, plan, outcome, progress).await?;
            true
        }
    };
    if aether_changed {
        aether_later(steps, plan, switched, &mut outcome, progress).await;
    }
    Ok(outcome)
}

/// `aether update`; whether it installed another Aether. A failure is a warning.
async fn update_aether(steps: &impl Steps, outcome: &mut Outcome, progress: &Progress) -> bool {
    let detail = format!("for at most {}s", AETHER_UPDATE_TIMEOUT.as_secs());
    progress.enter(Phase::UpdatingAether, &detail);
    match steps.update_aether().await {
        Ok(update) => {
            outcome.lines.push(update.line);
            update.changed
        }
        Err(error) => {
            let warning = format!("aether was not updated: {error:#}");
            outcome.warnings.push(warning);
            false
        }
    }
}

/// Restart archond on the installed archon, keeping Aether and the stage
/// agents in it, then renew the orchestrator. `false` when archond still
/// drains after [`SWITCH_LIMIT`] and restarts by itself later.
async fn switch_archond(
    steps: &impl Steps,
    plan: &Plan,
    old: &Running,
    outcome: &mut Outcome,
    progress: &Progress,
) -> anyhow::Result<bool> {
    assert!(old.pid > 0, "a running archond has a pid");
    let mut detail = format!(
        "pid {}, for at most {}s; running stage agents keep running",
        old.pid,
        SWITCH_LIMIT.as_secs()
    );
    let older = for_older_archond(plan, old);
    if let Some(older) = &older {
        detail.push_str(&format!(
            "; archond {} waits up to {}s for them first, then restarts anyway",
            old.version, older.drain_secs
        ));
    }
    let plan = older.as_ref().unwrap_or(plan);
    progress.enter(Phase::AskingArchond, &detail);
    if restart(steps, plan, old, SWITCH_LIMIT, progress).await? == Switched::Later {
        outcome.lines.push(restarts_later(plan, old));
        return Ok(false);
    }
    outcome
        .lines
        .push("Restarted archond; its running stage agents kept running in Aether.".to_owned());
    renew_orchestrator(steps, plan, outcome, progress).await?;
    outcome.restarted = true;
    Ok(true)
}

/// The request for `old` when it predates [`AGENTS_OUT_OF_DRAIN_SINCE`]: it
/// waits for stage agents even when it keeps Aether, so it waits briefly and
/// restarts anyway. Keeping Aether, that stops no agent.
fn for_older_archond(plan: &Plan, old: &Running) -> Option<Plan> {
    let first: Version = AGENTS_OUT_OF_DRAIN_SINCE
        .parse()
        .expect("a release version");
    let version = bare_version(&old.version).parse::<Version>();
    if version.is_ok_and(|version| version >= first) {
        return None;
    }
    let wait = OLDER_ARCHOND_DRAIN.as_secs();
    assert!(
        wait < SWITCH_LIMIT.as_secs(),
        "the update outwaits the drain"
    );
    Some(Plan {
        drain_secs: wait.min(plan.drain_secs),
        requeue: true,
        ..plan.clone()
    })
}

/// What happens to an archond still draining its own work when the update returns.
fn restarts_later(plan: &Plan, old: &Running) -> String {
    let then = match plan.requeue {
        true => "restarts anyway".to_owned(),
        false => format!(
            "gives the restart up after {}s, saying why in `archon log`",
            plan.drain_secs
        ),
    };
    format!(
        "archond (pid {}) still drains its own spawns, merges or deliveries after {}s; it \
         restarts on archon {} once they end, or {then}. Its stage agents keep running; \
         `archon status` shows the version it runs.",
        old.pid,
        SWITCH_LIMIT.as_secs(),
        plan.installed
    )
}

/// A new Aether runs once its aetherd restarts. archond restarts its own in
/// the background, after the stage agents in it finish; the operator's is
/// theirs. A failure is a warning: archond already runs the installed archon.
async fn aether_later(
    steps: &impl Steps,
    plan: &Plan,
    switched: bool,
    outcome: &mut Outcome,
    progress: &Progress,
) {
    let line = match (plan.dedicated, switched) {
        (false, _) => "Your aetherd runs the new Aether once you restart it: [aether] mode is \
                       \"shared\", so the update leaves it running."
            .to_owned(),
        (true, false) => "Archon's own aetherd keeps the old Aether: once archond is back, \
                          `archon daemon restart` restarts it after its stage agents finish."
            .to_owned(),
        (true, true) => {
            progress.enter(Phase::RestartingAether, "");
            let asked =
                tokio::time::timeout(AETHER_RESTART_DEADLINE, steps.restart_aether_later(plan))
                    .await
                    .unwrap_or_else(|_| {
                        let waited = AETHER_RESTART_DEADLINE.as_secs();
                        Err(anyhow::anyhow!("archond did not answer within {waited}s"))
                    });
            match asked {
                Ok(said) => format!(
                    "Archon's own aetherd restarts on the new Aether in the background, once its \
                     stage agents finish (at most {}s): {said}",
                    plan.drain_secs
                ),
                Err(error) => {
                    let warning = format!(
                        "archond was not asked to restart Archon's own aetherd, which keeps the \
                         old Aether until `archon daemon restart`: {error:#}"
                    );
                    outcome.warnings.push(warning);
                    return;
                }
            }
        }
    };
    outcome.lines.push(line);
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

/// How archond's restart went within the update's wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switched {
    /// A new archond answers.
    Now,
    /// The old one still drains, and restarts by itself once it has.
    Later,
}

/// Ask archond to restart by `deadline`; an error unless it accepted.
async fn ask_restart(
    steps: &impl Steps,
    plan: &Plan,
    old: &Running,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    assert!(old.pid > 0, "a running archond has a pid");
    let limit = deadline.saturating_duration_since(tokio::time::Instant::now());
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
        })
}

/// Ask archond to restart and poll until a new one answers, for `limit` at
/// most: a poll that does not answer is cut off by the same deadline.
async fn restart(
    steps: &impl Steps,
    plan: &Plan,
    old: &Running,
    limit: Duration,
    progress: &Progress,
) -> anyhow::Result<Switched> {
    assert!(limit >= POLL, "the wait allows a poll");
    let deadline = tokio::time::Instant::now() + limit;
    ask_restart(steps, plan, old, deadline).await?;
    // Accepted: from here archond restarts whether or not this one waits for it.
    progress.mark(Phase::RestartingArchond);
    let polls = limit.as_secs() / POLL.as_secs();
    let (mut idle, mut last_down, mut draining) = (0, None, false);
    for _ in 0..polls {
        steps.pause().await;
        let Ok(seen) = tokio::time::timeout_at(deadline, steps.observe(old.pid)).await else {
            last_down = Some("the last poll got no answer".to_owned());
            break;
        };
        draining = seen == Seen::Old { upgrading: true };
        match seen {
            Seen::New => return Ok(Switched::Now),
            Seen::Down(why) => {
                idle = 0;
                last_down = why.or(last_down);
            }
            Seen::Old { upgrading: true } => idle = 0,
            Seen::Old { upgrading: false } => idle += 1,
        }
        anyhow::ensure!(
            idle < GIVE_UP_AFTER,
            "archond (pid {}) gave up the restart: its own spawns, merges or deliveries still ran \
             after {}s. Nothing was restarted; `archon log` has the reason. Ask again with \
             `archon update --wait SECS`, or `--requeue` to restart anyway.",
            old.pid,
            plan.drain_secs
        );
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    if draining {
        return Ok(Switched::Later);
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
        // Aether and the stage agents in it keep running for the next archond.
        let request = UpgradeRequest {
            archon: None,
            aether: None,
            drain_secs: plan.drain_secs,
            requeue: plan.requeue,
            keep_aether: true,
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
        tools::run(&aether, &args, MCP_REFRESH_TIMEOUT).await?;
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

    async fn restart_aether_later(&self, plan: &Plan) -> anyhow::Result<String> {
        assert!(
            self.archon.is_absolute(),
            "the installed archon is named whole"
        );
        let wait = plan.drain_secs.to_string();
        let mut args = ["--json", "daemon", "restart", "--wait", &wait]
            .map(OsStr::new)
            .to_vec();
        if plan.requeue {
            args.push(OsStr::new("--requeue"));
        }
        // The installed archon speaks the restarted archond's protocol.
        let ran = tools::run(&self.archon, &args, AETHER_RESTART_DEADLINE).await?;
        let said: serde_json::Value = serde_json::from_slice(&ran.stdout).with_context(|| {
            format!(
                "`{} --json daemon restart` printed no JSON",
                self.archon.display()
            )
        })?;
        let started = said["started"]
            .as_str()
            .filter(|text| !text.is_empty())
            .with_context(|| format!("archond did not say it started the restart: {said}"))?;
        Ok(started.to_owned())
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
