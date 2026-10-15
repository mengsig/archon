//! Reviewers' experiment directories, `<data>/experiments/<item id>` (ARCHITECTURE §13.5),
//! kept until nothing of the item runs; neither they nor their root may be symlinks.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;

use crate::context::DaemonPaths;

pub(crate) fn item_dir(paths: &DaemonPaths, item: &QueueItemId) -> PathBuf {
    let dir = paths.experiments().join(item.as_str());
    assert!(dir.is_absolute(), "the data directory is absolute");
    dir
}

/// Whether `dir` exists as a real directory. A symlink, which could lead
/// out of the data directory, or any other file is refused.
pub(crate) fn real_dir(dir: &Path) -> Result<bool, ArchonError> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ArchonError::io(format!("inspecting {}", dir.display()), e)),
    };
    if meta.is_dir() {
        return Ok(true);
    }
    let what = match meta.file_type().is_symlink() {
        true => "a symlink, which Archon does not follow",
        false => "not a directory",
    };
    Err(ArchonError::Invalid(format!("{} is {what}", dir.display())))
}

/// Create the item's directory unless it exists. Blocking; the caller holds
/// the run table and the spawn's claim, so it never races a discard.
pub(crate) fn prepare(paths: &DaemonPaths, item: &QueueItemId) -> Result<(), ArchonError> {
    let dir = item_dir(paths, item);
    assert!(
        !dir.starts_with(&paths.scratch),
        "no scratch-mode workspace holds it"
    );
    for step in [paths.experiments(), dir] {
        match std::fs::create_dir(&step) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(ArchonError::io(format!("creating {}", step.display()), e)),
        }
        let made = real_dir(&step)?;
        assert!(made, "{} was just created", step.display());
    }
    Ok(())
}

/// Rename a stopped item's directory aside to delete, with the run table held
/// so a retry that starts meanwhile creates a fresh one. `None` when it has none.
pub(crate) fn set_aside(paths: &DaemonPaths, item: &QueueItemId) -> Option<PathBuf> {
    let dir = item_dir(paths, item);
    let renamed = match real_dir(&paths.experiments()).and_then(|_| real_dir(&dir)) {
        Ok(false) => return None,
        Ok(true) => crate::gc::set_aside(&dir).map_err(|e| e.to_string()),
        Err(refused) => Err(refused.to_string()),
    };
    match renamed {
        Ok(aside) => {
            assert_eq!(aside.parent(), dir.parent(), "set aside in place");
            Some(aside)
        }
        Err(error) => {
            // The item is settled, so a gc pass collects what it can.
            tracing::warn!(dir = %dir.display(), %error, "experiments not set aside");
            None
        }
    }
}

/// Delete a directory [`set_aside`] renamed. Its name marks it claimed, so a
/// deletion that fails is logged and left to the next gc pass.
pub(crate) async fn delete(paths: &DaemonPaths, aside: PathBuf) {
    assert_eq!(
        aside.parent(),
        Some(paths.experiments().as_path()),
        "set aside under the experiments root"
    );
    let claimed = aside.file_name().and_then(|name| name.to_str());
    assert!(
        claimed.is_some_and(crate::gc::is_claimed),
        "only a claimed name is deleted: {}",
        aside.display()
    );
    let target = aside.clone();
    let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&target)).await;
    let error = match removed {
        Ok(Ok(())) => return,
        Ok(Err(e)) if e.kind() == ErrorKind::NotFound => return,
        Ok(Err(e)) => e.to_string(),
        Err(e) => format!("the removal task failed: {e}"),
    };
    tracing::warn!(dir = %aside.display(), %error, "experiments not deleted; gc retries");
}
