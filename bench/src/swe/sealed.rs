//! Guards around the sealed holdout. Its store (`~/.local/share/bench-holdout`)
//! and keys (`~/.config/bench-holdout`) belong to Aether's runner: archon-bench
//! never reads, writes or points a run into them, and its own scratch space is
//! private and short-lived.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail, ensure};

use crate::workspace;

/// Every directory the holdout or its keys may live in.
fn store_roots() -> anyhow::Result<Vec<PathBuf>> {
    let var = std::env::var_os;
    store_roots_from(var("HOME"), var("XDG_DATA_HOME"), var("XDG_CONFIG_HOME"))
}

/// Aether's fixed HOME paths, plus their XDG forms in case a later Aether honours them.
fn store_roots_from(
    home: Option<OsString>,
    xdg_data: Option<OsString>,
    xdg_config: Option<OsString>,
) -> anyhow::Result<Vec<PathBuf>> {
    let absolute =
        |value: Option<OsString>| value.map(PathBuf::from).filter(|path| path.is_absolute());
    let home =
        absolute(home).context("HOME is not an absolute path, so the holdout store is unknown")?;
    let mut roots = vec![
        home.join(".local/share/bench-holdout"),
        home.join(".config/bench-holdout"),
    ];
    for base in [absolute(xdg_data), absolute(xdg_config)]
        .into_iter()
        .flatten()
    {
        roots.push(base.join("bench-holdout"));
    }
    roots.sort();
    roots.dedup();
    assert!(
        roots.iter().all(|r| r.is_absolute()),
        "every store root is absolute"
    );
    Ok(roots)
}

/// Refuse any path inside the holdout store or its keys, before anything opens it.
pub fn refuse_inside_store(path: &Path) -> anyhow::Result<()> {
    for store in store_roots()? {
        refuse_inside(path, &store)?;
    }
    Ok(())
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
    fn the_store_covers_home_data_and_keys_whatever_xdg_says() {
        let roots = store_roots_from(Some("/h".into()), Some("/x".into()), None).unwrap();
        assert_eq!(
            roots,
            [
                PathBuf::from("/h/.config/bench-holdout"),
                PathBuf::from("/h/.local/share/bench-holdout"),
                PathBuf::from("/x/bench-holdout"),
            ]
        );
        let roots =
            store_roots_from(Some("/h".into()), Some("rel".into()), Some("/c".into())).unwrap();
        assert!(roots.contains(&PathBuf::from("/c/bench-holdout")));
        assert_eq!(roots.len(), 3, "a relative XDG value is ignored");
        assert!(store_roots_from(Some("rel".into()), Some("/x".into()), None).is_err());
        assert!(store_roots_from(None, None, None).is_err());
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
