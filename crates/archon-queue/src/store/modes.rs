//! The store holds operator messages, gate answers and spend: its directory
//! is 0700 and its database, WAL and shared-memory files 0600, whatever the
//! umask, as Aether keeps its own store.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Create `dir` and any missing parent 0700; an existing one is left as it is.
pub(super) fn create_private_dir(dir: &Path) -> Result<(), ArchonError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(|e| ArchonError::io(format!("create {}", dir.display()), e))?;
    assert!(dir.is_dir(), "the store's directory exists once created");
    Ok(())
}

/// Create the store file 0600 if it is missing, before SQLite would create
/// it under the umask, and tighten `dir` and every store file that exists.
/// SQLite gives a new `-wal` and `-shm` the database file's mode.
pub(super) fn make_private(dir: &Path, store: &Path) -> Result<(), ArchonError> {
    let parent = store.parent().filter(|p| !p.as_os_str().is_empty());
    assert_eq!(
        parent.unwrap_or(Path::new(".")),
        dir,
        "the store lies in its directory"
    );
    tighten(dir, DIR_MODE)?;
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(store);
    match created {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(ArchonError::io(format!("create {}", store.display()), e)),
    }
    for file in store_files(store) {
        tighten(&file, FILE_MODE)?;
    }
    Ok(())
}

/// Clear the bits of `path`'s mode beyond `mode`; a missing path is fine.
fn tighten(path: &Path, mode: u32) -> Result<(), ArchonError> {
    let current = match std::fs::metadata(path) {
        Ok(meta) => meta.permissions().mode() & 0o7777,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ArchonError::io(format!("inspect {}", path.display()), e)),
    };
    let wanted = current & mode;
    if wanted == current {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted))
        .map_err(|e| ArchonError::io(format!("set the mode of {}", path.display()), e))
}

/// The database file and the WAL and shared-memory files beside it.
fn store_files(store: &Path) -> [PathBuf; 3] {
    let beside = |suffix: &str| {
        let mut name = store.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [store.to_path_buf(), beside("-wal"), beside("-shm")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn existing_store_files_and_their_directory_are_tightened() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = dir.join("archon.db");
        for file in store_files(&store) {
            std::fs::write(&file, b"").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        make_private(&dir, &store).unwrap();
        assert_eq!(mode(&dir), 0o700);
        for file in store_files(&store) {
            assert_eq!(mode(&file), 0o600, "{}", file.display());
        }
    }

    #[test]
    fn a_tighter_mode_is_never_widened() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("archon.db");
        std::fs::write(&store, b"").unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o400)).unwrap();
        make_private(root.path(), &store).unwrap();
        assert_eq!(mode(&store), 0o400);
        assert!(
            !root.path().join("archon.db-wal").exists(),
            "nothing is created beside it"
        );
    }
}
