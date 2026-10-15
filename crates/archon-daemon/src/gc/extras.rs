//! What gc collects outside a run's workspace (ARCHITECTURE §13.5): a
//! repository's shared build cache once nobody used it for a while, an
//! item's published artifacts `artifacts.keep_after` it settled, and the
//! experiments directory of an item nothing runs for any more. All are
//! claimed under the run table by renaming them aside, atomically, so a run
//! that starts meanwhile builds into a fresh directory; the slow deletion
//! then happens without any lock.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use archon_core::Timestamp;
use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, StageRunId};
use archon_core::protocol::{GcReport, ItemQuery, Reclaimed};
use archon_core::repo::Repo;
use archon_queue::Store;

use super::Roots;
use super::cache::disk_usage;
use crate::build_cache::USED_MARKER;

/// Directories claimed by a pass that stopped before deleting them start
/// with this; the next pass deletes them.
const CLAIMED_PREFIX: &str = ".gc-";

/// The cutoffs for the shared caches and the artifacts, as of one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extras {
    /// A cache last used before this is deleted.
    pub cache_before: Timestamp,
    /// Artifacts of an item that settled before this are deleted.
    pub artifacts_before: Timestamp,
}

impl Extras {
    /// `older_than` overrides `worktrees.shared_cache_after`, as it does
    /// `gc_after`; `pressure` shortens both to `worktrees.pressure_after`.
    pub fn new(
        config: &Config,
        now: Timestamp,
        older_than: Option<Age>,
        pressure: bool,
    ) -> Result<Self, ArchonError> {
        let mut cache_after = older_than.unwrap_or(config.worktrees.shared_cache_after);
        if pressure
            && older_than.is_none()
            && cache_after.secs() != 0
            && config.worktrees.pressure_after.secs() != 0
        {
            cache_after = cache_after.min(config.worktrees.pressure_after);
        }
        let extras = Self {
            cache_before: super::cutoff(now, cache_after, older_than.is_none())?,
            artifacts_before: super::cutoff(now, config.artifacts.keep_after, true)?,
        };
        assert!(extras.cache_before <= now && extras.artifacts_before <= now);
        Ok(extras)
    }
}

/// A directory renamed aside, waiting to be deleted.
#[derive(Debug)]
pub(super) struct Claimed {
    original: PathBuf,
    aside: PathBuf,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cache,
    Artifacts,
    Experiments,
}

/// Claim what is due under the cache, artifacts and experiments roots, the caller holding the
/// run table (spawns `in_flight`) and the store. What cannot be claimed is noted in `report`.
pub(super) fn claim(
    store: &Store,
    in_flight: &BTreeSet<StageRunId>,
    roots: &Roots,
    extras: &Extras,
    report: &mut GcReport,
) -> Result<Vec<Claimed>, ArchonError> {
    let mut claimed = Vec::new();
    let live = live_repositories(store)?;
    for dir in subdirectories(&roots.cache(), report) {
        let name = file_name(&dir);
        let due = if name.starts_with(CLAIMED_PREFIX) {
            true
        } else if idle_since(&dir).is_some_and(|at| at < SystemTime::from(extras.cache_before)) {
            let live = live.contains(&name);
            if live {
                report
                    .skipped
                    .push((dir.clone(), "a run of the repository is live".into()));
            }
            !live
        } else {
            false
        };
        if due {
            claim_dir(dir, Kind::Cache, &mut claimed, report);
        }
    }
    for dir in subdirectories(&roots.artifacts(), report) {
        let name = file_name(&dir);
        let due = if name.starts_with(CLAIMED_PREFIX) {
            true
        } else {
            expired(store, &name, extras)?
        };
        if due {
            claim_dir(dir, Kind::Artifacts, &mut claimed, report);
        }
    }
    for dir in experiment_dirs(&roots.experiments(), report) {
        let name = file_name(&dir);
        if is_claimed(&name) || orphaned(store, in_flight, &name)? {
            claim_dir(dir, Kind::Experiments, &mut claimed, report);
        }
    }
    Ok(claimed)
}

/// Whether an experiments directory outlived its item: nothing of the item can run any
/// more, or the store does not know it. A name that is no item id is left alone.
fn orphaned(
    store: &Store,
    in_flight: &BTreeSet<StageRunId>,
    name: &str,
) -> Result<bool, ArchonError> {
    let Ok(id) = QueueItemId::parse(name) else {
        return Ok(false);
    };
    match store.item(&id) {
        Ok(item) => assert_eq!(item.id, id, "the store returns the item asked for"),
        Err(ArchonError::NotFound(_)) => return Ok(true),
        Err(error) => return Err(error),
    }
    crate::executor::item_stopped(store, in_flight, &id)
}

/// The real directories under the experiments `root`. A root or an entry
/// that is a symlink is reported and left alone, as is everything under it.
fn experiment_dirs(root: &Path, report: &mut GcReport) -> Vec<PathBuf> {
    match crate::experiments::real_dir(root) {
        Ok(true) => {}
        Ok(false) => return Vec::new(),
        Err(refused) => {
            report
                .failed
                .push((root.to_path_buf(), refused.to_string()));
            return Vec::new();
        }
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            report
                .failed
                .push((root.to_path_buf(), format!("reading it: {e}")));
            return Vec::new();
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report
                    .failed
                    .push((root.to_path_buf(), format!("reading it: {e}")));
                continue;
            }
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
            Ok(kind) if kind.is_symlink() => report
                .skipped
                .push((entry.path(), "a symlink, which gc does not follow".into())),
            Ok(_) => {}
            Err(e) => report
                .failed
                .push((entry.path(), format!("inspecting it: {e}"))),
        }
    }
    assert!(
        dirs.iter().all(|dir| dir.parent() == Some(root)),
        "only entries of the root itself are claimed"
    );
    dirs.sort();
    dirs
}

/// Whether `name` is one a pass or [`set_aside`] renamed a directory to.
pub(crate) fn is_claimed(name: &str) -> bool {
    name.starts_with(CLAIMED_PREFIX)
}

/// The names of the registered repositories with an item not yet settled.
fn live_repositories(store: &Store) -> Result<HashSet<String>, ArchonError> {
    let names: std::collections::HashMap<_, _> = store
        .repos()?
        .into_iter()
        .map(|Repo { id, name, .. }| (id, name))
        .collect();
    let query = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    let mut live = HashSet::new();
    for item in store.items(&query)? {
        if item.state.is_terminal() {
            continue;
        }
        if let Some(name) = item.repo.as_ref().and_then(|id| names.get(id)) {
            live.insert(name.clone());
        }
    }
    Ok(live)
}

/// Whether the item's artifacts are due: it settled before the cutoff. An
/// unknown item's directory is the operator's to judge.
fn expired(store: &Store, name: &str, extras: &Extras) -> Result<bool, ArchonError> {
    let Ok(id) = QueueItemId::parse(name) else {
        return Ok(false);
    };
    match store.item(&id) {
        Ok(item) => Ok(item.state.is_terminal() && item.updated_at < extras.artifacts_before),
        Err(ArchonError::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// The last time the cache was used: its marker's, else the directory's.
fn idle_since(dir: &Path) -> Option<SystemTime> {
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    modified(&dir.join(USED_MARKER)).or_else(|| modified(dir))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The real directories directly under `root`; none when it does not exist.
fn subdirectories(root: &Path, report: &mut GcReport) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            report
                .failed
                .push((root.to_path_buf(), format!("reading it: {e}")));
            return Vec::new();
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_type().is_ok_and(|t| t.is_dir()) => dirs.push(entry.path()),
            Ok(_) => {}
            Err(e) => report
                .failed
                .push((root.to_path_buf(), format!("reading it: {e}"))),
        }
    }
    dirs.sort();
    dirs
}

fn claim_dir(dir: PathBuf, kind: Kind, claimed: &mut Vec<Claimed>, report: &mut GcReport) {
    if file_name(&dir).starts_with(CLAIMED_PREFIX) {
        claimed.push(Claimed {
            original: dir.clone(),
            aside: dir,
            kind,
        });
        return;
    }
    match set_aside(&dir) {
        Ok(aside) => claimed.push(Claimed {
            original: dir,
            aside,
            kind,
        }),
        Err(e) => report.failed.push((dir, format!("setting it aside: {e}"))),
    }
}

/// Rename `dir` beside itself under a claimed name, which the next pass
/// deletes should this one stop first. Returns the new path.
pub(crate) fn set_aside(dir: &Path) -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = file_name(dir);
    assert!(
        !name.is_empty() && !name.starts_with(CLAIMED_PREFIX),
        "only a named, unclaimed directory is set aside"
    );
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let aside = dir.with_file_name(format!("{CLAIMED_PREFIX}{}-{name}-{n}", std::process::id()));
    std::fs::rename(dir, &aside)?;
    Ok(aside)
}

/// Delete what [`claim`] set aside, counting the space each frees. Blocking.
pub(super) fn delete(claimed: Vec<Claimed>) -> GcReport {
    let mut report = GcReport::default();
    for Claimed {
        original,
        aside,
        kind,
    } in claimed
    {
        let freed = disk_usage(&aside).and_then(|bytes| {
            std::fs::remove_dir_all(&aside)
                .map_err(|e| ArchonError::io(format!("removing {}", aside.display()), e))?;
            Ok(bytes)
        });
        match freed {
            Ok(bytes) => {
                let freed = Reclaimed {
                    path: original,
                    bytes,
                };
                match kind {
                    Kind::Cache => report.caches.push(freed),
                    Kind::Artifacts => report.artifacts.push(freed),
                    Kind::Experiments => report.removed.push(freed.path),
                }
                report.bytes_freed = report.bytes_freed.saturating_add(bytes);
            }
            Err(error) => report.failed.push((original, error.to_string())),
        }
    }
    report
}
