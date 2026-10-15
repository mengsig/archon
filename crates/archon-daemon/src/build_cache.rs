//! The shared build cache (ARCHITECTURE §13.3): one cargo target directory
//! per repository under `<data>/cache/<repo>/`, never one per task. Aether
//! cannot set an agent's environment, so cargo finds the directory through
//! a `.cargo/config.toml` archond keeps above the repository's worktrees,
//! and the stage contract names it for any shell cargo runs elsewhere.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::repo::Repo;

use crate::context::DaemonPaths;

/// The cargo target directory inside a repository's cache directory.
pub(crate) const CARGO_TARGET: &str = "cargo-target";
/// Touched whenever a run starts in the repository: the cache's last use.
pub(crate) const USED_MARKER: &str = ".used";
/// First line of the cargo config archond wrote; it removes no other.
const CONFIG_MARK: &str = "# Written by archond: this repository's stages share one build cache.";

/// `<data>/cache/<repo>`, everything the repository's stages share.
pub(crate) fn repo_dir(paths: &DaemonPaths, repo_name: &str) -> PathBuf {
    assert!(
        Repo::valid_name(repo_name),
        "{repo_name:?} is not path-safe"
    );
    paths.cache().join(repo_name)
}

pub(crate) fn cargo_target(paths: &DaemonPaths, repo_name: &str) -> PathBuf {
    repo_dir(paths, repo_name).join(CARGO_TARGET)
}

/// Make the cache ready for a run starting in `repo_name`, or, with
/// `shared` off, take back the cargo config archond wrote earlier. Safe to
/// repeat. Worktrees of the repository live under `<worktrees>/<repo>/`.
pub(crate) fn prepare(
    paths: &DaemonPaths,
    repo_name: &str,
    shared: bool,
) -> Result<(), ArchonError> {
    let config = paths
        .worktrees
        .join(repo_name)
        .join(".cargo")
        .join("config.toml");
    if !shared {
        return retire_config(&config);
    }
    let target = cargo_target(paths, repo_name);
    create_dir(&target)?;
    let marker = repo_dir(paths, repo_name).join(USED_MARKER);
    std::fs::write(&marker, archon_core::now().to_rfc3339())
        .map_err(|e| ArchonError::io(format!("writing {}", marker.display()), e))?;
    write_config(&config, &target)
}

fn create_dir(dir: &Path) -> Result<(), ArchonError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))
}

/// The config that points cargo at `target`; a repository's own
/// `.cargo/config.toml` is nearer a worktree, so its `target-dir` wins.
fn config_text(target: &Path) -> Result<String, ArchonError> {
    let target = target
        .to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", target.display())))?;
    let quoted = serde_json::to_string(target)?;
    Ok(format!("{CONFIG_MARK}\n[build]\ntarget-dir = {quoted}\n"))
}

fn write_config(config: &Path, target: &Path) -> Result<(), ArchonError> {
    let text = config_text(target)?;
    match std::fs::read_to_string(config) {
        Ok(current) if current == text => return Ok(()),
        // Somebody else's file: leave it, and the contract still names the path.
        Ok(current) if !current.starts_with(CONFIG_MARK) => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ArchonError::io(format!("reading {}", config.display()), e)),
    }
    let dir = config.parent().expect("the config has a directory");
    create_dir(dir)?;
    std::fs::write(config, text)
        .map_err(|e| ArchonError::io(format!("writing {}", config.display()), e))
}

fn retire_config(config: &Path) -> Result<(), ArchonError> {
    let current = match std::fs::read_to_string(config) {
        Ok(current) => current,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ArchonError::io(format!("reading {}", config.display()), e)),
    };
    if !current.starts_with(CONFIG_MARK) {
        return Ok(());
    }
    std::fs::remove_file(config)
        .map_err(|e| ArchonError::io(format!("removing {}", config.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> DaemonPaths {
        DaemonPaths {
            socket: root.join("archond.sock"),
            data_dir: root.join("data"),
            store: root.join("data/archon.db"),
            worktrees: root.join("worktrees"),
            orchestrator_workspace: root.join("data/orchestrator"),
            scratch: root.join("data/scratch"),
            reports: root.join("data/reports"),
            logs: root.join("data/logs"),
            archon_bin: PathBuf::from("/usr/bin/false"),
        }
    }

    #[test]
    fn the_cache_is_per_repository_under_the_data_dir() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let target = cargo_target(&paths, "archon");
        assert_eq!(target, root.path().join("data/cache/archon/cargo-target"));
        assert_ne!(target, cargo_target(&paths, "other"));
    }

    #[test]
    fn prepare_creates_the_cache_and_points_cargo_at_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        prepare(&paths, "archon", true).unwrap();
        prepare(&paths, "archon", true).unwrap();
        let target = cargo_target(&paths, "archon");
        assert!(target.is_dir());
        assert!(repo_dir(&paths, "archon").join(USED_MARKER).is_file());
        let config = root.path().join("worktrees/archon/.cargo/config.toml");
        let text = std::fs::read_to_string(config).unwrap();
        assert!(text.starts_with(CONFIG_MARK), "{text}");
        assert!(
            text.contains(&format!("target-dir = {:?}", target.to_str().unwrap())),
            "{text}"
        );
    }

    #[test]
    fn turning_the_cache_off_takes_back_only_archonds_config() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let config = root.path().join("worktrees/archon/.cargo/config.toml");
        prepare(&paths, "archon", true).unwrap();
        prepare(&paths, "archon", false).unwrap();
        assert!(!config.exists(), "archond's own config goes");
        assert!(
            cargo_target(&paths, "archon").is_dir(),
            "the cache itself is gc's to delete"
        );

        std::fs::write(&config, "[build]\ntarget-dir = \"/mine\"\n").unwrap();
        prepare(&paths, "archon", true).unwrap();
        prepare(&paths, "archon", false).unwrap();
        let kept = std::fs::read_to_string(&config).unwrap();
        assert!(
            kept.contains("/mine"),
            "an operator's config is kept: {kept}"
        );
    }
}
