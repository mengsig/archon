//! A role, or a work stage, can populate a submodule whose git directory
//! it made itself, with a filter naming any command: `status`, `diff` and
//! `add` would run it as the daemon. The daemon must refuse to run git
//! that enters such a submodule, and the command must never run. A role's
//! next attempt still starts: its worktree is moved aside unentered.

mod common;

use std::path::Path;

use archon_core::error::ArchonError;
use archon_daemon::worktree::team::{SET_ASIDE_DIR, merge_team};
use archon_pipeline::team::MergeOutcome;
use common::team_worktree::{behind_gitfile, plant_filter, team_run};
use common::{git, init_repo};

/// A role planted the submodule in its own worktree: the merge refuses
/// the role by name, the next attempt moves its worktree aside and starts
/// afresh, and nothing ran.
#[test]
fn a_role_s_own_submodule_repository_is_refused_then_set_aside() {
    for gitfile in [false, true] {
        let r = team_run();
        let api = r.worker("api");
        let sub = plant_filter(&api, "evil", &r.marker);
        if gitfile {
            behind_gitfile(&api, &sub);
        }
        let before = git(&r.repo, &["for-each-ref"]);

        let merged = merge_team(&r.repo, &r.run.path, &r.plan(&["api"])).expect("an outcome");
        let MergeOutcome::Refused { role, reason } = merged.outcome else {
            panic!("expected a refusal, got {:?}", merged.outcome);
        };
        assert_eq!(role, "api");
        assert!(reason.contains(": evil. The next attempt"), "{reason}");
        assert!(merged.saved.is_empty(), "nothing is committed");
        r.assert_filter_never_ran();

        let aside = r.set_aside("api", 2);
        r.assert_filter_never_ran();
        assert_eq!(aside.submodules, ["evil"]);
        assert_eq!(aside.head, format!("`{}`", r.branch("api", 1)));
        let root = r.run.path.parent().expect("item directory");
        assert_eq!(aside.kept_at, root.join(SET_ASIDE_DIR).join("build-api-1"));
        assert!(aside.kept_at.join("api.txt").exists(), "files are kept");
        assert!(aside.kept_at.join("evil/f.txt").exists());
        r.assert_fresh("api", 2, &aside);
        let refs = git(&r.repo, &["for-each-ref"]);
        let new_branch = format!("refs/heads/{}", r.branch("api", 2));
        let old: Vec<&str> = refs.lines().filter(|l| !l.ends_with(&new_branch)).collect();
        assert_eq!(old.join("\n"), before, "no other ref moved");

        std::fs::write(r.worker_path("api").join("again.txt"), "again\n").expect("write");
        let mut plan = r.plan(&["api"]);
        plan.roles[0].branch = r.branch("api", 2);
        let merged = merge_team(&r.repo, &r.run.path, &plan).expect("merge");
        assert_eq!(merged.outcome, MergeOutcome::Merged);
        assert!(r.run.path.join("again.txt").exists());
        r.assert_filter_never_ran();
    }
}

/// An agent replaced in the same attempt keeps the worktree as it is: no
/// git that enters the submodule runs, and nothing is moved.
#[test]
fn a_retried_agent_keeps_a_worktree_with_its_own_submodule() {
    let r = team_run();
    let api = r.worker("api");
    plant_filter(&api, "evil", &r.marker);
    let kept = r.ensure_kept("api", 1).expect("the same attempt");
    assert_eq!(kept, None);
    assert!(api.join("evil/f.txt").exists(), "left where it was");
    assert!(!api.parent().expect("item").join(SET_ASIDE_DIR).exists());
    r.assert_filter_never_ran();
}

/// A worktree set aside on a detached `HEAD`: its commits, on no branch,
/// are kept under `refs/archon/saved/` before its git directory is moved.
#[test]
fn a_detached_head_set_aside_is_kept() {
    let r = team_run();
    let api = r.worker("api");
    git(&api, &["switch", "--quiet", "--detach"]);
    plant_filter(&api, "evil", &r.marker);
    let commit = git(&api, &["rev-parse", "HEAD"]);
    let aside = r.set_aside("api", 2);
    assert_eq!(aside.head, format!("`refs/archon/saved/{commit}`"));
    let kept = git(
        &r.repo,
        &["rev-parse", &format!("refs/archon/saved/{commit}")],
    );
    assert_eq!(kept, commit);
    r.assert_fresh("api", 2, &aside);
    r.assert_filter_never_ran();
}

/// Each attempt moved aside is kept under its own name.
#[test]
fn worktrees_set_aside_twice_are_both_kept() {
    let r = team_run();
    let api = r.worker("api");
    plant_filter(&api, "evil", &r.marker);
    let first = r.set_aside("api", 2);
    plant_filter(&api, "worse", &r.marker);
    let second = r.set_aside("api", 3);
    assert_ne!(first.kept_at, second.kept_at);
    assert!(
        second.kept_at.ends_with("build-api-2"),
        "{:?}",
        second.kept_at
    );
    assert!(first.kept_at.join("evil/f.txt").exists());
    assert!(second.kept_at.join("worse/f.txt").exists());
    r.assert_fresh("api", 3, &second);
    r.assert_filter_never_ran();
}

/// A work stage planted it in the run worktree: no role forks from it,
/// no merge runs there, and nothing ran.
#[test]
fn a_run_worktree_s_own_submodule_repository_is_refused_and_runs_nothing() {
    let r = team_run();
    r.worker("api");
    plant_filter(&r.run.path, "evil", &r.marker);
    let before = git(&r.repo, &["for-each-ref"]);

    let refused = r.ensure("ui", 1).expect_err("a fork from the run worktree");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(!r.worker_path("ui").exists(), "no role forked");
    r.assert_filter_never_ran();

    let refused = merge_team(&r.repo, &r.run.path, &r.plan(&["api"])).expect_err("a merge");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(refused.to_string().contains("evil"), "{refused}");
    r.assert_filter_never_ran();
    assert_eq!(git(&r.repo, &["for-each-ref"]), before, "no ref moved");
}

/// A gitfile naming a submodule git directory the repository keeps, but
/// for another checkout: git would run in that checkout's submodule.
#[test]
fn a_submodule_pointed_at_another_checkout_s_is_refused() {
    let r = team_run();
    let upstream = init_repo(&r.dir.path().join("upstream"));
    let url = upstream.to_str().expect("UTF-8");
    let add = ["-c", "protocol.file.allow=always", "submodule", "add"];
    git(&r.run.path, &[&add[..], &["--quiet", url, "sub"]].concat());
    git(&r.run.path, &["commit", "--quiet", "-m", "add sub"]);
    let run_module = git(
        &r.run.path.join("sub"),
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    );
    let api = r.worker("api");
    let head = git(&upstream, &["rev-parse", "HEAD"]);
    let link = format!("160000,{head},borrowed");
    git(&api, &["update-index", "--add", "--cacheinfo", &link]);
    git(&api, &["commit", "--quiet", "-m", "borrow"]);
    std::fs::create_dir(api.join("borrowed")).expect("mkdir");
    let gitfile = format!("gitdir: {run_module}\n");
    std::fs::write(api.join("borrowed/.git"), gitfile).expect("gitfile");

    let merged = merge_team(&r.repo, &r.run.path, &r.plan(&["api"])).expect("an outcome");
    let MergeOutcome::Refused { reason, .. } = merged.outcome else {
        panic!("expected a refusal, got {:?}", merged.outcome);
    };
    assert!(reason.contains(": borrowed. The next attempt"), "{reason}");
    let run_status = git(&r.run.path, &["status", "--porcelain"]);
    let aside = r.set_aside("api", 2);
    assert_eq!(aside.submodules, ["borrowed"]);
    r.assert_fresh("api", 2, &aside);
    assert_eq!(git(&r.run.path, &["status", "--porcelain"]), run_status);
    assert!(
        Path::new(&run_module).is_dir(),
        "the run's submodule is kept"
    );
}

/// Submodules added the usual way, in the run worktree and in a role's,
/// pass: their git directories are the repository's own.
#[test]
fn submodules_added_with_git_pass() {
    let r = team_run();
    let upstream = init_repo(&r.dir.path().join("upstream"));
    let url = upstream.to_str().expect("UTF-8");
    let add = ["-c", "protocol.file.allow=always", "submodule", "add"];
    git(&r.run.path, &[&add[..], &["--quiet", url, "sub"]].concat());
    git(&r.run.path, &["commit", "--quiet", "-m", "add sub"]);
    let api = r.worker("api");
    let update = ["-c", "protocol.file.allow=always", "submodule", "update"];
    git(&api, &[&update[..], &["--init", "--quiet"]].concat());
    git(&api, &[&add[..], &["--quiet", url, "api-sub"]].concat());
    git(&api, &["commit", "--quiet", "-m", "add api-sub"]);

    let merged = merge_team(&r.repo, &r.run.path, &r.plan(&["api"])).expect("merge");
    assert_eq!(merged.outcome, MergeOutcome::Merged);
    r.ensure("api", 2).expect("the next attempt");
    r.ensure("ui", 1).expect("a fork");
}

/// A submodule added with `git submodule add` in a role's worktree keeps
/// its git directory in the worktree's own: moved aside with it, commits
/// made only inside the submodule, or one nested in it, stay, and nothing
/// kept names the git directory the fresh worktree reuses.
#[test]
fn a_submodule_added_with_git_keeps_its_commits_when_set_aside() {
    let r = team_run();
    let upstream = init_repo(&r.dir.path().join("upstream"));
    let url = upstream.to_str().expect("UTF-8");
    let api = r.worker("api");
    let add = ["-c", "protocol.file.allow=always", "submodule", "add"];
    git(&api, &[&add[..], &["--quiet", url, "legit"]].concat());
    let legit = api.join("legit");
    std::fs::write(legit.join("local.txt"), "local\n").expect("write");
    git(&legit, &["add", "."]);
    git(&legit, &[&add[..], &["--quiet", url, "inner"]].concat());
    git(&legit, &["commit", "--quiet", "-m", "only here"]);
    let local = git(&legit, &["rev-parse", "HEAD"]);
    let inner = git(&legit.join("inner"), &["rev-parse", "HEAD"]);
    git(&api, &["add", "legit"]);
    git(&api, &["commit", "--quiet", "-m", "add legit"]);
    let admin = git(&api, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    plant_filter(&api, "evil", &r.marker);

    let merged = merge_team(&r.repo, &r.run.path, &r.plan(&["api"])).expect("an outcome");
    assert!(
        matches!(merged.outcome, MergeOutcome::Refused { .. }),
        "{merged:?}"
    );
    let aside = r.set_aside("api", 2);
    assert_eq!(aside.submodules, ["evil"]);
    r.assert_fresh("api", 2, &aside);
    r.assert_filter_never_ran();

    let kept_legit = aside.kept_at.join("legit");
    assert_eq!(
        git(&kept_legit, &["rev-parse", "HEAD"]),
        local,
        "its commit is kept"
    );
    let module = git(
        &kept_legit,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    );
    assert_eq!(
        Path::new(&module),
        aside.git_dir_kept_at.join("modules/legit")
    );
    let top = git(&kept_legit, &["rev-parse", "--show-toplevel"]);
    assert_eq!(Path::new(&top), kept_legit, "never the fresh worktree's");
    assert_eq!(git(&kept_legit, &["status", "--porcelain"]), "");
    let kept_inner = kept_legit.join("inner");
    assert_eq!(git(&kept_inner, &["rev-parse", "HEAD"]), inner);
    let top = git(&kept_inner, &["rev-parse", "--show-toplevel"]);
    assert_eq!(Path::new(&top), kept_inner);
    assert!(
        aside.git_dir_kept_at.join("index").is_file(),
        "its index is kept"
    );
    let fresh = git(
        &r.worker_path("api"),
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    );
    assert_eq!(fresh, admin, "git reused the name for the fresh worktree");
    assert!(
        !Path::new(&admin).join("modules").exists(),
        "nothing of the old one"
    );
}

/// A role locked its worktree: a lock only keeps git from pruning it, and
/// it is moved aside, lock and all, since git is not asked to remove it.
#[test]
fn a_locked_worktree_is_set_aside() {
    let r = team_run();
    let api = r.worker("api");
    let path = api.to_str().expect("UTF-8");
    git(&r.repo, &["worktree", "lock", "--reason", "mine", path]);
    plant_filter(&api, "evil", &r.marker);
    let aside = r.set_aside("api", 2);
    assert!(
        aside.git_dir_kept_at.join("locked").is_file(),
        "the lock moved too"
    );
    r.assert_fresh("api", 2, &aside);
    r.assert_filter_never_ran();
}

/// Only worktrees set aside are left of a team run: a single-worktree run
/// of the item still names the team layout.
#[test]
fn an_item_holding_only_worktrees_set_aside_is_a_team_run() {
    let r = team_run();
    let api = r.worker("api");
    plant_filter(&api, "evil", &r.marker);
    r.set_aside("api", 2);
    let (manager, data) = (&r.manager, r.dir.path().join("data"));
    manager
        .remove(&data, &r.repo, &api)
        .expect("remove the role's worktree");
    manager
        .remove(&data, &r.repo, &r.run.path)
        .expect("remove the run worktree");
    let refused = manager
        .create(
            &r.repo,
            "demo",
            &r.item,
            &manager.branch_name(&r.item, "team", 0),
            None,
        )
        .expect_err("a team layout is not reused as a single worktree");
    assert!(
        refused.to_string().contains("holds a team run"),
        "{refused}"
    );
}
