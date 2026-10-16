//! Git directories only archond writes (ARCHITECTURE §13.4). The shadow of
//! a repository is one per object format under the data directory, run
//! over the repository's own object store: merges, pushes and fetches read
//! its config, which the daemon rewrites before every command, never the
//! repository's, which an agent can write.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use archon_core::error::ArchonError;

use super::daemon_git::{Config, DaemonGit};

/// Under the data directory: the shadows, one per object format.
pub(crate) const SHADOW_DIR: &str = "daemon-git";
/// A git directory still being made; a leading dot is no object format.
const NEW_PREFIX: &str = ".new-";
/// Files git reads in a git directory besides `config`; none is the daemon's.
const FOREIGN: [&str; 5] = [
    "info/attributes",
    "config.worktree",
    "commondir",
    "objects/info/alternates",
    "info/grafts",
];

/// Held while a daemon-owned git directory is made or sealed: deliveries in
/// different repositories run at once and share the shadow of their format.
static SEALING: Mutex<()> = Mutex::new(());

/// [`SEALING`], held. A panic while it was held left at worst a stale config
/// or a half-made directory, which the next holder replaces.
fn sealing() -> MutexGuard<'static, ()> {
    SEALING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A repository's object store seen through the shadow of its format.
#[derive(Debug, Clone)]
pub(crate) struct Shadow {
    git_dir: PathBuf,
    config: String,
    /// The repository's common object store, where objects are read and written.
    objects: PathBuf,
}

impl Shadow {
    /// The shadow for the repository at `repo`, made if missing.
    pub(crate) fn of(data_dir: &Path, repo: &Path) -> Result<Self, ArchonError> {
        assert!(data_dir.is_absolute(), "the data directory is absolute");
        let (common, format) = common_dir_and_format(repo)?;
        let objects = common.join("objects");
        if !objects.is_dir() {
            return Err(ArchonError::Invalid(format!(
                "{} has no object store at {}",
                repo.display(),
                objects.display()
            )));
        }
        let git_dir = data_dir.join(SHADOW_DIR).join(&format);
        let config = sealed_config(&format, true, None);
        make(&git_dir, &format)?;
        assert!(objects.is_absolute(), "rev-parse printed an absolute path");
        Ok(Self {
            git_dir,
            config,
            objects,
        })
    }

    /// Git on the repository's objects through the shadow, reading the
    /// shadow's config and `work_tree`'s files, if given.
    pub(crate) fn git(
        &self,
        work_tree: Option<&Path>,
        config: Config,
    ) -> Result<DaemonGit, ArchonError> {
        seal(&self.git_dir, &self.config)?;
        Ok(DaemonGit::owned(&self.git_dir, work_tree, config).objects(&self.objects))
    }

    /// Where git fetches from and pushes to for `url` once the operator's
    /// `insteadOf` and `pushInsteadOf` apply, as git itself resolves them for a remote.
    pub(crate) fn rewritten(&self, url: &str) -> Result<[String; 2], ArchonError> {
        assert!(!url.is_empty() && !url.contains('\n'), "a checked URL");
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = self.git_dir.parent().expect("a git directory has a parent");
        let scratch = parent.join(format!("rewritten-{}-{n}", std::process::id()));
        let resolved = resolve_in(&scratch, url);
        remove_entry(&scratch)?;
        resolved
    }

    /// An index file of the shadow's own for one command, not yet created.
    pub(crate) fn scratch_index(&self) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        self.git_dir
            .join(format!("index-{}-{n}", std::process::id()))
    }
}

/// `repo`'s common git directory and object format, as git reports them.
fn common_dir_and_format(repo: &Path) -> Result<(PathBuf, String), ArchonError> {
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
        "--show-object-format",
    ];
    let printed = DaemonGit::sealed_in(repo).args(args).text()?;
    let lines: Vec<&str> = printed.lines().collect();
    let [common, format] = lines.as_slice() else {
        return Err(ArchonError::Protocol(format!(
            "git rev-parse printed {printed:?}, not a git directory and an object format"
        )));
    };
    if !matches!(*format, "sha1" | "sha256") || !common.starts_with('/') {
        return Err(ArchonError::Protocol(format!(
            "git rev-parse printed {printed:?} for {}",
            repo.display()
        )));
    }
    Ok((PathBuf::from(common), (*format).to_owned()))
}

/// The whole config of a daemon-owned git directory of object `format`.
pub(crate) fn sealed_config(format: &str, bare: bool, hooks: Option<&Path>) -> String {
    assert!(matches!(format, "sha1" | "sha256"), "a known object format");
    let version = if format == "sha256" { 1 } else { 0 };
    let mut text = format!("[core]\n\trepositoryformatversion = {version}\n\tbare = {bare}\n");
    if let Some(hooks) = hooks {
        assert!(hooks.is_absolute(), "the hooks directory is absolute");
        text.push_str(&format!("\thooksPath = {}\n", hooks.display()));
    }
    if format == "sha256" {
        text.push_str("[extensions]\n\tobjectformat = sha256\n");
    }
    text
}

/// [`Shadow::rewritten`] through a fresh repository at `scratch` whose only
/// remote is `url`; the operator's config brings the rewrites.
fn resolve_in(scratch: &Path, url: &str) -> Result<[String; 2], ArchonError> {
    let parent = scratch.parent().expect("a scratch directory has a parent");
    let path = scratch
        .to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", scratch.display())))?;
    remove_entry(scratch)?;
    DaemonGit::sealed_in(parent)
        .args(["init", "--quiet", "--bare", "--template=", path])
        .text()?;
    DaemonGit::owned(scratch, None, Config::Sealed)
        .args(["config", "remote.archon-rewritten.url", url])
        .text()?;
    let get = |push: bool| -> Result<String, ArchonError> {
        let mut args = vec!["remote", "get-url"];
        args.extend(push.then_some("--push"));
        args.push("archon-rewritten");
        DaemonGit::owned(scratch, None, Config::Operator)
            .args(args)
            .text()
    };
    let urls = [get(false)?, get(true)?];
    assert!(
        urls.iter().all(|u| !u.is_empty() && !u.contains('\n')),
        "one URL each"
    );
    Ok(urls)
}

/// Make the git directory `git_dir` of object `format` if it is missing,
/// whole: made beside its place and renamed in.
pub(crate) fn make(git_dir: &Path, format: &str) -> Result<(), ArchonError> {
    let _sealing = sealing();
    if is_real_dir(git_dir)? {
        return Ok(());
    }
    remove_entry(git_dir)?;
    let parent = git_dir.parent().expect("a git directory has a parent");
    std::fs::create_dir_all(parent)
        .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
    let name = git_dir.file_name().expect("a git directory has a name");
    let fresh = parent.join(format!("{NEW_PREFIX}{}", name.to_string_lossy()));
    remove_entry(&fresh)?;
    let path = fresh
        .to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", fresh.display())))?;
    let format_arg = format!("--object-format={format}");
    DaemonGit::sealed_in(parent)
        .args([
            "init",
            "--quiet",
            "--bare",
            "--template=",
            &format_arg,
            path,
        ])
        .text()?;
    std::fs::rename(&fresh, git_dir)
        .map_err(|e| ArchonError::io(format!("moving {} into place", git_dir.display()), e))?;
    assert!(is_real_dir(git_dir)?, "the git directory was just made");
    Ok(())
}

/// Make `git_dir`'s config exactly `config` and remove every other file git
/// would read there, before a command runs in it.
pub(crate) fn seal(git_dir: &Path, config: &str) -> Result<(), ArchonError> {
    let _sealing = sealing();
    if !is_real_dir(git_dir)? {
        return Err(ArchonError::Invalid(format!(
            "{} is not the git directory archond made",
            git_dir.display()
        )));
    }
    let path = git_dir.join("config");
    let plain = kind(&path)?.is_some_and(|kind| kind.is_file());
    if !plain || std::fs::read_to_string(&path).ok().as_deref() != Some(config) {
        let fresh = git_dir.join("config.archon-new");
        std::fs::write(&fresh, config)
            .map_err(|e| ArchonError::io(format!("writing {}", fresh.display()), e))?;
        std::fs::rename(&fresh, &path)
            .map_err(|e| ArchonError::io(format!("replacing {}", path.display()), e))?;
    }
    for name in FOREIGN {
        remove_entry(&git_dir.join(name))?;
    }
    debug_assert!(!git_dir.join("info/attributes").exists());
    Ok(())
}

pub(crate) fn is_real_dir(path: &Path) -> Result<bool, ArchonError> {
    Ok(kind(path)?.is_some_and(|kind| kind.is_dir()))
}

/// What is at `path` itself, a symbolic link not followed; `None` if nothing.
pub(crate) fn kind(path: &Path) -> Result<Option<std::fs::FileType>, ArchonError> {
    match std::fs::symlink_metadata(path) {
        Ok(found) => Ok(Some(found.file_type())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
    }
}

/// Remove whatever is at `path`: a directory with all it holds, a symbolic
/// link as the link itself. Nothing there is fine.
pub(crate) fn remove_entry(path: &Path) -> Result<(), ArchonError> {
    let removed = match kind(path)? {
        None => return Ok(()),
        Some(kind) if kind.is_dir() => std::fs::remove_dir_all(path),
        Some(_) => std::fs::remove_file(path),
    };
    removed.map_err(|e| ArchonError::io(format!("removing {}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a rewrite of the operator's, git fetches from and pushes to the
    /// URL itself, and the scratch repository asked is gone after.
    #[test]
    fn a_url_nobody_rewrites_is_where_git_goes() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().canonicalize().unwrap().join("sha1");
        make(&git_dir, "sha1").unwrap();
        let shadow = Shadow {
            objects: git_dir.join("objects"),
            config: sealed_config("sha1", true, None),
            git_dir,
        };
        let url = "/nowhere/rewritten/origin.git";
        assert_eq!(shadow.rewritten(url).unwrap(), [url, url]);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "only the shadow: {left:?}");
    }

    #[test]
    fn a_planted_config_or_attributes_file_is_gone_before_the_next_command() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().canonicalize().unwrap().join("sha1");
        make(&git_dir, "sha1").unwrap();
        let config = sealed_config("sha1", true, None);
        std::fs::write(git_dir.join("config"), "[filter \"x\"]\n\tsmudge = evil\n").unwrap();
        std::fs::create_dir_all(git_dir.join("info")).unwrap();
        std::fs::write(git_dir.join("info/attributes"), "* filter=x\n").unwrap();
        seal(&git_dir, &config).unwrap();
        assert_eq!(
            std::fs::read_to_string(git_dir.join("config")).unwrap(),
            config
        );
        assert!(!git_dir.join("info/attributes").exists());
        let link = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&git_dir, &link).unwrap();
        assert!(
            seal(&link, &config).is_err(),
            "a link is no git directory of ours"
        );
    }

    /// Deliveries in different repositories run at once and share the shadow
    /// of their object format, also on its very first use.
    #[test]
    fn many_first_uses_at_once_make_and_seal_one_shadow() {
        const AT_ONCE: usize = 8;
        for _ in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let git_dir = dir.path().canonicalize().unwrap().join("sha1");
            let config = sealed_config("sha1", true, None);
            let start = std::sync::Barrier::new(AT_ONCE);
            let results: Vec<Result<(), ArchonError>> = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..AT_ONCE)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            make(&git_dir, "sha1")?;
                            seal(&git_dir, &config)
                        })
                    })
                    .collect();
                workers.into_iter().map(|w| w.join().unwrap()).collect()
            });
            for result in results {
                result.expect("every first use succeeds");
            }
            assert_eq!(
                std::fs::read_to_string(git_dir.join("config")).unwrap(),
                config
            );
            let names: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, ["sha1"], "no half-made directory is left");
        }
    }
}
