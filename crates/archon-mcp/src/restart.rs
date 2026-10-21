//! A bridge that outlived an upgrade: Aether keeps the `archon mcp` process
//! it spawned, so after `archon update` it is still the old binary and
//! archond refuses its protocol. The bridge notices that its executable was
//! replaced, finishes the calls in flight and starts the new one in its place.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;

use archon_core::error::ArchonError;

/// Which file `archon_bin` named when the bridge started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BinaryStamp {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
}

impl BinaryStamp {
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            device: meta.dev(),
            inode: meta.ino(),
            len: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
        })
    }
}

/// Whether the file at `path` is no longer the one the bridge started from.
/// A binary that cannot be read now is not a replacement to start.
pub(crate) fn replaced(started_as: Option<BinaryStamp>, path: &Path) -> bool {
    match (started_as, BinaryStamp::of(path)) {
        (Some(then), Some(now)) => then != now,
        _ => false,
    }
}

/// Replace this process with `archon_bin mcp`, on the same stdin and stdout.
/// Returns only the error that kept it from starting.
pub fn exec_current(archon_bin: &Path) -> ArchonError {
    assert!(archon_bin.is_absolute(), "the binary is named in full");
    let error = std::process::Command::new(archon_bin).arg("mcp").exec();
    ArchonError::io(
        format!(
            "restarting the archon MCP bridge as {}; run `archon aether mcp refresh archon` to restart it",
            archon_bin.display()
        ),
        error,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_swapped_in_place_is_replaced_and_an_untouched_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("archon");
        std::fs::write(&bin, "old").unwrap();
        let started = BinaryStamp::of(&bin);
        assert!(started.is_some());
        assert!(!replaced(started, &bin));
        let new = dir.path().join("archon.new");
        std::fs::write(&new, "a newer build").unwrap();
        std::fs::rename(&new, &bin).unwrap();
        assert!(replaced(started, &bin));
        std::fs::remove_file(&bin).unwrap();
        assert!(!replaced(started, &bin), "a missing binary starts nothing");
    }
}
