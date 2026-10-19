//! A team run on a real repository for tests of role worktrees moved
//! aside, with a planted filter that must never run as the daemon.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_daemon::worktree::team::{GIT_SET_ASIDE_DIR, KeptWork, SetAside, ensure_worker};
use archon_daemon::worktree::{Worktree, WorktreeManager};
use archon_pipeline::team::{MergePlan, RoleBranch, worker_branch, worker_dir};

use super::{git, init_repo};

pub struct Run {
    pub dir: tempfile::TempDir,
    pub repo: PathBuf,
    pub manager: WorktreeManager,
    pub item: QueueItemId,
    pub run: Worktree,
    pub run_id: PipelineRunId,
    /// Created by the filter's command, which must never run.
    pub marker: PathBuf,
}

pub fn team_run() -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    let repo = init_repo(&root.join("repo"));
    let manager = WorktreeManager::new(root.join("worktrees"), "archon/".into());
    let item = QueueItemId::new();
    let run = manager
        .create_team_run(
            &repo,
            "demo",
            &item,
            &manager.branch_name(&item, "team", 0),
            None,
        )
        .expect("team run");
    Run {
        dir,
        repo,
        manager,
        item,
        run,
        run_id: PipelineRunId::new(),
        marker: root.join("filter-ran"),
    }
}

impl Run {
    pub fn branch(&self, role: &str, attempt: u32) -> String {
        worker_branch(&self.run.branch, &self.run_id, role, attempt)
    }

    pub fn worker_path(&self, role: &str) -> PathBuf {
        let root = self.run.path.parent().expect("item directory");
        root.join(worker_dir("build", role))
    }

    pub fn ensure(&self, role: &str, attempt: u32) -> Result<PathBuf, ArchonError> {
        self.ensure_kept(role, attempt)
            .map(|_| self.worker_path(role))
    }

    pub fn ensure_kept(&self, role: &str, attempt: u32) -> Result<Option<KeptWork>, ArchonError> {
        let path = self.worker_path(role);
        let branch = self.branch(role, attempt);
        ensure_worker(&self.repo, &self.run.path, &path, &branch, &self.run.branch)
    }

    /// `role`'s next attempt, which must move its worktree aside.
    pub fn set_aside(&self, role: &str, attempt: u32) -> SetAside {
        match self.ensure_kept(role, attempt).expect("the next attempt") {
            Some(KeptWork::SetAside(aside)) => aside,
            other => panic!("expected the worktree set aside, got {other:?}"),
        }
    }

    /// `role`'s fresh worktree on its `attempt`'s branch, forked from the
    /// run branch, no longer registered where it was moved aside.
    pub fn assert_fresh(&self, role: &str, attempt: u32, aside: &SetAside) {
        let path = self.worker_path(role);
        assert_eq!(aside.worktree, path);
        let checked_out = git(&path, &["symbolic-ref", "--short", "HEAD"]);
        assert_eq!(checked_out, self.branch(role, attempt));
        let run_head = git(&self.run.path, &["rev-parse", "HEAD"]);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), run_head);
        let listed = git(&self.repo, &["worktree", "list", "--porcelain"]);
        let kept = aside.kept_at.display().to_string();
        assert!(!listed.contains(&kept), "{listed}");
        assert!(!aside.kept_at.join(".git").exists(), "not a worktree now");
        let common = self.repo.join(".git");
        let git_dirs = common.join(GIT_SET_ASIDE_DIR);
        assert_eq!(aside.git_dir_kept_at.parent(), Some(git_dirs.as_path()));
        assert!(
            aside.git_dir_kept_at.join("HEAD").is_file(),
            "its git directory is kept"
        );
        let described = aside.describe();
        let named = aside.git_dir_kept_at.display().to_string();
        assert!(described.contains(&named), "{described}");
    }

    /// `role`'s worktree, attempt 1, with one commit of its own.
    pub fn worker(&self, role: &str) -> PathBuf {
        let path = self.ensure(role, 1).expect("worker");
        std::fs::write(path.join(format!("{role}.txt")), "work\n").expect("write");
        git(&path, &["add", "."]);
        git(&path, &["commit", "--quiet", "-m", role]);
        path
    }

    pub fn plan(&self, roles: &[&str]) -> MergePlan {
        MergePlan {
            stage_id: "build".into(),
            attempt: 1,
            into: self.run.branch.clone(),
            roles: roles
                .iter()
                .map(|role| RoleBranch {
                    role: (*role).into(),
                    branch: self.branch(role, 1),
                })
                .collect(),
        }
    }

    pub fn assert_filter_never_ran(&self) {
        assert!(
            !self.marker.exists(),
            "the planted filter ran as the daemon"
        );
    }
}

/// A repository at `tree/<name>` whose tracked `f.txt` has a clean filter
/// creating `marker`, committed on `tree`'s branch as a gitlink, and its
/// file's time moved so that git would run the filter to compare it.
pub fn plant_filter(tree: &Path, name: &str, marker: &Path) -> PathBuf {
    let sub = init_repo(&tree.join(name));
    std::fs::write(sub.join(".gitattributes"), "*.txt filter=evil\n").expect("write");
    std::fs::write(sub.join("f.txt"), "f\n").expect("write");
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "--quiet", "-m", "filtered"]);
    let command = format!("touch '{}'; cat", marker.display());
    git(&sub, &["config", "filter.evil.clean", &command]);
    let head = git(&sub, &["rev-parse", "HEAD"]);
    let link = format!("160000,{head},{name}");
    git(tree, &["update-index", "--add", "--cacheinfo", &link]);
    git(tree, &["commit", "--quiet", "-m", "add a gitlink"]);
    let later = SystemTime::now() + Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(sub.join("f.txt"))
        .and_then(|file| file.set_modified(later))
        .expect("touch");
    sub
}

/// The same repository with its git directory moved beside it inside
/// `tree`, named by a gitfile: still the role's, not the repository's.
pub fn behind_gitfile(tree: &Path, sub: &Path) {
    let hidden = tree.join(".hidden-git");
    std::fs::rename(sub.join(".git"), &hidden).expect("move .git");
    std::fs::write(sub.join(".git"), "gitdir: ../.hidden-git\n").expect("gitfile");
}
