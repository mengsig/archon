//! What the daemon keeps of a role's worktree it cannot reuse: a failure
//! after moving it aside still says where it went, nothing is moved
//! through a planted symlink, nothing kept names the git directory the
//! fresh worktree reuses, and a worktree deleted by hand keeps its git
//! directory.

use crate::common;

use std::path::{Path, PathBuf};
use std::process::Command;

use archon_core::error::ArchonError;
use archon_daemon::worktree::team::{GIT_SET_ASIDE_DIR, KeptWork, SET_ASIDE_DIR};
use common::team_worktree::{Run, plant_filter, team_run};
use common::{git, init_repo};

fn git_dir(tree: &Path) -> PathBuf {
    PathBuf::from(git(
        tree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    ))
}

/// `core.worktree` in the git config file `config`, if set.
fn config_worktree(config: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(config)
        .args(["--get", "core.worktree"])
        .output()
        .expect("git runs");
    assert!(output.status.code() == Some(0) || output.status.code() == Some(1));
    let value = String::from_utf8(output.stdout).expect("UTF-8");
    output.status.success().then(|| value.trim().to_owned())
}

/// A repository made at `tree/<name>` whose git directory is moved to
/// `git_dir` and named by a gitfile, as a role could do.
fn repository_at(tree: &Path, name: &str, git_dir: &Path) -> String {
    let sub = init_repo(&tree.join(name));
    let head = git(&sub, &["rev-parse", "HEAD"]);
    std::fs::create_dir_all(git_dir.parent().expect("parent")).expect("mkdir");
    std::fs::rename(sub.join(".git"), git_dir).expect("move .git");
    let gitfile = format!("gitdir: {}\n", git_dir.display());
    std::fs::write(sub.join(".git"), gitfile).expect("gitfile");
    head
}

/// The role's next attempt fails once its worktree is moved aside: the
/// error still says where the worktree and its git directory went.
#[test]
fn a_failure_after_moving_a_worktree_aside_says_where_it_went() {
    let r = team_run();
    let api = r.worker("api");
    plant_filter(&api, "evil", &r.marker);
    let next = r.branch("api", 2);
    git(&r.repo, &["branch", &next, &r.run.branch]);
    let elsewhere = r.dir.path().join("elsewhere");
    let elsewhere = elsewhere.to_str().expect("UTF-8");
    git(&r.repo, &["worktree", "add", "--quiet", elsewhere, &next]);

    let failed = r
        .ensure_kept("api", 2)
        .expect_err("the branch is checked out elsewhere");
    let text = failed.to_string();
    let root = r.run.path.parent().expect("item directory");
    let kept_at = root.join(SET_ASIDE_DIR).join("build-api-1");
    assert!(text.contains("Moved a role's worktree"), "{text}");
    assert!(text.contains(&kept_at.display().to_string()), "{text}");
    let git_dirs = r.repo.join(".git").join(GIT_SET_ASIDE_DIR);
    assert!(text.contains(&git_dirs.display().to_string()), "{text}");
    assert!(text.contains("already"), "git's own reason is kept: {text}");
    assert!(kept_at.join("api.txt").exists() && kept_at.join("evil/f.txt").exists());
    r.assert_filter_never_ran();
}

/// Where worktrees or git directories are moved aside is a symlink the
/// role planted: nothing is moved through it, and nothing moves at all.
#[test]
fn a_symlinked_set_aside_directory_is_refused_before_anything_moves() {
    for git_side in [false, true] {
        let r = team_run();
        let api = r.worker("api");
        plant_filter(&api, "evil", &r.marker);
        let target = r.dir.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        let root = r.run.path.parent().expect("item directory");
        let planted = match git_side {
            false => root.join(SET_ASIDE_DIR),
            true => r.repo.join(".git").join(GIT_SET_ASIDE_DIR),
        };
        std::os::unix::fs::symlink(&target, &planted).expect("symlink");
        let admin = git_dir(&api);

        let refused = r
            .ensure_kept("api", 2)
            .expect_err("a symlinked destination");
        assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
        assert!(refused.to_string().contains("symbolic link"), "{refused}");
        let moved: Vec<_> = std::fs::read_dir(&target).expect("read").collect();
        assert!(moved.is_empty(), "nothing moved through the symlink");
        assert!(api.join(".git").is_file() && admin.join("HEAD").is_file());
        assert_eq!(git_dir(&api), admin, "the worktree is where it was");
        r.assert_filter_never_ran();
    }
}

/// A submodule git must not enter whose gitfile names a git directory in
/// the worktree's own, and a stray git directory there whose work tree is
/// in the worktree: once moved aside, the one is named where it went and
/// has no work tree elsewhere, and the other names the kept work tree.
#[test]
fn nothing_kept_names_the_git_directory_the_fresh_worktree_reuses() {
    let r = team_run();
    let api = r.worker("api");
    let admin = git_dir(&api);
    let sneaky = admin.join("modules/sneaky");
    let head = repository_at(&api, "sneaky", &sneaky);
    let run_tree = r.run.path.to_str().expect("UTF-8");
    let config = sneaky.join("config");
    let config_arg = config.to_str().expect("UTF-8");
    git(
        &api,
        &["config", "--file", config_arg, "core.worktree", run_tree],
    );
    let link = format!("160000,{head},sneaky");
    git(&api, &["update-index", "--add", "--cacheinfo", &link]);
    git(&api, &["commit", "--quiet", "-m", "add sneaky"]);
    let stray = admin.join("modules/nested/stray");
    repository_at(r.dir.path(), "stray-source", &stray);
    let stray_config = stray.join("config");
    let stray_tree = api.join("stray-tree");
    std::fs::create_dir(&stray_tree).expect("mkdir");
    let args = [
        "config",
        "--file",
        stray_config.to_str().expect("UTF-8"),
        "core.worktree",
    ];
    git(
        &api,
        &[&args[..], &[stray_tree.to_str().expect("UTF-8")]].concat(),
    );

    let aside = r.set_aside("api", 2);
    assert_eq!(aside.submodules, ["sneaky"]);
    r.assert_fresh("api", 2, &aside);
    let kept_sneaky = aside.git_dir_kept_at.join("modules/sneaky");
    let gitfile = std::fs::read_to_string(aside.kept_at.join("sneaky/.git")).expect("gitfile");
    assert_eq!(gitfile, format!("gitdir: {}\n", kept_sneaky.display()));
    assert_eq!(
        config_worktree(&kept_sneaky.join("config")),
        None,
        "never the run's"
    );
    let kept_stray = aside.git_dir_kept_at.join("modules/nested/stray/config");
    let expected = aside.kept_at.join("stray-tree").display().to_string();
    assert_eq!(config_worktree(&kept_stray), Some(expected));
    assert_eq!(git_dir(&r.worker_path("api")), admin, "the name is reused");
    assert!(!admin.join("modules").exists(), "nothing of the old one");
}

/// `role`'s worktree deleted by hand, its next use on `attempt`.
fn orphaned(r: &Run, role: &str, attempt: u32) -> archon_daemon::worktree::team::OrphanedGitDir {
    std::fs::remove_dir_all(r.worker_path(role)).expect("rm -rf");
    match r.ensure_kept(role, attempt).expect("a fresh worktree") {
        Some(KeptWork::Orphaned(orphan)) => orphan,
        other => panic!("expected the git directory kept, got {other:?}"),
    }
}

/// A submodule `name` added in `tree` with git, and committed.
fn add_submodule(r: &Run, tree: &Path, name: &str) {
    let upstream = init_repo(&r.dir.path().join("upstream"));
    let url = upstream.to_str().expect("UTF-8");
    let add = ["-c", "protocol.file.allow=always", "submodule", "add"];
    git(tree, &[&add[..], &["--quiet", url, name]].concat());
    git(tree, &["commit", "--quiet", "-m", &format!("add {name}")]);
}

/// A role's worktree deleted by hand, on a detached `HEAD` with a
/// submodule added with git: its git directory is moved aside rather than
/// deleted, its commits are named, and a fresh worktree takes its place.
#[test]
fn a_worktree_deleted_by_hand_keeps_its_git_directory() {
    let r = team_run();
    let api = r.worker("api");
    add_submodule(&r, &api, "legit");
    git(&api, &["switch", "--quiet", "--detach"]);
    std::fs::write(api.join("detached.txt"), "only here\n").expect("write");
    git(&api, &["add", "detached.txt"]);
    git(&api, &["commit", "--quiet", "-m", "detached"]);
    let commit = git(&api, &["rev-parse", "HEAD"]);
    let admin = git_dir(&api);

    let orphan = orphaned(&r, "api", 1);
    assert_eq!(orphan.worktree, api);
    assert_eq!(orphan.head, format!("`refs/archon/saved/{commit}`"));
    let saved = git(
        &r.repo,
        &["rev-parse", &format!("refs/archon/saved/{commit}")],
    );
    assert_eq!(saved, commit, "its detached commit is kept");
    let git_dirs = r.repo.join(".git").join(GIT_SET_ASIDE_DIR);
    assert_eq!(orphan.git_dir_kept_at.parent(), Some(git_dirs.as_path()));
    assert!(orphan.git_dir_kept_at.join("index").is_file());
    let module = orphan.git_dir_kept_at.join("modules/legit");
    assert!(
        module.join("HEAD").is_file(),
        "the submodule's git directory is kept"
    );
    assert_eq!(
        config_worktree(&module.join("config")),
        None,
        "its work tree is gone"
    );
    assert!(
        orphan.describe().contains("deleted by hand"),
        "{}",
        orphan.describe()
    );

    assert_eq!(
        git(&api, &["symbolic-ref", "--short", "HEAD"]),
        r.branch("api", 1)
    );
    assert_eq!(git_dir(&api), admin, "git reused the name");
    assert!(!admin.join("modules").exists(), "nothing of the old one");
    let listed = git(&r.repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        listed.matches(api.to_str().expect("UTF-8")).count(),
        1,
        "{listed}"
    );
}

/// Deleted by hand on its branch: the branch keeps its commits, and the
/// next attempt forks afresh.
#[test]
fn a_worktree_deleted_by_hand_on_its_branch_names_it() {
    let r = team_run();
    r.worker("api");
    let orphan = orphaned(&r, "api", 2);
    assert_eq!(orphan.head, format!("`{}`", r.branch("api", 1)));
    let saved = git(&r.repo, &["for-each-ref", "refs/archon/saved/"]);
    assert_eq!(saved, "", "a branch needs no saved name");
    let api = r.worker_path("api");
    assert_eq!(
        git(&api, &["symbolic-ref", "--short", "HEAD"]),
        r.branch("api", 2)
    );
}
