//! A mirror's work tree is read-only on disk: no write bit on any file or
//! directory, so an engine's native edit, patch or shell write into it fails
//! with a permission error, whatever Aether's scope or the engine's sandbox
//! allow. The daemon opens a tree only for its own checkout and closes it
//! again. The git directory stays writable: Aether snapshots a mirror there.

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

const WRITE_BITS: u32 = 0o222;

/// Close the tree at `root`: clear every write bit, keeping each directory
/// readable and searchable by its owner. Nothing there is fine.
pub(crate) fn close(root: &Path) -> Result<(), ArchonError> {
    walk(root, |mode, is_dir| {
        let readable = if is_dir { mode | 0o500 } else { mode };
        readable & !WRITE_BITS
    })
}

/// Open the tree at `root` for the daemon's own writes: its owner may write
/// every file and enter and change every directory. Nothing there is fine.
pub(crate) fn open(root: &Path) -> Result<(), ArchonError> {
    walk(
        root,
        |mode, is_dir| {
            if is_dir { mode | 0o700 } else { mode | 0o600 }
        },
    )
}

/// Give every entry under `root`, itself included, the mode `wanted` makes
/// of its own. A directory's mode is set before it is read, and no symbolic
/// link is followed or changed.
fn walk(root: &Path, wanted: impl Fn(u32, bool) -> u32) -> Result<(), ArchonError> {
    assert!(root.is_absolute(), "mirror paths are absolute");
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            continue;
        }
        let mode = meta.permissions().mode() & 0o7777;
        let next = wanted(mode, kind.is_dir());
        assert_eq!(next & !0o7777, 0, "only permission bits change");
        if next != mode {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(next)).map_err(
                |e| ArchonError::io(format!("setting the mode of {}", path.display()), e),
            )?;
        }
        if !kind.is_dir() {
            continue;
        }
        let reading = |e| ArchonError::io(format!("reading {}", path.display()), e);
        for entry in std::fs::read_dir(&path).map_err(reading)? {
            pending.push(entry.map_err(reading)?.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn a_closed_tree_refuses_writes_until_it_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("mirror");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.py"), "x = 1\n").unwrap();
        std::fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(root.join("run.sh"), PermissionsExt::from_mode(0o755)).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "kept writable\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        close(&root).unwrap();
        assert_eq!(mode(&root.join("src")), 0o555);
        assert_eq!(mode(&root.join("src/lib.py")), 0o444);
        assert_eq!(
            mode(&root.join("run.sh")),
            0o555,
            "executable stays executable"
        );
        assert_eq!(mode(&outside), 0o644, "a link's target is not changed");
        let edit = std::fs::write(root.join("src/lib.py"), "x = 2\n");
        assert_eq!(edit.unwrap_err().kind(), ErrorKind::PermissionDenied);
        let add = std::fs::write(root.join("src/new.py"), "");
        assert_eq!(add.unwrap_err().kind(), ErrorKind::PermissionDenied);
        let rename = std::fs::rename(root.join("src/lib.py"), root.join("src/moved.py"));
        assert_eq!(rename.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.py")).unwrap(),
            "x = 1\n"
        );

        open(&root).unwrap();
        std::fs::write(root.join("src/lib.py"), "x = 2\n").unwrap();
        std::fs::write(root.join("src/new.py"), "").unwrap();
        assert_eq!(mode(&root.join("run.sh")), 0o755);
    }

    #[test]
    fn a_directory_its_owner_cannot_enter_is_still_closed_and_opened() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("mirror");
        std::fs::create_dir_all(root.join("hidden")).unwrap();
        std::fs::write(root.join("hidden/file"), "").unwrap();
        std::fs::set_permissions(root.join("hidden"), PermissionsExt::from_mode(0o000)).unwrap();
        close(&root).unwrap();
        assert_eq!(mode(&root.join("hidden")), 0o500);
        assert_eq!(mode(&root.join("hidden/file")), 0o444);
        open(&root).unwrap();
        assert_eq!(mode(&root.join("hidden")), 0o700);
        assert!(close(&dir.path().canonicalize().unwrap().join("absent")).is_ok());
    }
}
