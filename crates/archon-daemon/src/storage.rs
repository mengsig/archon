//! Read-only filesystem accounting, cached briefly for frequent status polls.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::protocol::StorageUsage;
use tokio::sync::watch;

use crate::context::DaemonPaths;
use crate::gc::cache::disk_usage;

/// How long a measurement stands before status measures again.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// The last measurement and the one running. A measurement runs in its own
/// task, so a status past its deadline shows the last one instead.
#[derive(Debug, Default)]
pub struct StorageCache {
    state: Arc<Mutex<CacheState>>,
}

#[derive(Debug, Default)]
struct CacheState {
    reading: Option<(Instant, Vec<StorageUsage>)>,
    /// Why the latest measurement ended without a reading.
    failure: Option<String>,
    /// Set while a measurement runs; it turns `true` when it ends.
    measuring: Option<watch::Receiver<bool>>,
    /// Bumped by `forget`, so a measurement begun before it is dropped.
    /// Only the current generation's measurement touches the state.
    generation: u64,
}

enum Cached {
    Fresh(Vec<StorageUsage>),
    Measuring(watch::Receiver<bool>),
}

impl StorageCache {
    /// Drop the reading (gc freed space): the next status measures again,
    /// without waiting for a measurement begun before.
    pub fn forget(&self) {
        let mut state = lock(&self.state);
        state.reading = None;
        state.measuring = None;
        state.generation += 1;
    }

    /// The usage under `paths`, measured again once a minute old; past
    /// `deadline` the last reading stands, and none before the first.
    pub async fn usage(
        &self,
        paths: &DaemonPaths,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<StorageUsage>, ArchonError> {
        let paths = paths.clone();
        self.usage_with(deadline, move || measure(&paths)).await
    }

    async fn usage_with(
        &self,
        deadline: tokio::time::Instant,
        measure: impl FnOnce() -> Vec<StorageUsage> + Send + 'static,
    ) -> Result<Vec<StorageUsage>, ArchonError> {
        let mut done = match self.fresh_or_measure(measure) {
            Cached::Fresh(usage) => return Ok(usage),
            Cached::Measuring(done) => done,
        };
        // A measurement that panicked closed its channel: it is over too.
        let ended = tokio::time::timeout_at(deadline, done.wait_for(|ended| *ended)).await;
        if ended.is_err() {
            tracing::debug!("status answers before the storage measurement ends");
        }
        let state = lock(&self.state);
        if let Some(why) = &state.failure {
            let error = std::io::Error::other(why.clone());
            return Err(ArchonError::io("measuring storage", error));
        }
        let usage = state.reading.as_ref().map(|(_, usage)| usage.clone());
        let usage = usage.unwrap_or_default();
        assert!(
            usage.iter().all(|u| u.bytes.is_some() != u.error.is_some()),
            "each kind has a size or says why not"
        );
        Ok(usage)
    }

    fn fresh_or_measure(
        &self,
        measure: impl FnOnce() -> Vec<StorageUsage> + Send + 'static,
    ) -> Cached {
        let mut state = lock(&self.state);
        if let Some((at, usage)) = &state.reading
            && at.elapsed() < FRESH_FOR
        {
            return Cached::Fresh(usage.clone());
        }
        // A measurement that panicked dropped its sender; it no longer runs.
        if let Some(running) = &state.measuring
            && running.has_changed().is_ok()
        {
            assert!(!*running.borrow(), "an ended measurement clears itself");
            return Cached::Measuring(running.clone());
        }
        let (ended, done) = watch::channel(false);
        state.measuring = Some(done.clone());
        let (cache, generation) = (self.state.clone(), state.generation);
        tokio::spawn(async move {
            let measured = tokio::task::spawn_blocking(measure).await;
            let mut state = lock(&cache);
            // Measured before gc freed space: a newer measurement replaces it.
            if state.generation == generation {
                match measured {
                    Ok(usage) => {
                        state.reading = Some((Instant::now(), usage));
                        state.failure = None;
                    }
                    Err(error) => state.failure = Some(error.to_string()),
                }
                state.measuring = None;
            }
            ended.send_replace(true);
        });
        Cached::Measuring(done)
    }
}

fn lock(state: &Mutex<CacheState>) -> MutexGuard<'_, CacheState> {
    // Only plain assignments run under the lock; a poisoned one is whole.
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn measure(paths: &DaemonPaths) -> Vec<StorageUsage> {
    let roots = [
        ("worktrees", paths.worktrees.clone()),
        ("scratch", paths.scratch.clone()),
        ("experiments", paths.experiments()),
        ("build_cache", paths.cache()),
        ("artifacts", paths.artifacts()),
        ("reports_lavish", paths.reports.clone()),
        ("logs", paths.logs.clone()),
        ("store", paths.store.clone()),
        ("orchestrator_mirrors", paths.orchestrator_workspace.clone()),
        ("aether", paths.data_dir.join("aether")),
        ("mirror_git", paths.data_dir.join("mirror-git")),
        ("pipeline_snapshots", paths.data_dir.join("pipeline-runs")),
        (
            "initiative_bodies",
            paths.data_dir.join("initiative-bodies"),
        ),
        ("chat_history", paths.data_dir.join("chat_history")),
        ("mirror_hooks", paths.data_dir.join("mirror-hooks")),
    ];
    let mut usage: Vec<_> = roots
        .into_iter()
        .map(|(kind, path)| reading(kind, path))
        .collect();
    usage.push(logs(paths));
    // SQLite sidecars belong to the store, and may disappear at checkpoints.
    let store = usage
        .iter_mut()
        .find(|u| u.kind == "store")
        .expect("store reading");
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = paths.store.as_os_str().to_os_string();
        sidecar.push(suffix);
        add(store, &PathBuf::from(sidecar));
    }
    usage
}

fn reading(kind: &str, path: PathBuf) -> StorageUsage {
    let (bytes, error) = match disk_usage(&path) {
        Ok(bytes) => (Some(bytes), None),
        Err(error) => (None, Some(error.to_string())),
    };
    StorageUsage {
        kind: kind.into(),
        path,
        bytes,
        error,
    }
}

fn add(usage: &mut StorageUsage, path: &Path) {
    match disk_usage(path) {
        Ok(bytes) => usage.bytes = usage.bytes.map(|total| total.saturating_add(bytes)),
        Err(error) => {
            usage.bytes = None;
            usage.error = Some(error.to_string());
        }
    }
}

fn logs(paths: &DaemonPaths) -> StorageUsage {
    let path = archon_core::paths::startup_log_for(&paths.socket);
    let mut usage = reading("daemon_log", path.clone());
    let Some(parent) = path.parent() else {
        return usage;
    };
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return usage,
        Err(error) => {
            usage.bytes = None;
            usage.error = Some(error.to_string());
            return usage;
        }
    };
    let prefix = format!(
        "{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    for entry in entries {
        match entry {
            Ok(entry)
                if entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix(&prefix)
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                    }) =>
            {
                add(&mut usage, &entry.path())
            }
            Ok(_) => {}
            Err(error) => {
                usage.bytes = None;
                usage.error = Some(error.to_string());
            }
        }
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> DaemonPaths {
        DaemonPaths {
            socket: root.join("run/archond.sock"),
            data_dir: root.join("data"),
            store: root.join("data/archon.db"),
            worktrees: root.join("trees"),
            orchestrator_workspace: root.join("data/orchestrator"),
            scratch: root.join("data/scratch"),
            reports: root.join("data/reports"),
            logs: root.join("data/logs"),
            archon_bin: root.join("archon"),
        }
    }

    #[test]
    fn accounting_includes_sidecars_and_numeric_log_archives_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
        for name in ["archon.db", "archon.db-wal", "archon.db-shm"] {
            std::fs::write(paths.data_dir.join(name), vec![1; 8192]).unwrap();
        }
        let active = archon_core::paths::startup_log_for(&paths.socket);
        std::fs::write(&active, vec![1; 8192]).unwrap();
        let archive = active.with_extension("log.1");
        std::fs::write(&archive, vec![1; 8192]).unwrap();
        std::fs::write(active.with_extension("log.lock"), vec![1; 8192]).unwrap();
        std::fs::create_dir_all(&paths.worktrees).unwrap();
        std::os::unix::fs::symlink(&paths.data_dir, paths.worktrees.join("alias")).unwrap();
        let usage = measure(&paths);
        let bytes = |kind| {
            usage
                .iter()
                .find(|usage| usage.kind == kind)
                .unwrap()
                .bytes
                .unwrap()
        };
        let store_bytes: u64 = ["archon.db", "archon.db-wal", "archon.db-shm"]
            .into_iter()
            .map(|name| disk_usage(&paths.data_dir.join(name)).unwrap())
            .sum();
        assert_eq!(bytes("store"), store_bytes);
        assert_eq!(
            bytes("daemon_log"),
            disk_usage(&active).unwrap() + disk_usage(&archive).unwrap()
        );
        assert!(
            bytes("worktrees") < store_bytes,
            "directory links are not traversed"
        );
        assert_eq!(bytes("artifacts"), 0, "absent directories hold no data");
    }

    fn sized(bytes: u64) -> Vec<StorageUsage> {
        let path = PathBuf::from("/data/archon.db");
        let usage = StorageUsage {
            kind: "store".into(),
            path,
            bytes: Some(bytes),
            error: None,
        };
        vec![usage]
    }

    fn sizes(usage: &[StorageUsage]) -> Vec<Option<u64>> {
        usage.iter().map(|u| u.bytes).collect()
    }

    #[tokio::test]
    async fn a_slow_measurement_leaves_status_the_last_reading_and_is_shared() {
        let cache = StorageCache::default();
        let soon = || tokio::time::Instant::now() + Duration::from_millis(200);
        let first = cache.usage_with(soon(), || sized(1)).await.unwrap();
        assert_eq!(sizes(&first), [Some(1)]);
        let expired = Instant::now().checked_sub(FRESH_FOR).expect("uptime");
        lock(&cache.state).reading.as_mut().unwrap().0 = expired;

        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = |bytes| {
            let runs = runs.clone();
            move || {
                runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(800));
                sized(bytes)
            }
        };
        let started = Instant::now();
        let stale = cache.usage_with(soon(), counted(2)).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(700), "bounded");
        assert_eq!(sizes(&stale), [Some(1)], "the last reading stands");
        let joined = cache.usage_with(soon(), counted(3)).await.unwrap();
        assert_eq!(sizes(&joined), [Some(1)]);
        cache.forget();
        let forgotten = cache.usage_with(soon(), counted(4)).await.unwrap();
        assert!(forgotten.is_empty(), "gc dropped the reading");
        let late = Instant::now() + Duration::from_secs(5);
        let after = cache.usage_with(late.into(), counted(5)).await.unwrap();
        assert_eq!(sizes(&after), [Some(4)], "measured anew after gc");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn unreadable_paths_report_errors_instead_of_zero_usage() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "data").unwrap();
        let usage = reading("broken", file.join("not-a-directory"));
        assert!(usage.bytes.is_none());
        assert!(usage.error.is_some());
    }
}
