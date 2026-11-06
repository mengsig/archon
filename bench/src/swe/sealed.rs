//! Guards around the sealed holdout. Its store
//! (`$XDG_DATA_HOME/bench-holdout`, normally `~/.local/share/bench-holdout`)
//! belongs to Aether's runner: archon-bench never reads, writes or points a
//! run into it, and its own scratch space is private and short-lived.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail, ensure};

use crate::workspace;

/// The holdout store's root, from the same variables as the work directory.
pub fn store_root() -> anyhow::Result<PathBuf> {
    store_root_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

fn store_root_from(xdg: Option<OsString>, home: Option<OsString>) -> anyhow::Result<PathBuf> {
    let absolute = |value: OsString| {
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    };
    let data_home = xdg
        .and_then(absolute)
        .or_else(|| home.and_then(absolute).map(|h| h.join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is absolute, so the holdout store is unknown")?;
    Ok(data_home.join("bench-holdout"))
}

/// Refuse any path inside the holdout store, before anything opens it.
pub fn refuse_inside_store(path: &Path) -> anyhow::Result<()> {
    refuse_inside(path, &store_root()?)
}

fn refuse_inside(path: &Path, store: &Path) -> anyhow::Result<()> {
    assert!(store.is_absolute(), "the holdout store is absolute");
    let absolute =
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    ensure!(
        !absolute.components().any(|c| c == Component::ParentDir),
        "{} climbs with `..`; give a direct path",
        path.display()
    );
    // Resolve symlinks where the path exists, so a link into the store is caught too.
    let resolved = existing_prefix_canonical(&absolute)?;
    let store_resolved = existing_prefix_canonical(store)?;
    for candidate in [&absolute, &resolved] {
        if candidate.starts_with(store) || candidate.starts_with(&store_resolved) {
            bail!(
                "refused: {} is inside the sealed holdout store",
                path.display()
            );
        }
    }
    Ok(())
}

/// The canonical form of the longest existing prefix of `path`, with the rest appended.
fn existing_prefix_canonical(path: &Path) -> anyhow::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
            break;
        };
        rest.push(name);
        existing.pop();
    }
    let mut resolved = std::fs::canonicalize(&existing)
        .with_context(|| format!("resolving {}", existing.display()))?;
    resolved.extend(rest.iter().rev());
    Ok(resolved)
}

/// A new owner-only directory under `work_dir` for one runner invocation.
pub fn private_scratch(work_dir: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        work_dir.is_absolute(),
        "the work directory {} is not absolute",
        work_dir.display()
    );
    refuse_inside_store(work_dir)?;
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("creating the work directory {}", work_dir.display()))?;
    let dir = work_dir.join(format!("swe-{}", workspace::unguessable_name()?));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating the runner scratch {}", dir.display()))?;
    let mode = std::fs::metadata(&dir)?.permissions().mode() & 0o777;
    assert_eq!(mode & 0o077, 0, "the runner scratch is owner-only");
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_follows_xdg_data_home_then_home() {
        let root = store_root_from(Some("/x".into()), Some("/h".into())).unwrap();
        assert_eq!(root, Path::new("/x/bench-holdout"));
        let root = store_root_from(Some("relative".into()), Some("/h".into())).unwrap();
        assert_eq!(root, Path::new("/h/.local/share/bench-holdout"));
        assert!(store_root_from(None, None).is_err());
    }

    #[test]
    fn paths_in_the_store_or_linked_into_it_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("bench-holdout");
        std::fs::create_dir_all(store.join("swe-v1")).unwrap();
        assert!(refuse_inside(&store.join("swe-v1/holdout.gpg"), &store).is_err());
        assert!(refuse_inside(&store, &store).is_err());
        let link = dir.path().join("innocent");
        std::os::unix::fs::symlink(store.join("swe-v1"), &link).unwrap();
        assert!(
            refuse_inside(&link.join("holdout.gpg"), &store).is_err(),
            "through a symlink"
        );
        assert!(refuse_inside(&dir.path().join("bench-holdout-not/x"), &store).is_ok());
        assert!(refuse_inside(&dir.path().join("results/new"), &store).is_ok());
        let climbing = dir.path().join("results/../bench-holdout");
        assert!(refuse_inside(&climbing, &store).is_err());
    }

    #[test]
    fn scratch_directories_are_new_and_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = private_scratch(dir.path()).unwrap();
        let b = private_scratch(dir.path()).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(private_scratch(Path::new("relative")).is_err());
    }
}
