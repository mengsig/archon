//! Read-only filesystem accounting, cached briefly for frequent status polls.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;
use archon_core::protocol::StorageUsage;

use crate::context::{DaemonContext, DaemonPaths};
use crate::gc::cache::disk_usage;

pub(crate) async fn usage(ctx: &DaemonContext) -> Result<Vec<StorageUsage>, ArchonError> {
    let mut cached = ctx.storage.lock().await;
    if let Some((at, usage)) = &*cached
        && at.elapsed() < Duration::from_secs(60)
    {
        return Ok(usage.clone());
    }
    let paths = ctx.paths.clone();
    let usage = tokio::task::spawn_blocking(move || measure(&paths))
        .await
        .map_err(|error| ArchonError::io("measuring storage", std::io::Error::other(error)))?;
    *cached = Some((Instant::now(), usage.clone()));
    Ok(usage)
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
