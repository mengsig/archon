//! Collecting what finished runs leave on disk (ARCHITECTURE §13.2): the
//! pass archond runs periodically and before a disk-held dispatch, which
//! `archon gc` asks for, and which `archon gc --offline` runs itself.
//!
//! A kept worktree's build caches go `worktrees.cache_after` after its runs
//! finished, its source staying for inspection; the worktree itself goes
//! after `gc_after` unless it holds uncommitted changes. Branches are kept.
//! Only paths inside Archon's worktree root that git lists as worktrees of
//! the item's repository, scratch directories under the data dir, and what
//! a team run or a fresh retry set aside, are touched, and only for items
//! still settled. A read-only run's snapshot left behind goes whole, dirty
//! or locked.
//!
//! Beside the workspaces, a pass deletes a repository's shared build cache
//! once nothing used it for `worktrees.shared_cache_after` and no item of the
//! repository is live, an item's published artifacts `artifacts.keep_after`
//! it settled, and the experiments directory of an item nothing runs for any
//! more (`extras`).
//! While dispatch is held for disk space, the ages of worktrees and caches
//! drop to `worktrees.pressure_after` if that is shorter.
//! A pass that freed anything leaves a note in the feed and in `archon status`.

mod aether;
pub(crate) mod cache;
mod extras;
mod mark;
mod plan;
mod preview;
mod snapshot;
mod sweep;
mod team;

pub(crate) use team::set_aside_of;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::Timestamp;
use archon_core::age::Age;
use archon_core::config::{Config, WorktreeConfig};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{GcReport, GcStatus, Reclaimed, gib};
use archon_queue::Store;
use tokio::sync::MutexGuard;

use crate::compose::Volumes;
use crate::context::{DaemonContext, DaemonPaths};
use crate::feed;
use crate::worktree::users::{self, LiveWorkspaces};

pub use extras::Extras;
pub(crate) use extras::{is_claimed, set_aside};
pub use mark::{Collecting, Mark};

/// The kv key of the last pass that freed anything, as a [`GcStatus`].
const LAST_PASS_KEY: &str = "gc_last_pass";

/// How often archond's own pass runs; the first runs at startup.
const INTERVAL: Duration = Duration::from_secs(30 * 60);

/// While dispatch is held for disk space, a pass runs before a dispatch at
/// most this often: one that freed too little is not repeated every tick.
const DISK_PASS_GAP: Duration = Duration::from_secs(5 * 60);

/// How long a pass before a held dispatch may hold the dispatch pass up.
/// Past it the pass stops; the item sweep under way finishes on its own,
/// its mark keeping the item from starting, and a later pass goes on.
const DISK_PASS_BUDGET: Duration = Duration::from_secs(30);

/// What a pass collects, as of one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// A worktree whose runs all finished before this is removed.
    pub remove_before: Timestamp,
    /// A worktree whose runs all finished before this loses its caches.
    pub reclaim_before: Timestamp,
    /// Remove worktrees with uncommitted changes too.
    pub force: bool,
    /// `worktrees.cache_dirs`: empty reclaims the known build-output roots.
    pub cache_dirs: Vec<PathBuf>,
    /// A shipped, succeeded run's worktree archond should have removed at once
    /// (`keep_on_success` off), and did not, goes with its caches.
    pub owed_cleanup: bool,
}

impl Policy {
    /// The policy of `config` as of `now`; `older_than` overrides `gc_after`.
    pub fn new(
        config: &WorktreeConfig,
        now: Timestamp,
        older_than: Option<Age>,
        force: bool,
    ) -> Result<Self, ArchonError> {
        let explicit = older_than.is_some();
        let older_than = older_than.unwrap_or(config.gc_after);
        let policy = Self {
            remove_before: cutoff(now, older_than, !explicit)?,
            reclaim_before: cutoff(now, config.cache_after, true)?,
            force,
            cache_dirs: config.cache_dirs.clone(),
            owed_cleanup: !config.keep_on_success && config.gc_after.secs() != 0,
        };
        assert!(policy.remove_before <= now, "ages are not negative");
        assert!(policy.reclaim_before <= now, "ages are not negative");
        Ok(policy)
    }

    /// The policy while dispatch is held for disk space: worktrees of
    /// finished runs go after `worktrees.pressure_after` if that is shorter
    /// than their age so far.
    pub fn under_pressure(
        mut self,
        config: &WorktreeConfig,
        now: Timestamp,
    ) -> Result<Self, ArchonError> {
        let sooner = cutoff(now, config.pressure_after, false)?;
        if config.gc_after.secs() != 0 && config.pressure_after.secs() != 0 {
            self.remove_before = self.remove_before.max(sooner);
        }
        assert!(self.remove_before <= now, "ages are not negative");
        Ok(self)
    }

    /// Items settled before this may hold something to collect.
    fn settled_before(&self) -> Timestamp {
        self.remove_before.max(self.reclaim_before)
    }
}

/// A configured zero disables expiry; explicit CLI zero still means now.
fn cutoff(now: Timestamp, age: Age, zero_disables: bool) -> Result<Timestamp, ArchonError> {
    if zero_disables && age.secs() == 0 {
        return Ok(Timestamp::MIN_UTC);
    }
    now.checked_sub_signed(age.chrono()?).ok_or_else(|| {
        ArchonError::Invalid(format!("the age {age} is too large for this timestamp"))
    })
}

/// Where archond creates run workspaces, with symlinks resolved as git
/// resolves them for the worktrees it lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub worktrees: PathBuf,
    pub scratch: PathBuf,
    /// Where the shadows that check a worktree is clean live.
    pub data_dir: PathBuf,
}

impl Roots {
    pub fn cache(&self) -> PathBuf {
        self.data_dir.join(crate::context::CACHE_DIR)
    }

    pub fn artifacts(&self) -> PathBuf {
        self.data_dir.join(crate::context::ARTIFACTS_DIR)
    }

    pub fn experiments(&self) -> PathBuf {
        self.data_dir.join(crate::context::EXPERIMENTS_DIR)
    }

    pub fn resolve(paths: &DaemonPaths) -> Result<Self, ArchonError> {
        let roots = Self {
            worktrees: canonical_root(&paths.worktrees)?,
            scratch: canonical_root(&paths.scratch)?,
            data_dir: canonical_root(&paths.data_dir)?,
        };
        assert!(roots.worktrees.is_absolute() && roots.scratch.is_absolute());
        assert!(
            roots.data_dir.is_absolute(),
            "the data directory is absolute"
        );
        Ok(roots)
    }
}

/// A root that does not exist holds no workspace, so its absolute form is
/// as good as a canonical one there.
fn canonical_root(path: &Path) -> Result<PathBuf, ArchonError> {
    let resolved = match path.canonicalize() {
        Err(e) if e.kind() == ErrorKind::NotFound => std::path::absolute(path),
        other => other,
    };
    let resolved =
        resolved.map_err(|e| ArchonError::io(format!("resolving {}", path.display()), e))?;
    assert!(resolved.is_absolute(), "roots are absolute");
    Ok(resolved)
}

/// archond's own pass, at startup and every 30 minutes, started only with
/// `worktrees.auto_gc` on. Only a store failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    assert!(ctx.config.worktrees.auto_gc, "started only when enabled");
    loop {
        match collect(&ctx, None, false).await {
            Ok(report) => log(&report),
            Err(error) if error.is_store() => return Err(error),
            Err(error) => tracing::warn!(%error, "worktree gc failed; the next pass retries"),
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// While dispatch is held for disk space, a pass before dispatching, so
/// the space finished runs hold comes back, within [`DISK_PASS_BUDGET`].
/// Returns whether it may have freed any.
pub(crate) async fn before_dispatch(ctx: &DaemonContext) -> Result<bool, ArchonError> {
    if !ctx.config.worktrees.auto_gc || !ctx.disk.holding() {
        return Ok(false);
    }
    // A pass already running frees what it can without holding dispatch up.
    let recent = match ctx.gc.try_lock() {
        Ok(last_pass) => last_pass.is_some_and(|at| at.elapsed() < DISK_PASS_GAP),
        Err(_) => true,
    };
    if recent {
        return Ok(false);
    }
    let pass = tokio::time::timeout(DISK_PASS_BUDGET, collect(ctx, None, false));
    let Ok(outcome) = pass.await else {
        tracing::warn!(
            budget = ?DISK_PASS_BUDGET,
            "worktree gc before a held dispatch ran over its budget; a later pass goes on"
        );
        // The stopped pass recorded nothing; keep the gap all the same. A
        // pass that took the lock meanwhile records itself.
        if let Ok(mut last_pass) = ctx.gc.try_lock() {
            *last_pass = Some(Instant::now());
        }
        // What it freed before it stopped is unknown, so read the disk again.
        return Ok(true);
    };
    match outcome {
        Ok(report) => {
            log(&report);
            Ok(report.freed_any())
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(%error, "worktree gc before a held dispatch failed");
            Ok(false)
        }
    }
}

/// One pass, serialized with any other: [`plan`], then [`collect_planned`].
pub async fn collect(
    ctx: &DaemonContext,
    older_than: Option<Age>,
    force: bool,
) -> Result<GcReport, ArchonError> {
    let pass = plan(ctx, older_than, force).await?;
    collect_planned(ctx, pass).await
}

/// What a pass would delete from run worktrees, deleting nothing.
pub async fn preview(
    ctx: &DaemonContext,
    older_than: Option<Age>,
    force: bool,
) -> Result<GcReport, ArchonError> {
    let Pass {
        policy,
        roots,
        pressure,
        mut plans,
        ..
    } = plan(ctx, older_than, force).await?;
    let store = ctx.store.lock().await;
    let in_use = leave_out_live_places(&store, &ctx.paths.worktrees, &mut plans)?;
    drop(store);
    let mut report = blocking(move || preview::preview(&plans, &roots, &policy)).await?;
    report.skipped.extend(in_use);
    report.pressure = pressure;
    Ok(report)
}

/// Leave the places live items use out of `plans`; returns them with why.
fn leave_out_live_places(
    store: &Store,
    worktrees: &Path,
    plans: &mut [plan::ItemPlan],
) -> Result<Vec<(PathBuf, String)>, ArchonError> {
    let live = LiveWorkspaces::read(store, worktrees)?;
    let in_use: Vec<_> = plans
        .iter_mut()
        .flat_map(|plan| plan.found.keep_in_use(&live))
        .collect();
    debug_assert!(in_use.iter().all(|(path, _)| live.user_of(path).is_some()));
    Ok(in_use)
}

/// A planned pass; it holds off any other until it is collected or dropped.
#[derive(Debug)]
pub struct Pass<'a> {
    last_pass: MutexGuard<'a, Option<Instant>>,
    policy: Arc<Policy>,
    roots: Arc<Roots>,
    extras: Extras,
    pressure: bool,
    plans: Vec<plan::ItemPlan>,
}

impl Pass<'_> {
    /// How many items hold something to collect.
    pub fn items(&self) -> usize {
        self.plans.len()
    }
}

/// What a pass will collect, from the store as it is now.
pub async fn plan(
    ctx: &DaemonContext,
    older_than: Option<Age>,
    force: bool,
) -> Result<Pass<'_>, ArchonError> {
    let last_pass = ctx.gc.lock().await;
    let now = archon_core::now();
    let pressure = ctx.disk.holding();
    let mut policy = Policy::new(&ctx.config.worktrees, now, older_than, force)?;
    if pressure && older_than.is_none() {
        policy = policy.under_pressure(&ctx.config.worktrees, now)?;
    }
    let policy = Arc::new(policy);
    let extras = Extras::new(&ctx.config, now, older_than, pressure)?;
    let roots = Arc::new(Roots::resolve(&ctx.paths)?);
    let settled = plan::settled(&*ctx.store.lock().await, policy.settled_before())?;
    let (r, p) = (roots.clone(), policy.clone());
    let plans = blocking(move || plan::plan_all(&settled, &r, &p)).await?;
    Ok(Pass {
        last_pass,
        policy,
        roots,
        extras,
        pressure,
        plans,
    })
}

/// Collect each planned item that is still as settled as it was planned:
/// one retried meanwhile may be using its worktree again, and so may a
/// refresh or restack fix of the item that owns it. Each item, and that
/// owner, is marked (see [`Collecting`]) holding the run table, which
/// dispatch and a retry take too, the places live items use are left out,
/// then the rest is collected off the workers without the table. Its marks
/// poke dispatch as they go, for an item they held back.
pub async fn collect_planned(ctx: &DaemonContext, pass: Pass<'_>) -> Result<GcReport, ArchonError> {
    let Pass {
        mut last_pass,
        policy,
        roots,
        extras,
        pressure,
        plans,
    } = pass;
    let mut report = GcReport::default();
    let mut repos = BTreeSet::new();
    for mut item in plans {
        let marks = match mark_if_settled(ctx, &mut item).await? {
            Marking::Marked { marks, in_use } => {
                report.skipped.extend(in_use);
                marks
            }
            Marking::Unsettled => continue,
            Marking::Held => {
                report.absorb(held_by_a_sweep(&item));
                continue;
            }
        };
        let (r, p) = (roots.clone(), policy.clone());
        let id = item.item.clone();
        let swept = blocking(move || {
            let _marks = marks;
            sweep::sweep(item.found, &r, &p)
        });
        let swept = swept.await?;
        for removed in &swept.report.removed {
            let when = "when gc removed it";
            crate::compose::tear_down(ctx, &id, removed, Volumes::Remove, when).await?;
        }
        repos.extend(swept.repos);
        report.absorb(swept.report);
    }
    report.absorb(after_removals(ctx, &repos).await?);
    report.absorb(collect_extras(ctx, &roots, &extras).await?);
    report.absorb(collect_reports(ctx).await?);
    report.pressure = pressure;
    *last_pass = Some(Instant::now());
    ctx.storage.forget();
    announce(ctx, &report).await?;
    Ok(report)
}

/// When the pass removed worktrees of `repos`: have Aether delete the
/// workspaces it delegated, then prune git's entries for every gone directory.
async fn after_removals(
    ctx: &DaemonContext,
    repos: &BTreeSet<PathBuf>,
) -> Result<GcReport, ArchonError> {
    if repos.is_empty() {
        return Ok(GcReport::default());
    }
    let aether_gc = aether::storage_gc(ctx).await;
    let repos = repos.clone();
    let mut report = blocking(move || aether::prune(&repos)).await?;
    report.aether_gc = Some(aether_gc);
    Ok(report)
}

/// Claim the idle shared caches and expired artifacts holding the run table,
/// so no item starts using one meanwhile, then delete them without it.
async fn collect_extras(
    ctx: &DaemonContext,
    roots: &Arc<Roots>,
    extras: &Extras,
) -> Result<GcReport, ArchonError> {
    let mut report = GcReport::default();
    let claimed = {
        let table = ctx.runs.lock().await;
        let store = ctx.store.lock().await;
        extras::claim(&store, &table.in_flight, roots, extras, &mut report)?
    };
    report.absorb(blocking(move || extras::delete(claimed)).await?);
    Ok(report)
}

async fn collect_reports(ctx: &DaemonContext) -> Result<GcReport, ArchonError> {
    let days = ctx.config.storage.retention_days;
    if days == 0 {
        return Ok(GcReport::default());
    }
    // Publishing and reopening take this lock before the store too. Keeping
    // the store held makes the terminal-item and gate checks atomic with expiry.
    let _files = ctx.report_files.lock().await;
    let cutoff = archon_core::now() - chrono::Duration::days(i64::from(days));
    crate::reports::expire_files(&mut *ctx.store.lock().await, &ctx.paths.reports, cutoff)
}

/// Tell the operator what a pass that freed something did: a feed note, and
/// the record `archon status` shows. A note that cannot be posted is logged;
/// only a store failure fails the pass, whose work is done.
async fn announce(ctx: &DaemonContext, report: &GcReport) -> Result<(), ArchonError> {
    if !report.freed_any() {
        return Ok(());
    }
    let status = GcStatus {
        at: archon_core::now(),
        bytes_freed: report.bytes_freed,
        pressure: report.pressure,
        headline: report.headline(),
    };
    record(ctx, status).await
}

/// Tell the operator an item's build directory went when the item ended.
pub async fn announce_item_release(
    ctx: &DaemonContext,
    item: &QueueItemId,
    freed: &Reclaimed,
) -> Result<(), ArchonError> {
    ctx.storage.forget();
    if freed.bytes == 0 {
        return Ok(());
    }
    let status = GcStatus {
        at: archon_core::now(),
        bytes_freed: freed.bytes,
        pressure: false,
        headline: format!(
            "Freed {} of build output: item {item} ended and its cargo target directory went.",
            gib(freed.bytes)
        ),
    };
    record(ctx, status).await
}

async fn record(ctx: &DaemonContext, status: GcStatus) -> Result<(), ArchonError> {
    let recorded = ctx
        .store
        .lock()
        .await
        .kv_set(LAST_PASS_KEY, &serde_json::to_string(&status)?);
    let posted = match recorded {
        Ok(()) => feed::system(ctx, status.headline).await.map(drop),
        Err(error) => Err(error),
    };
    match posted {
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(%error, "could not tell the operator what worktree gc freed");
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

/// The last pass that freed anything, for `archon status`.
pub(crate) fn last_pass(store: &Store) -> Result<Option<GcStatus>, ArchonError> {
    store
        .kv_get(LAST_PASS_KEY)?
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|e| ArchonError::Corrupt(format!("kv {LAST_PASS_KEY}: {e}")))
        })
        .transpose()
}

/// Whether a pass may collect a planned item now.
enum Marking {
    /// The marks of the item and of its worktree's owner, with the places
    /// left out of its plan because live items use them.
    Marked {
        marks: Vec<Mark>,
        in_use: Vec<(PathBuf, String)>,
    },
    /// Retried or changed since it was planned.
    Unsettled,
    /// An earlier pass was stopped mid-sweep; its sweep collects the item.
    Held,
}

/// The marks of the item and of the owner of its worktree, taken holding
/// the run table, while the store still shows it as settled as `item`
/// planned it and no sweep holds either mark. Dispatch holds back any item
/// of that worktree while they last; what a live item uses already leaves
/// `item`'s plan here, under the same table.
async fn mark_if_settled(
    ctx: &DaemonContext,
    item: &mut plan::ItemPlan,
) -> Result<Marking, ArchonError> {
    let _table = ctx.runs.lock().await;
    let store = ctx.store.lock().await;
    if !plan::still_settled(&store, item)? {
        return Ok(Marking::Unsettled);
    }
    let owner = users::owner(&store, &item.item)?;
    let live = LiveWorkspaces::read(&store, &ctx.paths.worktrees)?;
    drop(store);
    let mut marks = Vec::with_capacity(2);
    for id in [Some(&item.item), (owner != item.item).then_some(&owner)] {
        match id.map(|id| ctx.collecting.mark(id.clone())) {
            Some(Some(mark)) => marks.push(mark),
            Some(None) => return Ok(Marking::Held),
            None => {}
        }
    }
    assert!(
        ctx.collecting.contains(&owner),
        "the worktree's owner is marked"
    );
    let in_use = item.found.keep_in_use(&live);
    Ok(Marking::Marked { marks, in_use })
}

/// A report that leaves every place `item` planned to the sweep that still
/// holds its mark, so `archon gc` says why it did nothing there.
fn held_by_a_sweep(item: &plan::ItemPlan) -> GcReport {
    let why = format!(
        "a stopped gc pass is still collecting item {}; a later pass goes on",
        item.item
    );
    let skipped: Vec<_> = item.found.paths().map(|path| (path, why.clone())).collect();
    assert!(
        !skipped.is_empty(),
        "only items with something to collect are planned"
    );
    GcReport {
        skipped,
        ..GcReport::default()
    }
}

/// One read-only pass without archond, for a disk too full to start it, holding the daemon lock
/// and the data dir's lock shared so none runs meanwhile. Fails while an archond holds either.
pub fn collect_offline(
    config: &Config,
    paths: &DaemonPaths,
    older_than: Option<Age>,
    force: bool,
) -> Result<GcReport, ArchonError> {
    let _data = archon_queue::DataLock::share(&paths.data_dir)?;
    let _lock = crate::server::lock_daemon(&paths.socket)?;
    let store = Store::open_read_only(&paths.store)?;
    let (policy, pressure) = offline_policy(config, paths, older_than, force)?;
    let extras = Extras::new(config, archon_core::now(), older_than, pressure)?;
    let roots = Roots::resolve(paths)?;
    let mut report = collect_locked(&store, (&roots, &paths.worktrees), &policy, &extras)?;
    report.pressure = pressure;
    Ok(report)
}

/// [`preview`] without archond, under the same locks as [`collect_offline`].
pub fn preview_offline(
    config: &Config,
    paths: &DaemonPaths,
    older_than: Option<Age>,
    force: bool,
) -> Result<GcReport, ArchonError> {
    let _data = archon_queue::DataLock::share(&paths.data_dir)?;
    let _lock = crate::server::lock_daemon(&paths.socket)?;
    let store = Store::open_read_only(&paths.store)?;
    let (policy, pressure) = offline_policy(config, paths, older_than, force)?;
    let roots = Roots::resolve(paths)?;
    let settled = plan::settled(&store, policy.settled_before())?;
    let mut plans = plan::plan_all(&settled, &roots, &policy);
    let in_use = leave_out_live_places(&store, &paths.worktrees, &mut plans)?;
    let mut report = preview::preview(&plans, &roots, &policy);
    report.skipped.extend(in_use);
    report.pressure = pressure;
    Ok(report)
}

/// The policy of an offline pass and whether the disk is under pressure.
fn offline_policy(
    config: &Config,
    paths: &DaemonPaths,
    older_than: Option<Age>,
    force: bool,
) -> Result<(Policy, bool), ArchonError> {
    let now = archon_core::now();
    let pressure = below_minimum_free(config, paths);
    let mut policy = Policy::new(&config.worktrees, now, older_than, force)?;
    if pressure && older_than.is_none() {
        policy = policy.under_pressure(&config.worktrees, now)?;
    }
    Ok((policy, pressure))
}

/// Whether the data dir has less free than `limits.min_free_gb`, the level
/// at which archond would hold dispatch. An unreadable disk is not pressure.
fn below_minimum_free(config: &Config, paths: &DaemonPaths) -> bool {
    let min = config.limits.min_free_bytes();
    min > 0 && crate::disk::free_bytes(&paths.data_dir).is_ok_and(|free| free < min)
}

/// A pass over `store` while the daemon lock is held.
fn collect_locked(
    store: &Store,
    (roots, worktrees): (&Roots, &Path),
    policy: &Policy,
    extras: &Extras,
) -> Result<GcReport, ArchonError> {
    let settled = plan::settled(store, policy.settled_before())?;
    let mut report = GcReport::default();
    let mut plans = plan::plan_all(&settled, roots, policy);
    let in_use = leave_out_live_places(store, worktrees, &mut plans)?;
    report.skipped.extend(in_use);
    let mut repos = BTreeSet::new();
    for item in plans {
        if plan::still_settled(store, &item)? {
            let swept = sweep::sweep(item.found, roots, policy);
            repos.extend(swept.repos);
            report.absorb(swept.report);
        }
    }
    // No archond runs to ask Aether for its storage cleanup.
    report.absorb(aether::prune(&repos));
    // No archond runs, so no spawn is in flight.
    let in_flight = std::collections::BTreeSet::new();
    let claimed = extras::claim(store, &in_flight, roots, extras, &mut report)?;
    report.absorb(extras::delete(claimed));
    Ok(report)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ArchonError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ArchonError::io("the worktree gc task", std::io::Error::other(e)))
}

fn log(report: &GcReport) {
    for (path, why) in &report.failed {
        tracing::warn!(path = %path.display(), %why, "worktree gc could not collect");
    }
    for (path, why) in &report.skipped {
        tracing::debug!(path = %path.display(), %why, "worktree gc left it");
    }
    if report.freed_any() {
        tracing::info!(
            removed = report.removed.len(),
            reclaimed = report.reclaimed.len(),
            freed = %archon_core::protocol::gib(report.bytes_freed),
            "worktree gc freed disk space"
        );
    }
}
