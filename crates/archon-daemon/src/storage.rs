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
        ("build_cache", paths.cache()),
        ("artifacts", paths.artifacts()),
        ("reports_lavish", paths.reports.clone()),
        ("logs", paths.logs.clone()),
        ("store", paths.store.clone()),
        ("orchestrator_mirrors", paths.orchestrator_workspace.clone()),
        ("aether", paths.data_dir.join("aether")),
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
        add(
            store,
            &PathBuf::from(format!("{}{suffix}", paths.store.display())),
        );
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
            Ok(entry) if entry.file_name().to_string_lossy().starts_with(&prefix) => {
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
