//! A role, or a work stage, controls its worktree's `.git` file and can
//! swap the directory for a symlink: the daemon must then refuse to run
//! git there, and never commit, switch or merge in the checkout the
//! worktree was pointed at, be it the operator's own, the run worktree or
//! another role's. Nor do its refs sway the daemon: a tag named like a
//! branch, or `HEAD` pointed at a tag.

use crate::common;

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_daemon::worktree::team::{ensure_worker, merge_team};
use archon_daemon::worktree::{Worktree, WorktreeManager};
use archon_pipeline::team::{MergeOutcome, MergePlan, RoleBranch, worker_branch, worker_dir};
use common::{committed, git, init_repo};

/// A team run with two roles, `api` and `ui`, each with one commit, and
/// an untracked file in every checkout that a daemon commit would take.
struct Team {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    run: Worktree,
    api: PathBuf,
    ui: PathBuf,
    run_id: PipelineRunId,
}

fn team() -> Team {
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
    let run_id = PipelineRunId::new();
    let workers = ["api", "ui"].map(|role| {
        let path = manager
            .path_for("demo", &item)
            .join(worker_dir("build", role));
        let branch = worker_branch(&run.branch, &run_id, role, 1);
        ensure_worker(&repo, &run.path, &path, &branch, &run.branch).expect("worker");
        std::fs::write(path.join(format!("{role}.txt")), "work\n").expect("write");
        git(&path, &["add", "."]);
        git(&path, &["commit", "--quiet", "-m", role]);
        path
    });
    for checkout in [&repo, &run.path, &workers[0], &workers[1]] {
        std::fs::write(checkout.join("victim.txt"), "not the daemon's\n").expect("write");
    }
    let [api, ui] = workers;
    Team {
        _dir: dir,
        repo,
        run,
        api,
        ui,
        run_id,
    }
}

impl Team {
    fn branch(&self, role: &str, attempt: u32) -> String {
        worker_branch(&self.run.branch, &self.run_id, role, attempt)
    }

    fn plan(&self) -> MergePlan {
        MergePlan {
            stage_id: "build".into(),
            attempt: 1,
            into: self.run.branch.clone(),
            roles: ["api", "ui"]
                .map(|role| RoleBranch {
                    role: role.into(),
                    branch: self.branch(role, 1),
                })
                .to_vec(),
        }
    }

    /// Every ref, and the branch and status of each checkout but `api`'s.
    fn snapshot(&self) -> Vec<String> {
        snapshot(&self.repo, [&self.repo, &self.run.path, &self.ui])
    }

    /// As [`Team::snapshot`], with `api`'s checkout but not the run's.
    fn snapshot_without_run(&self) -> Vec<String> {
        snapshot(&self.repo, [&self.repo, &self.api, &self.ui])
    }

    /// Both ways into `tree`: its next attempt, and the merge. Each is
    /// refused, and neither changes any other checkout or ref.
    fn assert_refused(&self, tree: &Path, why: &str) {
        let before = self.snapshot();
        let next = self.branch("api", 2);
        let refused = ensure_worker(&self.repo, &self.run.path, tree, &next, &self.run.branch)
            .expect_err("a next attempt in a hijacked worktree");
        assert!(
            matches!(refused, ArchonError::Invalid(_)),
            "{why}: {refused:?}"
        );
        assert_eq!(self.snapshot(), before, "{why}: a next attempt changed git");

        let refused = merge_team(&self.repo, &self.run.path, &self.plan())
            .expect_err("a merge of a hijacked worktree");
        assert!(
            matches!(refused, ArchonError::Invalid(_)),
            "{why}: {refused:?}"
        );
        assert_eq!(self.snapshot(), before, "{why}: the merge changed git");
    }
}

fn snapshot(repo: &Path, checkouts: [&PathBuf; 3]) -> Vec<String> {
    let refs = ["for-each-ref", "--format=%(refname) %(objectname)"];
    let mut seen = vec![git(repo, &refs)];
    for checkout in checkouts {
        seen.push(git(checkout, &["rev-parse", "--abbrev-ref", "HEAD"]));
        seen.push(git(
            checkout,
            &["status", "--porcelain", "--untracked-files=all"],
        ));
    }
    seen
}

fn git_dir(tree: &Path) -> String {
    git(tree, &["rev-parse", "--path-format=absolute", "--git-dir"])
}

fn point_gitfile(tree: &Path, git_dir: &str) {
    std::fs::write(tree.join(".git"), format!("gitdir: {git_dir}\n")).expect("rewrite .git");
}

/// The gitfile names the repository's own git directory: git would run
/// in the operator's checkout, on its branch and index.
#[test]
fn a_worker_pointed_at_the_repository_is_refused() {
    let t = team();
    let common = t.repo.join(".git");
    point_gitfile(&t.api, common.to_str().expect("UTF-8"));
    t.assert_refused(&t.api, "the repository's .git");
    assert_eq!(git(&t.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
}

#[test]
fn a_worker_pointed_at_the_run_worktree_is_refused() {
    let t = team();
    point_gitfile(&t.api, &git_dir(&t.run.path));
    t.assert_refused(&t.api, "main's admin dir");
}

#[test]
fn a_worker_pointed_at_another_role_is_refused() {
    let t = team();
    point_gitfile(&t.api, &git_dir(&t.ui));
    t.assert_refused(&t.api, "ui's admin dir");
}

/// The worker directory replaced by a symlink to another checkout: git
/// would find that checkout's own `.git` there, registered for it.
#[test]
fn a_symlinked_worker_is_refused() {
    let t = team();
    std::fs::remove_dir_all(&t.api).expect("remove");
    for target in [&t.ui, &t.run.path, &t.repo] {
        std::os::unix::fs::symlink(target, &t.api).expect("symlink");
        t.assert_refused(&t.api, &format!("a symlink to {}", target.display()));
        std::fs::remove_file(&t.api).expect("unlink");
    }
}

/// A symlinked `.git` inside the worker, to another worktree's gitfile.
#[test]
fn a_symlinked_gitfile_is_refused() {
    let t = team();
    std::fs::remove_file(t.api.join(".git")).expect("remove");
    std::os::unix::fs::symlink(t.ui.join(".git"), t.api.join(".git")).expect("symlink");
    t.assert_refused(&t.api, "a symlinked .git");
}

/// A copy of `run`'s administrative directory inside `run` itself, on the
/// run branch and sharing the repository: every other check passes there.
fn forge_admin(run: &Path, common: &Path) -> String {
    let real = PathBuf::from(git_dir(run));
    let forged = run.join(".forged");
    std::fs::create_dir(&forged).expect("mkdir");
    for file in ["HEAD", "index", "gitdir"] {
        std::fs::copy(real.join(file), forged.join(file)).expect("copy");
    }
    let common = common.to_str().expect("UTF-8");
    std::fs::write(forged.join("commondir"), format!("{common}\n")).expect("write");
    forged.to_str().expect("UTF-8").to_owned()
}

/// A work stage pointed the run worktree elsewhere: at the operator's
/// checkout, another role's, or a forged copy of its own administrative
/// directory. Neither a role's fork nor the merge runs git there.
#[test]
fn a_run_worktree_pointed_elsewhere_is_refused() {
    let t = team();
    let common = t.repo.join(".git");
    let ui_admin = git_dir(&t.ui);
    let forged = forge_admin(&t.run.path, &common);
    for target in [common.to_str().expect("UTF-8"), &ui_admin, &forged] {
        let original = std::fs::read_to_string(t.run.path.join(".git")).expect("read");
        point_gitfile(&t.run.path, target);
        let before = t.snapshot_without_run();
        let fresh = t.api.parent().expect("item directory").join("build-db");
        let branch = t.branch("db", 1);
        let refused = ensure_worker(&t.repo, &t.run.path, &fresh, &branch, &t.run.branch)
            .expect_err("a fork from a hijacked run worktree");
        assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
        let refused = merge_team(&t.repo, &t.run.path, &t.plan())
            .expect_err("a merge into a hijacked run worktree");
        assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
        assert_eq!(t.snapshot_without_run(), before, "pointed at {target}");
        assert!(!fresh.exists(), "no role forked");
        std::fs::write(t.run.path.join(".git"), original).expect("restore");
    }
}

/// The checks let the real layout through, `worktree.useRelativePaths`
/// included, and moving on still saves what the role left.
#[test]
fn registered_worktrees_pass_with_relative_paths_too() {
    let t = team();
    git(&t.repo, &["config", "worktree.useRelativePaths", "true"]);
    git(
        &t.repo,
        &["worktree", "repair", t.api.to_str().expect("UTF-8")],
    );
    let gitfile = std::fs::read_to_string(t.api.join(".git")).expect("read");
    assert!(gitfile.starts_with("gitdir: ../"), "{gitfile}");

    let next = t.branch("api", 2);
    let saved = committed(
        ensure_worker(&t.repo, &t.run.path, &t.api, &next, &t.run.branch)
            .expect("a registered worktree moves on"),
    );
    assert_eq!(saved.files, ["victim.txt"]);
    assert_eq!(saved.on, t.branch("api", 1));
    assert_eq!(git(&t.api, &["rev-parse", "--abbrev-ref", "HEAD"]), next);
}

/// A role checked out a branch that is not its own, here the operator's
/// `main` while the operator works on another: what it left is committed
/// on a detached `HEAD` kept under `refs/archon/saved/`, and `main` and
/// the operator's checkout never move.
#[test]
fn leftovers_on_a_branch_not_the_role_s_never_move_it() {
    let t = team();
    git(&t.repo, &["switch", "--quiet", "-c", "feature"]);
    let main = git(&t.repo, &["rev-parse", "refs/heads/main"]);
    git(&t.api, &["switch", "--quiet", "main"]);
    std::fs::write(t.api.join("stray.env"), "SECRET=1\n").expect("write");
    let repo_before = git(&t.repo, &["status", "--porcelain", "--untracked-files=all"]);

    let next = t.branch("api", 2);
    let saved = committed(
        ensure_worker(&t.repo, &t.run.path, &t.api, &next, &t.run.branch)
            .expect("the next attempt"),
    );
    assert_eq!(git(&t.repo, &["rev-parse", "refs/heads/main"]), main);
    assert_eq!(
        git(&t.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feature"
    );
    let repo_after = git(&t.repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert_eq!(repo_after, repo_before);
    assert!(saved.on.contains("off `main`"), "{}", saved.on);
    assert_eq!(saved.files, ["stray.env", "victim.txt"]);
    let kept = format!("refs/archon/saved/{}", saved.commit);
    assert_eq!(git(&t.repo, &["rev-parse", &kept]), saved.commit);
    let files = git(&t.repo, &["ls-tree", "--name-only", &saved.commit]);
    assert!(files.lines().any(|file| file == "stray.env"), "{files}");
    assert_eq!(git(&t.api, &["rev-parse", "--abbrev-ref", "HEAD"]), next);
}

/// Another role's branch is not this role's own either.
#[test]
fn leftovers_on_another_role_s_branch_never_move_it() {
    let t = team();
    let ui_branch = t.branch("ui", 1);
    git(&t.ui, &["switch", "--quiet", "--detach"]);
    let ui_head = git(&t.repo, &["rev-parse", &ui_branch]);
    git(&t.api, &["switch", "--quiet", &ui_branch]);

    let next = t.branch("api", 2);
    let saved = committed(
        ensure_worker(&t.repo, &t.run.path, &t.api, &next, &t.run.branch)
            .expect("the next attempt"),
    );
    assert_eq!(git(&t.repo, &["rev-parse", &ui_branch]), ui_head);
    assert!(saved.on.starts_with("a detached HEAD"), "{}", saved.on);
}

/// `ensure_worker` makes worktrees only for worker branches of the run.
#[test]
fn a_branch_that_is_no_worker_s_is_refused() {
    let t = team();
    let path = t.api.parent().expect("item directory").join("build-db");
    for branch in ["main", "feature", t.run.branch.as_str()] {
        let refused = ensure_worker(&t.repo, &t.run.path, &path, branch, &t.run.branch)
            .expect_err("not a worker branch");
        assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    }
    assert!(!path.exists());
}

/// A role tagged its commit with its branch's own name: `rev-parse
/// --abbrev-ref` would then say `heads/<branch>`. The worktree still counts
/// as on its branch, so nothing is committed or moved, and it merges.
#[test]
fn a_tag_named_like_the_role_s_branch_changes_nothing() {
    let t = team();
    let branch = t.branch("api", 1);
    git(&t.api, &["tag", &branch]);
    let before = git(&t.api, &["rev-parse", "HEAD"]);

    let kept = ensure_worker(&t.repo, &t.run.path, &t.api, &branch, &t.run.branch)
        .expect("the same attempt");
    assert_eq!(kept, None, "a worktree on its branch stays as it is");
    assert_eq!(git(&t.api, &["rev-parse", "HEAD"]), before);
    assert!(t.api.join("victim.txt").exists(), "nothing was committed");

    for tree in [&t.api, &t.run.path] {
        std::fs::remove_file(tree.join("victim.txt")).expect("remove");
    }
    let merged = merge_team(&t.repo, &t.run.path, &t.plan()).expect("merge");
    assert_eq!(merged.outcome, MergeOutcome::Merged);
    assert!(t.run.path.join("api.txt").exists());
    let next = t.branch("api", 2);
    ensure_worker(&t.repo, &t.run.path, &t.api, &next, &t.run.branch).expect("next attempt");
    assert_eq!(git(&t.api, &["symbolic-ref", "--short", "HEAD"]), next);
}

/// A tag named like the run branch, on another commit: new roles fork from
/// the run branch, whether added or switched, never from the tag.
#[test]
fn a_tag_named_like_the_run_branch_is_never_forked() {
    let t = team();
    std::fs::write(t.run.path.join("plan.md"), "plan\n").expect("write");
    git(&t.run.path, &["add", "plan.md"]);
    git(&t.run.path, &["commit", "--quiet", "-m", "plan"]);
    git(&t.run.path, &["tag", &t.run.branch, "main"]);
    let tip = git(
        &t.repo,
        &["rev-parse", &format!("refs/heads/{}", t.run.branch)],
    );

    let db = t.api.parent().expect("item directory").join("build-db");
    let branch = t.branch("db", 1);
    ensure_worker(&t.repo, &t.run.path, &db, &branch, &t.run.branch).expect("a new role");
    assert_eq!(
        git(&db, &["rev-parse", "HEAD"]),
        tip,
        "added from the branch"
    );

    let next = t.branch("api", 2);
    ensure_worker(&t.repo, &t.run.path, &t.api, &next, &t.run.branch).expect("next attempt");
    assert_eq!(
        git(&t.api, &["rev-parse", "HEAD"]),
        tip,
        "switched from the branch"
    );
}

/// A role pointed `HEAD` at a tag: committing there would move the tag, so
/// `HEAD` is detached from it first, as from a branch not the role's.
#[test]
fn leftovers_on_a_head_pointed_at_a_tag_never_move_it() {
    let t = team();
    git(&t.api, &["tag", "release"]);
    let tagged = git(&t.repo, &["rev-parse", "refs/tags/release"]);
    git(&t.api, &["symbolic-ref", "HEAD", "refs/tags/release"]);

    let next = t.branch("api", 2);
    let saved = committed(
        ensure_worker(&t.repo, &t.run.path, &t.api, &next, &t.run.branch)
            .expect("the next attempt"),
    );
    assert_eq!(git(&t.repo, &["rev-parse", "refs/tags/release"]), tagged);
    assert!(saved.on.contains("off `refs/tags/release`"), "{}", saved.on);
    assert_eq!(saved.files, ["victim.txt"]);
    assert_eq!(git(&t.api, &["symbolic-ref", "--short", "HEAD"]), next);
}

/// A branch named like a role's next attempt that does not hold the run
/// branch is no fork of this run's: it is refused whether the role's
/// worktree is there or not, and nothing changes.
#[test]
fn a_worker_branch_without_the_run_branch_is_never_reused() {
    let t = team();
    git(
        &t.run.path,
        &["commit", "--quiet", "--allow-empty", "-m", "ahead"],
    );
    let root = t.api.parent().expect("item directory");
    let missing = root.join(worker_dir("build", "db"));
    for (tree, role) in [(&t.api, "api"), (&missing, "db")] {
        let stale = t.branch(role, 2);
        git(&t.repo, &["branch", &stale, "main"]);
        let before = t.snapshot();
        let refused = ensure_worker(&t.repo, &t.run.path, tree, &stale, &t.run.branch)
            .expect_err("a branch that is not this run's");
        assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
        assert!(
            refused.to_string().contains("does not hold the run branch"),
            "{refused}"
        );
        assert_eq!(t.snapshot(), before, "{role}: nothing changed");
    }
    assert!(!missing.exists(), "no worktree was added");
}
