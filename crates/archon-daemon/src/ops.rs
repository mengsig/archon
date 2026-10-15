//! The operator's drained upgrade and restart of Archon (ARCHITECTURE §16):
//! install what was named, hold dispatch until the running stage agents
//! finish, hand the restart to a detached `archon daemon relaunch`, stop,
//! and report the versions once the next archond is up. archond never asks
//! the network for releases by itself; only `CheckUpdates` does.

pub mod install;
pub mod system;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::ops::{ProgramVersions, UpgradeRequest, UpgradeTarget, Versions, bare_version};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

pub use self::install::{InstallStep, Program};
use crate::context::DaemonContext;
use crate::feed;

/// How often a drain counts what still runs.
pub const DRAIN_POLL: Duration = Duration::from_secs(5);
/// How often the installed versions are read again, from disk only.
const PROBE_EVERY: Duration = Duration::from_secs(10 * 60);
/// How long the report after a restart waits for Aether's version.
const LINK_WAIT: Duration = Duration::from_secs(60);
const LINK_POLL: Duration = Duration::from_millis(500);
/// `kv`: the upgrade whose restart the next archond reports.
const RESTART_KEY: &str = "upgrade_restart";
const SHARED_AETHER: &str = "[aether] mode is \"shared\": the operator's aetherd is theirs \
     to upgrade, with `aether update`; upgrade archon alone";

/// What only the operating system can do for an upgrade; faked in tests.
#[async_trait::async_trait]
pub trait OpsPort: Send + Sync + 'static {
    /// The binary an upgrade of `program` replaces; `None` when absent.
    fn locate(&self, program: Program) -> Option<PathBuf>;
    /// cargo's bin directory, canonical; `None` when there is none.
    fn cargo_bin(&self) -> Option<PathBuf>;
    async fn install(&self, step: &InstallStep) -> Result<(), ArchonError>;
    /// What the binary on disk says it is; `None` when there is none.
    async fn installed_version(&self, program: Program) -> Result<Option<String>, ArchonError>;
    /// The latest published release, asked of the network.
    async fn latest_release(&self, program: Program) -> Result<Option<String>, ArchonError>;
    /// Start the detached helper that waits for this archond to exit and
    /// then starts the next one; it must outlive archond.
    fn spawn_restart(&self, plan: &RestartPlan) -> Result<(), ArchonError>;
}

/// What the restart helper is told. It waits for this archond's lock on
/// the data directory to go, so it needs no pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPlan {
    /// Stop Archon's own aetherd too, so its next start runs what is installed.
    pub restart_aether: bool,
}

impl RestartPlan {
    /// `archon` arguments of the helper.
    pub fn helper_args(&self) -> Vec<&'static str> {
        match self.restart_aether {
            true => vec!["daemon", "relaunch"],
            false => vec!["daemon", "relaunch", "--keep-aether"],
        }
    }
}

/// The upgrade state of one archond, in its context.
pub struct OpsState {
    port: Arc<dyn OpsPort>,
    poll: Duration,
    /// Set while an upgrade holds dispatch; every dispatch snapshot reads it.
    draining: AtomicBool,
    job: Mutex<Job>,
    wake: Notify,
    /// Signalled once the restart is handed off: archond stops then.
    stop: Notify,
    /// Installed and latest versions as last read; `running` is read live.
    seen: Mutex<Versions>,
}

#[derive(Debug, Default)]
struct Job {
    /// Accepted, and not yet taken by the worker.
    queued: Option<UpgradeRequest>,
    /// What the upgrade under way is doing; `None` while there is none.
    phase: Option<String>,
}

impl std::fmt::Debug for OpsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpsState")
            .field("draining", &self.draining())
            .finish_non_exhaustive()
    }
}

impl OpsState {
    pub fn new(port: Arc<dyn OpsPort>, poll: Duration) -> Self {
        assert!(!poll.is_zero(), "a drain waits between counts");
        Self {
            port,
            poll,
            draining: AtomicBool::new(false),
            job: Mutex::new(Job::default()),
            wake: Notify::new(),
            stop: Notify::new(),
            seen: Mutex::new(Versions::default()),
        }
    }

    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Resolves once an upgrade handed the restart off.
    pub async fn stopping(&self) {
        self.stop.notified().await;
    }

    fn hold(&self, ctx: &DaemonContext, held: bool) {
        self.draining.store(held, Ordering::SeqCst);
        ctx.dispatch.notify_one();
    }
}

/// Accept `request` for the ops worker; one upgrade at a time.
pub async fn start(ctx: &DaemonContext, request: UpgradeRequest) -> Result<String, ArchonError> {
    request.check()?;
    if request.aether.is_some() && ctx.config.aether.mode != AetherMode::Dedicated {
        return Err(ArchonError::Invalid(SHARED_AETHER.into()));
    }
    let what = request.describe();
    let mut job = ctx.ops.job.lock().await;
    if let Some(phase) = &job.phase {
        return Err(ArchonError::Unavailable(format!(
            "an upgrade is already under way: {phase}"
        )));
    }
    job.phase = Some(format!("{what}: about to start"));
    job.queued = Some(request);
    drop(job);
    ctx.ops.wake.notify_one();
    Ok(format!(
        "Accepted {what}; each step is reported in the feed, and `archon status` shows how far it is."
    ))
}

/// What the upgrade under way is doing, for `status`.
pub async fn phase(ctx: &DaemonContext) -> Option<String> {
    ctx.ops.job.lock().await.phase.clone()
}

/// The running versions, with the installed and latest ones last read.
pub async fn versions(ctx: &DaemonContext) -> Versions {
    let mut versions = ctx.ops.seen.lock().await.clone();
    versions.archon.running = Some(env!("CARGO_PKG_VERSION").to_owned());
    versions.aether.running = ctx
        .aether
        .client()
        .ok()
        .map(|client| bare_version(&client.welcome().daemon_version).to_owned());
    versions
}

/// Ask the network for the latest releases (the operator's request) and
/// read the installed versions again.
pub async fn check_updates(ctx: &DaemonContext) -> Result<Versions, ArchonError> {
    refresh_installed(ctx).await;
    for program in [Program::Archon, Program::Aether] {
        let latest = ctx.ops.port.latest_release(program).await?;
        let mut seen = ctx.ops.seen.lock().await;
        program_versions(&mut seen, program).latest = latest;
    }
    Ok(versions(ctx).await)
}

fn program_versions(versions: &mut Versions, program: Program) -> &mut ProgramVersions {
    match program {
        Program::Archon => &mut versions.archon,
        Program::Aether => &mut versions.aether,
    }
}

/// Read the installed versions; one that cannot be read keeps the last.
async fn refresh_installed(ctx: &DaemonContext) {
    for program in [Program::Archon, Program::Aether] {
        match ctx.ops.port.installed_version(program).await {
            Ok(installed) => {
                let mut seen = ctx.ops.seen.lock().await;
                program_versions(&mut seen, program).installed = installed;
            }
            Err(error) => tracing::warn!(%program, %error, "installed version not read"),
        }
    }
}

/// The ops worker: reports a restart this archond came from, keeps the
/// installed versions current and runs each accepted upgrade.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    report_restart(&ctx).await?;
    loop {
        refresh_installed(&ctx).await;
        if tokio::time::timeout(PROBE_EVERY, ctx.ops.wake.notified())
            .await
            .is_err()
        {
            continue;
        }
        let Some(request) = ctx.ops.job.lock().await.queued.take() else {
            continue;
        };
        if upgrade(&ctx, &request).await? {
            ctx.ops.stop.notify_one();
            // archond stops now and aborts this worker with the others.
            return std::future::pending().await;
        }
    }
}

/// One upgrade; `true` once the restart is handed off. Any other end is
/// reported and releases dispatch; only a store failure is returned.
async fn upgrade(ctx: &DaemonContext, request: &UpgradeRequest) -> Result<bool, ArchonError> {
    let what = request.describe();
    let before = versions(ctx).await;
    feed::system(ctx, format!("Upgrade started: {what}.")).await?;
    match install_all(ctx, request, &what).await {
        Ok(()) => {}
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            let why = format!("{what} stopped: {error}. Nothing was restarted.");
            return give_up(ctx, why).await.map(|()| false);
        }
    }
    let limit = Duration::from_secs(request.drain_secs);
    ctx.ops.hold(ctx, true);
    let busy = drain(ctx, &what, limit).await?;
    if busy > 0 && !request.requeue {
        let why = format!(
            "{what} gave up: {busy} stage agents still ran after {}s, so nothing was restarted \
             and dispatch goes on. Ask again with a longer wait, or with requeue to restart anyway.",
            limit.as_secs()
        );
        return give_up(ctx, why).await.map(|()| false);
    }
    let restart_aether = ctx.config.aether.mode == AetherMode::Dedicated && !request.keep_aether;
    hand_off(
        ctx,
        Restarted {
            what,
            before,
            restart_aether,
        },
        busy,
    )
    .await
}

/// Plan every install before running any, so a refused one installs nothing.
async fn install_all(
    ctx: &DaemonContext,
    request: &UpgradeRequest,
    what: &str,
) -> Result<(), ArchonError> {
    let mut steps = Vec::new();
    for (program, target) in [
        (Program::Archon, &request.archon),
        (Program::Aether, &request.aether),
    ] {
        if let Some(target) = target {
            steps.push(install_step(ctx, program, target).await?);
        }
    }
    for step in &steps {
        set_phase(ctx, format!("{what}: installing {}", step.program)).await;
        feed::system(
            ctx,
            format!("Installing {} with {}.", step.program, step.describe()),
        )
        .await?;
        ctx.ops.port.install(step).await?;
    }
    Ok(())
}

/// The command that installs `target` of `program`; a cargo build's
/// `latest` is first named by the program's own release check.
async fn install_step(
    ctx: &DaemonContext,
    program: Program,
    target: &UpgradeTarget,
) -> Result<InstallStep, ArchonError> {
    let port = &ctx.ops.port;
    let binary = port.locate(program).ok_or_else(|| {
        ArchonError::Unavailable(format!("no installed {program} was found to upgrade"))
    })?;
    let cargo_bin = port.cargo_bin();
    let target = match target {
        UpgradeTarget::Latest if install::by_cargo(&binary, cargo_bin.as_deref()) => {
            let latest = port.latest_release(program).await?.ok_or_else(|| {
                ArchonError::Unavailable(format!("the latest {program} release is unknown"))
            })?;
            UpgradeTarget::parse(&latest)?
        }
        target => target.clone(),
    };
    install::plan(program, &target, &binary, cargo_bin.as_deref())
}

/// Wait until no stage agent runs, or `limit` passes; how many still run.
async fn drain(ctx: &DaemonContext, what: &str, limit: Duration) -> Result<u32, ArchonError> {
    let deadline = Instant::now() + limit;
    let mut told = false;
    loop {
        let busy = busy(ctx).await?;
        let now = Instant::now();
        if busy == 0 || now >= deadline {
            return Ok(busy);
        }
        let left = deadline - now;
        if !told {
            let note = format!(
                "Holding dispatch until {busy} running stage agents finish, for at most {}s.",
                left.as_secs()
            );
            feed::system(ctx, note).await?;
            told = true;
        }
        let phase = format!(
            "{what}: draining, {busy} still running, {}s left",
            left.as_secs()
        );
        set_phase(ctx, phase).await;
        tokio::time::sleep(ctx.ops.poll.min(left)).await;
    }
}

/// Stage agents running, and spawns, merges and deliveries under way.
async fn busy(ctx: &DaemonContext) -> Result<u32, ArchonError> {
    let working = {
        let table = ctx.runs.lock().await;
        table.in_flight.len() + table.merging.len() + table.shipping.len()
    };
    let running = ctx
        .store
        .lock()
        .await
        .dispatch_snapshot(archon_core::now())?
        .running_total;
    Ok(running.saturating_add(u32::try_from(working).unwrap_or(u32::MAX)))
}

/// Record the upgrade for the next archond, then start the restart helper.
async fn hand_off(
    ctx: &DaemonContext,
    restarted: Restarted,
    busy: u32,
) -> Result<bool, ArchonError> {
    let record = serde_json::to_string(&restarted)?;
    ctx.store.lock().await.kv_set(RESTART_KEY, &record)?;
    let plan = RestartPlan {
        restart_aether: restarted.restart_aether,
    };
    if let Err(error) = ctx.ops.port.spawn_restart(&plan) {
        ctx.store.lock().await.kv_delete(RESTART_KEY)?;
        let why = format!(
            "{} stopped: the restart could not start: {error}",
            restarted.what
        );
        return give_up(ctx, why).await.map(|()| false);
    }
    set_phase(ctx, format!("{}: restarting", restarted.what)).await;
    feed::system(ctx, restart_note(restarted.restart_aether, busy)).await?;
    Ok(true)
}

fn restart_note(restart_aether: bool, busy: u32) -> String {
    let (what, left) = match restart_aether {
        true => (
            "archond and Archon's own aetherd",
            "are stopped with aetherd and retried once it is back",
        ),
        false => ("archond", "keep running in Aether for the next archond"),
    };
    match busy {
        0 => format!("Restarting {what} now."),
        n => format!("Restarting {what} now; the {n} stage agents still running {left}."),
    }
}

/// Lift the hold, end the upgrade and say why.
async fn give_up(ctx: &DaemonContext, why: String) -> Result<(), ArchonError> {
    ctx.ops.hold(ctx, false);
    ctx.ops.job.lock().await.phase = None;
    feed::notify(ctx, NotificationKind::Outcome, why, None, None).await?;
    Ok(())
}

async fn set_phase(ctx: &DaemonContext, phase: String) {
    ctx.ops.job.lock().await.phase = Some(phase);
}

/// An upgrade whose restart the next archond reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Restarted {
    what: String,
    before: Versions,
    restart_aether: bool,
}

impl Restarted {
    fn report(&self, now: &Versions) -> String {
        let change = |program: &str, before: &ProgramVersions, now: &ProgramVersions| {
            let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "unknown".into());
            match (before.running == now.running, &now.running) {
                (true, Some(version)) => format!("{program} {version} (unchanged)"),
                _ => format!(
                    "{program} {} → {}",
                    show(&before.running),
                    show(&now.running)
                ),
            }
        };
        let mut text = format!(
            "Back after {}: {}, {}.",
            self.what,
            change("archon", &self.before.archon, &now.archon),
            change("aether", &self.before.aether, &now.aether)
        );
        if !self.restart_aether && self.before.archon.running != now.archon.running {
            text.push_str(
                " Agents started before keep the old archon tools; \
                 `archon orchestrator restart` gives the orchestrator the new ones.",
            );
        }
        text
    }
}

/// Report the upgrade this archond was restarted for, once Aether's
/// version is known or `LINK_WAIT` passes.
async fn report_restart(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let Some(record) = ctx.store.lock().await.kv_get(RESTART_KEY)? else {
        return Ok(());
    };
    let deadline = Instant::now() + LINK_WAIT;
    while !ctx.aether.is_connected() && Instant::now() < deadline {
        tokio::time::sleep(LINK_POLL).await;
    }
    match serde_json::from_str::<Restarted>(&record) {
        Ok(restarted) => {
            let text = restarted.report(&versions(ctx).await);
            feed::notify(ctx, NotificationKind::Outcome, text, None, None).await?;
        }
        Err(error) => tracing::warn!(%error, "the upgrade record is unreadable; not reported"),
    }
    ctx.store.lock().await.kv_delete(RESTART_KEY)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(archon: &str, aether: Option<&str>) -> Versions {
        let running = |v: Option<&str>| ProgramVersions {
            running: v.map(Into::into),
            ..ProgramVersions::default()
        };
        Versions {
            archon: running(Some(archon)),
            aether: running(aether),
        }
    }

    #[test]
    fn the_report_names_each_change() {
        let restarted = Restarted {
            what: "upgrade archon to main".into(),
            before: versions("1.2.2", Some("1.0.6")),
            restart_aether: true,
        };
        assert_eq!(
            restarted.report(&versions("1.2.3", Some("1.0.6"))),
            "Back after upgrade archon to main: archon 1.2.2 → 1.2.3, aether 1.0.6 (unchanged)."
        );
        assert_eq!(
            restarted.report(&versions("1.2.2", None)),
            "Back after upgrade archon to main: archon 1.2.2 (unchanged), aether 1.0.6 → unknown."
        );
        let kept = Restarted {
            restart_aether: false,
            ..restarted
        };
        assert!(
            kept.report(&versions("1.2.3", Some("1.0.6")))
                .contains("orchestrator restart")
        );
    }

    #[test]
    fn the_helper_keeps_aetherd_unless_told_to_restart_it() {
        let plan = RestartPlan {
            restart_aether: true,
        };
        assert_eq!(plan.helper_args(), ["daemon", "relaunch"]);
        let kept = RestartPlan {
            restart_aether: false,
        };
        assert_eq!(kept.helper_args(), ["daemon", "relaunch", "--keep-aether"]);
    }

    #[test]
    fn a_restart_note_says_what_happens_to_what_still_runs() {
        assert_eq!(restart_note(false, 0), "Restarting archond now.");
        assert!(restart_note(true, 2).contains("2 stage agents still running are stopped"));
        assert!(restart_note(false, 1).contains("keep running in Aether"));
    }
}
