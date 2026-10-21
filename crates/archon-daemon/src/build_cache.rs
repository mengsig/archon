//! The build cache (ARCHITECTURE §13.5): where a repository's stages put
//! cargo's output, and how lean it is. By default each item gets its own
//! target directory under `<data>/cache/<repo>/items/<item>/`, kept across
//! its retries and deleted when it ends; the other mode is one directory per
//! repository. Aether cannot set an agent's environment, so cargo finds the
//! lean profile (and the shared directory) through a `.cargo/config.toml`
//! archond keeps above the repository's worktrees, and the stage contract
//! names the directory for any shell cargo runs elsewhere.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use archon_core::config::BuildCache;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::Reclaimed;
use archon_core::repo::Repo;
use archon_queue::Store;

use crate::context::DaemonPaths;
use crate::gc::cache::remove_tree;

/// The cargo target directory inside a cache directory.
pub(crate) const CARGO_TARGET: &str = "cargo-target";
/// Holds one directory per item inside a repository's cache directory.
pub(crate) const ITEMS_DIR: &str = "items";
/// Touched whenever a run starts in the repository: the cache's last use.
pub(crate) const USED_MARKER: &str = ".used";
/// Starts the first line of a cargo config archond wrote; it removes no other.
const CONFIG_MARK: &str = "# Written by archond:";
const CONFIG_HEADER: &str =
    "# Written by archond: this repository's stages build with this cargo config.";

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

/// `<data>/cache/<repo>/items/<item>`, what one item's stages build.
pub(crate) fn item_dir(paths: &DaemonPaths, repo_name: &str, item: &QueueItemId) -> PathBuf {
    assert!(
        item.as_str()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "{item} is not path-safe"
    );
    repo_dir(paths, repo_name)
        .join(ITEMS_DIR)
        .join(item.as_str())
}

/// The target directory `item`'s stages build into under `mode`, if archond
/// provides one.
pub(crate) fn target_dir(
    paths: &DaemonPaths,
    repo_name: &str,
    item: &QueueItemId,
    mode: BuildCache,
) -> Option<PathBuf> {
    match mode {
        BuildCache::Shared => Some(cargo_target(paths, repo_name)),
        BuildCache::Item => Some(item_dir(paths, repo_name, item).join(CARGO_TARGET)),
        BuildCache::Worktree => None,
    }
}

/// Make the build cache ready for `item` starting in `repo_name`, and keep
/// the repository's cargo config in line with `mode` and `lean`; with
/// neither it takes back the config archond wrote earlier. Safe to repeat,
/// as a retry does. Worktrees of the repository live under
/// `<worktrees>/<repo>/`.
pub(crate) fn prepare(
    paths: &DaemonPaths,
    repo_name: &str,
    item: &QueueItemId,
    mode: BuildCache,
    lean: bool,
) -> Result<(), ArchonError> {
    let config = paths
        .worktrees
        .join(repo_name)
        .join(".cargo")
        .join("config.toml");
    if let Some(target) = target_dir(paths, repo_name, item, mode) {
        create_dir(&target)?;
        let marker = repo_dir(paths, repo_name).join(USED_MARKER);
        std::fs::write(&marker, archon_core::now().to_rfc3339())
            .map_err(|e| ArchonError::io(format!("writing {}", marker.display()), e))?;
    }
    if mode != BuildCache::Shared && !lean {
        return retire_config(&config);
    }
    let shared = (mode == BuildCache::Shared).then(|| cargo_target(paths, repo_name));
    write_config(&config, &config_text(shared.as_deref(), lean)?)
}

/// Whether nothing of `item` can still build: it is final, none of its
/// stage runs is spawning or running, no stop of their agents or what they
/// delegated is owed, and every run's node is retired, which waits for
/// Aether to confirm that all recorded builders and descendants have ended (an
/// accepted cancel is cooperative). The caller holds the run table.
pub(crate) fn is_idle(store: &Store, item: &QueueItemId) -> Result<bool, ArchonError> {
    if !store.item(item)?.state.is_terminal() {
        return Ok(false);
    }
    for stage in store.stage_runs_for_item(item)? {
        if !stage.state.is_terminal() || crate::executor::stop_owed(store, &stage.id)? {
            return Ok(false);
        }
    }
    for run in store.runs_for_item(item)? {
        if !run.state.is_terminal() || !crate::executor::node_retired(store, &run.id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `kv`: `build_release:<item>` → the repository name, while the item's
/// directory waits for Aether to confirm its agents ended.
const RELEASE_KEY_PREFIX: &str = "build_release:";

/// Owe the release of `item`'s build directory: kept until its agents are
/// confirmed ended, and meanwhile never swept by gc.
pub(crate) fn owe_release(
    store: &mut Store,
    item: &QueueItemId,
    repo_name: &str,
) -> Result<(), ArchonError> {
    store.kv_set(&format!("{RELEASE_KEY_PREFIX}{item}"), repo_name)
}

pub(crate) fn forget_release(store: &mut Store, item: &QueueItemId) -> Result<(), ArchonError> {
    store.kv_delete(&format!("{RELEASE_KEY_PREFIX}{item}"))
}

/// Every owed release, as `(item, repository name)`.
pub(crate) fn owed_releases(store: &Store) -> Result<Vec<(QueueItemId, String)>, ArchonError> {
    let mut owed = Vec::new();
    for (key, repo_name) in store.kv_prefix(RELEASE_KEY_PREFIX)? {
        let raw = key
            .strip_prefix(RELEASE_KEY_PREFIX)
            .expect("ranged by prefix");
        let item = QueueItemId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("build release key {key}: {e}")))?;
        owed.push((item, repo_name));
    }
    Ok(owed)
}

/// Whether gc may sweep `item`'s directory: idle, and no release owed,
/// which the item's end itself makes once Aether confirms its agents ended.
pub(crate) fn is_sweepable(store: &Store, item: &QueueItemId) -> Result<bool, ArchonError> {
    let owed = store
        .kv_get(&format!("{RELEASE_KEY_PREFIX}{item}"))?
        .is_some();
    Ok(!owed && is_idle(store, item)?)
}

/// A build directory renamed aside, atomically, so a run that starts
/// meanwhile builds into a fresh one; [`delete_claimed`] removes it.
#[derive(Debug)]
pub(crate) struct ClaimedBuild {
    original: PathBuf,
    aside: PathBuf,
}

/// Set `item`'s build directory aside, under the run table. `None` when it
/// had none. A leftover `.gc-` name is found and deleted by the next gc pass.
pub(crate) fn claim_item(
    paths: &DaemonPaths,
    repo_name: &str,
    item: &QueueItemId,
) -> Result<Option<ClaimedBuild>, ArchonError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let original = item_dir(paths, repo_name, item);
    let items = original
        .parent()
        .expect("an item directory is under items/");
    for ancestor in [repo_dir(paths, repo_name).as_path(), items] {
        match real_dir(ancestor) {
            Ok(true) => {}
            Ok(false) => {
                return Err(ArchonError::Invalid(format!(
                    "{} is not a directory of archond's (a symlink?); its build directories are left alone",
                    ancestor.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ArchonError::io(
                    format!("inspecting {}", ancestor.display()),
                    e,
                ));
            }
        }
    }
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!(".gc-{}-{item}-{n}", std::process::id());
    let aside = original.with_file_name(name);
    match std::fs::rename(&original, &aside) {
        Ok(()) => Ok(Some(ClaimedBuild { original, aside })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchonError::io(
            format!("setting {} aside", original.display()),
            e,
        )),
    }
}

/// Whether `dir` is a directory itself, not a symlink to one: a deletion
/// under the cache must never reach outside it.
pub(crate) fn real_dir(dir: &Path) -> std::io::Result<bool> {
    Ok(std::fs::symlink_metadata(dir)?.file_type().is_dir())
}

/// Delete a claimed build directory, however its tests left it. Blocking.
pub(crate) fn delete_claimed(claimed: ClaimedBuild) -> Result<Reclaimed, ArchonError> {
    let bytes = remove_tree(&claimed.aside)?;
    Ok(Reclaimed {
        path: claimed.original,
        bytes,
    })
}

/// Claim and delete `item`'s build directory. `None` when it had none.
#[cfg(test)]
pub(crate) fn release_item(
    paths: &DaemonPaths,
    repo_name: &str,
    item: &QueueItemId,
) -> Result<Option<Reclaimed>, ArchonError> {
    claim_item(paths, repo_name, item)?
        .map(delete_claimed)
        .transpose()
}

fn create_dir(dir: &Path) -> Result<(), ArchonError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))
}

/// The config: `target` when the repository shares one directory, and with
/// `lean` no incremental state and line tables for debuginfo, which keeps
/// backtraces' file:line. Cargo takes each key from the nearest config, so a
/// repository's own `.cargo/config.toml` wins where it sets the key; a
/// `[profile]` here beats the one in `Cargo.toml`, hence `[cache] lean = false`.
fn config_text(target: Option<&Path>, lean: bool) -> Result<String, ArchonError> {
    let mut text = format!("{CONFIG_HEADER}\n[build]\n");
    if lean {
        text.push_str("incremental = false\n");
    }
    if let Some(target) = target {
        let target = target
            .to_str()
            .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", target.display())))?;
        text.push_str(&format!(
            "target-dir = {}\n",
            serde_json::to_string(target)?
        ));
    }
    if lean {
        for profile in ["dev", "test"] {
            text.push_str(&format!(
                "\n[profile.{profile}]\ndebug = \"line-tables-only\"\n"
            ));
        }
    }
    Ok(text)
}

fn write_config(config: &Path, text: &str) -> Result<(), ArchonError> {
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

    fn config_of(root: &Path) -> PathBuf {
        root.join("worktrees/archon/.cargo/config.toml")
    }

    #[test]
    fn the_shared_cache_is_per_repository_and_an_item_s_is_its_own() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let target = cargo_target(&paths, "archon");
        assert_eq!(target, root.path().join("data/cache/archon/cargo-target"));
        assert_ne!(target, cargo_target(&paths, "other"));

        let (a, b) = (QueueItemId::new(), QueueItemId::new());
        let dir = target_dir(&paths, "archon", &a, BuildCache::Item).unwrap();
        assert_eq!(
            dir,
            root.path()
                .join(format!("data/cache/archon/items/{a}/cargo-target"))
        );
        assert_ne!(
            Some(dir),
            target_dir(&paths, "archon", &b, BuildCache::Item)
        );
        assert_eq!(
            target_dir(&paths, "archon", &a, BuildCache::Shared),
            Some(target)
        );
        assert_eq!(target_dir(&paths, "archon", &a, BuildCache::Worktree), None);
    }

    #[test]
    fn the_lean_config_trims_debuginfo_and_incremental_state() {
        let text = config_text(None, true).unwrap();
        assert!(text.starts_with(CONFIG_MARK), "{text}");
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(parsed["build"]["incremental"].as_bool(), Some(false));
        assert!(parsed["build"].get("target-dir").is_none(), "{text}");
        for profile in ["dev", "test"] {
            assert_eq!(
                parsed["profile"][profile]["debug"].as_str(),
                Some("line-tables-only"),
                "{text}"
            );
        }
        let shared = config_text(Some(Path::new("/d/cache/r/cargo-target")), false).unwrap();
        let parsed: toml::Table = toml::from_str(&shared).unwrap();
        assert_eq!(
            parsed["build"]["target-dir"].as_str(),
            Some("/d/cache/r/cargo-target")
        );
        assert!(!shared.contains("incremental") && !shared.contains("profile"));
    }

    #[test]
    fn prepare_makes_the_item_s_directory_and_a_lean_config_and_is_repeatable() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let item = QueueItemId::new();
        prepare(&paths, "archon", &item, BuildCache::Item, true).unwrap();
        let target = target_dir(&paths, "archon", &item, BuildCache::Item).unwrap();
        std::fs::write(target.join("kept"), b"x").unwrap();
        prepare(&paths, "archon", &item, BuildCache::Item, true).unwrap();
        assert!(target.join("kept").exists(), "a retry reuses the directory");
        assert!(repo_dir(&paths, "archon").join(USED_MARKER).is_file());
        let text = std::fs::read_to_string(config_of(root.path())).unwrap();
        assert_eq!(text, config_text(None, true).unwrap());
    }

    #[test]
    fn the_shared_mode_points_cargo_at_the_repository_directory() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        prepare(
            &paths,
            "archon",
            &QueueItemId::new(),
            BuildCache::Shared,
            false,
        )
        .unwrap();
        let target = cargo_target(&paths, "archon");
        assert!(target.is_dir());
        let text = std::fs::read_to_string(config_of(root.path())).unwrap();
        assert!(
            text.contains(&format!("target-dir = {:?}", target.to_str().unwrap())),
            "{text}"
        );
    }

    #[test]
    fn release_deletes_the_item_s_directory_even_with_unreadable_subdirectories() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let (item, other) = (QueueItemId::new(), QueueItemId::new());
        prepare(&paths, "archon", &item, BuildCache::Item, true).unwrap();
        prepare(&paths, "archon", &other, BuildCache::Item, true).unwrap();
        let target = target_dir(&paths, "archon", &item, BuildCache::Item).unwrap();
        let locked = target.join("debug/locked/inner");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("f"), vec![1u8; 50_000]).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(
            target.join("debug/locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let freed = release_item(&paths, "archon", &item).unwrap().unwrap();
        assert!(freed.bytes >= 50_000, "{freed:?}");
        assert!(!item_dir(&paths, "archon", &item).exists());
        assert!(item_dir(&paths, "archon", &other).exists(), "only its own");
        assert!(release_item(&paths, "archon", &item).unwrap().is_none());
    }

    #[test]
    fn turning_the_cache_off_takes_back_only_archonds_config() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let item = QueueItemId::new();
        let config = config_of(root.path());
        prepare(&paths, "archon", &item, BuildCache::Item, true).unwrap();
        prepare(&paths, "archon", &item, BuildCache::Worktree, false).unwrap();
        assert!(!config.exists(), "archond's own config goes");

        std::fs::write(&config, "[build]\ntarget-dir = \"/mine\"\n").unwrap();
        prepare(&paths, "archon", &item, BuildCache::Item, true).unwrap();
        prepare(&paths, "archon", &item, BuildCache::Worktree, false).unwrap();
        let kept = std::fs::read_to_string(&config).unwrap();
        assert!(
            kept.contains("/mine"),
            "an operator's config is kept: {kept}"
        );
    }
}
