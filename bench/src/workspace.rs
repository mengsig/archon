//! Trial directories (ARCHITECTURE §15). What an agent can reach is kept
//! apart from what only the harness may see:
//!
//! ```text
//! <work>/<stamp>/                 mode 0711: agents cannot list it
//!   <random>/                     the trial workspace: only the fixtures
//!     <repo>/                     each fixture as its own git repository, tagged bench-base
//!     .archon/                    archon arm only: its private archond's config/ and data/
//!   <random>/                     the grading copy, created after the agent stopped
//! <results>/<stamp>/trials/<task>/<arm>/<trial>/logs/
//!                                 arm output, answer.txt, grade output, result patches
//! ```
//!
//! Hidden tests only ever land in the grading copy, and the unguessable names
//! keep one trial from finding another's workspace or grading copy.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};

use crate::task::{BenchTask, Fixture};

/// The tag every fixture's initial commit carries; graders diff against it.
pub const BASE_TAG: &str = "bench-base";
/// Under a trial's logs: the logs of the extra grading of timed-out work,
/// apart from the verdict's so no fixture name can collide with them.
pub const TIMEOUT_LOG_DIR: &str = "timeout";
/// The branch fixtures start on (the archon arm registers it as the base).
pub const BASE_BRANCH: &str = "main";
/// Build output and caches never worth copying between trial directories.
const SKIPPED_NAMES: &[&str] = &["target", "node_modules", "__pycache__", ".pytest_cache"];
/// Traverse-only: a trial can enter its own directory by name but not list
/// its siblings.
const RUN_DIR_MODE: u32 = 0o711;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialLayout {
    /// Agent-visible: the fixture repositories and nothing else.
    pub workspace: PathBuf,
    /// Harness-only: the grading copy; must not exist before grading.
    pub grade: PathBuf,
    /// Harness-only: logs, answer and grader output.
    pub logs: PathBuf,
    /// The archon arm's private daemon state, inside the workspace so its
    /// worktrees see only this trial.
    pub archon: PathBuf,
}

impl TrialLayout {
    pub fn new(workspace: PathBuf, grade: PathBuf, logs: PathBuf) -> Self {
        assert!(
            !grade.starts_with(&workspace) && !logs.starts_with(&workspace),
            "harness directories must stay outside the agent workspace"
        );
        Self {
            archon: workspace.join(".archon"),
            workspace,
            grade,
            logs,
        }
    }

    /// `workspace/`, `grade/` and `logs/` side by side under `root`, for
    /// grader checks and tests that no agent ever sees.
    pub fn under(root: &Path) -> Self {
        Self::new(
            root.join("workspace"),
            root.join("grade"),
            root.join("logs"),
        )
    }

    /// A trial of a real run: unguessable workspace and grading names under
    /// the run's traverse-only directory, logs with the results.
    pub fn for_trial(run_dir: &Path, logs: PathBuf) -> anyhow::Result<Self> {
        ensure!(run_dir.is_dir(), "{} is not a directory", run_dir.display());
        let (workspace, grade) = (unguessable_name()?, unguessable_name()?);
        ensure!(workspace != grade, "two random names collided");
        Ok(Self::new(
            run_dir.join(workspace),
            run_dir.join(grade),
            logs,
        ))
    }

    /// This trial for the extra grading of timed-out work: the same copy,
    /// workspace and answer, logs under [`TIMEOUT_LOG_DIR`] so none collide.
    pub fn late_grade(&self) -> anyhow::Result<Self> {
        ensure!(
            self.logs.is_dir(),
            "{} is not a directory",
            self.logs.display()
        );
        let logs = self.logs.join(TIMEOUT_LOG_DIR);
        std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
        let late = Self::new(self.workspace.clone(), self.grade.clone(), logs);
        let answer = self.answer_file();
        if answer.exists() {
            std::fs::copy(&answer, late.answer_file())
                .with_context(|| format!("copying {}", answer.display()))?;
        }
        Ok(late)
    }

    pub fn repo(&self, fixture: &Fixture) -> PathBuf {
        self.workspace.join(&fixture.name)
    }

    /// Where a single agent starts: the repository itself, or the workspace
    /// holding every repository of a multi-repo task.
    pub fn agent_cwd(&self, task: &BenchTask) -> PathBuf {
        if task.is_multi_repo() {
            return self.workspace.clone();
        }
        let fixtures = task.fixtures();
        assert_eq!(fixtures.len(), 1, "a single-repo task has one fixture");
        self.repo(&fixtures[0])
    }

    pub fn answer_file(&self) -> PathBuf {
        self.logs.join("answer.txt")
    }
}

/// 24 hex characters from the kernel's random source.
pub fn unguessable_name() -> anyhow::Result<String> {
    let mut bytes = [0u8; 12];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(name.len(), 24, "two hex digits per byte");
    Ok(name)
}

/// Create a run's work directory, traverse-only so trials cannot list it.
pub fn create_run_dir(dir: &Path) -> anyhow::Result<()> {
    ensure!(
        !dir.exists(),
        "{} already exists; every run gets a fresh work directory",
        dir.display()
    );
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(RUN_DIR_MODE))
        .with_context(|| format!("restricting {}", dir.display()))?;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    ensure!(mode == RUN_DIR_MODE, "{} has mode {mode:o}", dir.display());
    Ok(())
}

/// Create the workspace with every fixture committed and tagged, and the
/// logs directory. Refuses to reuse either, so no trial sees leftovers.
pub fn prepare(task: &BenchTask, layout: &TrialLayout) -> anyhow::Result<()> {
    for dir in [&layout.workspace, &layout.logs, &layout.grade] {
        ensure!(!dir.exists(), "{} already exists", dir.display());
    }
    for dir in [&layout.workspace, &layout.logs] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    for fixture in task.fixtures() {
        ensure!(
            !fixture.dir.join(".git").exists(),
            "{}: fixtures must not carry git history",
            fixture.dir.display()
        );
        let repo = layout.repo(&fixture);
        copy_tree(&fixture.dir, &repo)?;
        init_repo(&repo)?;
    }
    Ok(())
}

/// `git init` on `main`, one commit, tag `bench-base`. The commit identity is
/// fixed so fixture history is identical across machines, and signing and
/// hooks are off so an operator's global git config cannot break setup.
pub fn init_repo(repo: &Path) -> anyhow::Result<()> {
    assert!(repo.is_dir(), "init_repo needs an existing directory");
    git(repo, &["init", "-q", "-b", BASE_BRANCH])?;
    git(repo, &["add", "-A"])?;
    git(
        repo,
        &[
            "commit",
            "-q",
            "--no-verify",
            "--allow-empty",
            "-m",
            "bench fixture",
        ],
    )?;
    git(repo, &["tag", BASE_TAG])?;
    let head = git(repo, &["rev-parse", "HEAD"])?;
    ensure!(
        !head.trim().is_empty(),
        "{}: no HEAD after commit",
        repo.display()
    );
    Ok(())
}

/// Run git in `dir` with the bench's fixed identity; stdout on success.
pub fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=archon-bench",
            "-c",
            "user.email=archon-bench@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
        .output()
        .with_context(|| format!("running git {args:?} in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "git {args:?} in {} failed ({}): {}",
            dir.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Recursive copy that keeps `.git` and symlinks but skips build output.
pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    ensure!(src.is_dir(), "{} is not a directory", src.display());
    ensure!(
        !dst.starts_with(src),
        "refusing to copy {} into itself",
        src.display()
    );
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if SKIPPED_NAMES.iter().any(|s| name == *s) {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)
                .with_context(|| format!("linking {}", to.display()))?;
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Delete the trial workspace `dir`. Archon leaves its orchestrator's
/// mirrors read-only, so each directory is opened to its owner first.
pub fn remove_workspace(dir: &Path) -> anyhow::Result<()> {
    ensure!(dir.is_absolute(), "{} is not absolute", dir.display());
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let meta = std::fs::symlink_metadata(&next)
            .with_context(|| format!("inspecting {}", next.display()))?;
        if !meta.file_type().is_dir() {
            continue;
        }
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            std::fs::set_permissions(&next, std::fs::Permissions::from_mode(mode | 0o700))
                .with_context(|| format!("opening {}", next.display()))?;
        }
        for entry in
            std::fs::read_dir(&next).with_context(|| format!("reading {}", next.display()))?
        {
            pending.push(entry?.path());
        }
    }
    std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn a_workspace_with_read_only_mirrors_is_removed() {
        let tmp = scratch();
        let dir = tmp.path().join("trial");
        let mirror = dir.join(".archon/data/orchestrator/repos/demo/src");
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::write(mirror.join("lib.py"), "").unwrap();
        for locked in [&mirror, mirror.parent().unwrap()] {
            std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        assert!(
            std::fs::remove_dir_all(&dir).is_err(),
            "plain removal fails"
        );
        remove_workspace(&dir).unwrap();
        assert!(!dir.exists());
    }

    pub(crate) fn scratch() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join(".work");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("ws-test-")
            .tempdir_in(base)
            .unwrap()
    }

    fn bundled(id: &str) -> BenchTask {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks");
        BenchTask::load(&root.join(id)).unwrap()
    }

    #[test]
    fn prepare_commits_and_tags_every_fixture() {
        let tmp = scratch();
        let task = bundled("fix-off-by-one");
        let layout = TrialLayout::under(&tmp.path().join("t1"));
        prepare(&task, &layout).unwrap();
        let repo = layout.agent_cwd(&task);
        assert!(repo.join("window.py").is_file());
        assert_eq!(git(&repo, &["status", "--porcelain"]).unwrap(), "");
        let tagged = git(&repo, &["rev-parse", BASE_TAG]).unwrap();
        let head = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(tagged, head);
        let branch = git(&repo, &["branch", "--show-current"]).unwrap();
        assert_eq!(branch.trim(), BASE_BRANCH);
        assert!(prepare(&task, &layout).is_err(), "never reuses a trial dir");
    }

    #[test]
    fn the_workspace_holds_only_the_fixture_and_one_commit() {
        let tmp = scratch();
        for task in
            BenchTask::load_all(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks")).unwrap()
        {
            let layout = TrialLayout::under(&tmp.path().join(&task.file.id));
            prepare(&task, &layout).unwrap();
            let mut names: Vec<String> = std::fs::read_dir(&layout.workspace)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let mut expected: Vec<String> = task.fixtures().into_iter().map(|f| f.name).collect();
            expected.sort();
            assert_eq!(names, expected, "{}: only the repositories", task.file.id);
            for fixture in task.fixtures() {
                let repo = layout.repo(&fixture);
                let log = git(&repo, &["rev-list", "--all"]).unwrap();
                assert_eq!(log.lines().count(), 1, "{}: one commit", repo.display());
                let files = git(&repo, &["ls-files"]).unwrap();
                for secret in ["hidden/", "reference.", "rubric.json", "grade.sh"] {
                    assert!(
                        !files.contains(secret),
                        "{}: {secret} leaked",
                        repo.display()
                    );
                }
            }
        }
    }

    #[test]
    fn multi_repo_tasks_get_one_repository_each() {
        let tmp = scratch();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks");
        let task = BenchTask::load_all(&root)
            .unwrap()
            .into_iter()
            .find(BenchTask::is_multi_repo)
            .expect("the suite has a multi-repo task");
        let layout = TrialLayout::under(&tmp.path().join("t"));
        prepare(&task, &layout).unwrap();
        assert_eq!(layout.agent_cwd(&task), layout.workspace);
        for fixture in task.fixtures() {
            let repo = layout.repo(&fixture);
            assert!(repo.join(".git").is_dir(), "{}", repo.display());
            git(&repo, &["rev-parse", BASE_TAG]).unwrap();
        }
    }

    #[test]
    fn trial_names_are_unguessable_and_the_run_dir_unlistable() {
        let tmp = scratch();
        let run = tmp.path().join("run");
        create_run_dir(&run).unwrap();
        let mode = std::fs::metadata(&run).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, RUN_DIR_MODE);
        assert!(create_run_dir(&run).is_err(), "a run dir is never reused");
        let a = TrialLayout::for_trial(&run, tmp.path().join("logs-a")).unwrap();
        let b = TrialLayout::for_trial(&run, tmp.path().join("logs-b")).unwrap();
        assert_ne!(a.workspace, b.workspace);
        assert_eq!(a.workspace.parent(), Some(run.as_path()));
        assert_eq!(a.grade.parent(), Some(run.as_path()));
        assert!(a.archon.starts_with(&a.workspace));
        let name = a
            .workspace
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.len() == 24 && name.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn copy_tree_skips_build_output_and_keeps_links() {
        let tmp = scratch();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("target/debug")).unwrap();
        std::fs::create_dir_all(src.join("pkg")).unwrap();
        std::fs::write(src.join("pkg/a.txt"), "a").unwrap();
        std::fs::write(src.join("target/debug/big"), "x").unwrap();
        std::os::unix::fs::symlink("pkg/a.txt", src.join("link")).unwrap();
        let dst = tmp.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("pkg/a.txt")).unwrap(), "a");
        assert!(!dst.join("target").exists());
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            Path::new("pkg/a.txt")
        );
        assert!(copy_tree(&src, &src.join("inner")).is_err());
    }
}
