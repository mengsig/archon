//! `WorktreeManager` on a real temp git repository: create, reuse, refuse
//! a foreign checkout, remove (never forced), and re-create from a kept branch;
//! team runs' layout, worker worktrees and the merge of their branches.

use crate::common;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_daemon::worktree::team::{KeptWork, ensure_worker, merge_team};
use archon_daemon::worktree::{Worktree, WorktreeManager};
use archon_pipeline::team::{MergeOutcome, MergePlan, RoleBranch, worker_branch, worker_dir};
use common::{committed, git, init_repo};

struct Fixture {
    _dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    data: std::path::PathBuf,
    manager: WorktreeManager,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    let repo = init_repo(&root.join("repo"));
    let manager = WorktreeManager::new(root.join("worktrees"), "archon/".into());
    Fixture {
        _dir: dir,
        repo,
        data: root.join("data"),
        manager,
    }
}

fn branch_exists(repo: &std::path::Path, branch: &str) -> bool {
    let reference = format!("refs/heads/{branch}");
    git(repo, &["for-each-ref", "--format=%(refname)", &reference]) == reference
}

#[test]
fn create_reuse_and_remove() {
    let f = fixture();
    let item = QueueItemId::new();
    let head = git(&f.repo, &["rev-parse", "HEAD"]);
    let branch = f.manager.branch_name(&item, "Add a greeting!", 0);
    let create = |base| f.manager.create(&f.repo, "demo", &item, &branch, base);
    let created = create(None).expect("create");
    assert_eq!(created.path, f.manager.path_for("demo", &item));
    assert!(created.branch.starts_with("archon/") && created.branch.ends_with("-add-a-greeting"));
    assert_eq!(created.base, head);
    assert_eq!(
        git(&created.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        created.branch
    );
    assert!(created.path.join("README.md").exists());
    assert!(created.added, "a new worktree was added");

    let reused = create(Some("main")).expect("reuse");
    assert_eq!(
        (reused.path.as_path(), reused.branch.as_str()),
        (created.path.as_path(), created.branch.as_str())
    );
    assert_eq!(reused.base, "main");
    assert!(!reused.added, "an existing worktree is reused, not added");

    // Uncommitted work is never discarded: removal needs a clean tree.
    std::fs::write(created.path.join("scratch.txt"), "wip").expect("write");
    let dirty = f.manager.remove(&f.data, &f.repo, &created.path);
    assert!(dirty.is_err(), "a dirty worktree is not removed");
    assert!(created.path.exists());
    std::fs::remove_file(created.path.join("scratch.txt")).expect("clean");
    f.manager
        .remove(&f.data, &f.repo, &created.path)
        .expect("remove");
    assert!(!created.path.exists());
    assert!(
        branch_exists(&f.repo, &created.branch),
        "the branch is kept"
    );

    let again = create(None).expect("recreate from the kept branch");
    assert_eq!(again.branch, created.branch);
    assert_eq!(
        git(&again.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        created.branch
    );
}

#[test]
fn reuse_refuses_a_directory_on_another_branch() {
    let f = fixture();
    let item = QueueItemId::new();
    let created = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "fix", 0),
            None,
        )
        .expect("create");
    git(&created.path, &["switch", "--quiet", "-c", "someone-else"]);
    let refused = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "fix", 0),
            None,
        )
        .expect_err("foreign branch");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
}

#[test]
fn items_get_separate_worktrees_and_branches() {
    let f = fixture();
    let (a, b) = (QueueItemId::new(), QueueItemId::new());
    let first = f
        .manager
        .create(
            &f.repo,
            "demo",
            &a,
            &f.manager.branch_name(&a, "same title", 0),
            None,
        )
        .expect("a");
    let second = f
        .manager
        .create(
            &f.repo,
            "demo",
            &b,
            &f.manager.branch_name(&b, "same title", 0),
            None,
        )
        .expect("b");
    assert_ne!(first.path, second.path);
    assert_ne!(first.branch, second.branch);
    let listed = git(&f.repo, &["worktree", "list", "--porcelain"]);
    for tree in [&first.path, &second.path] {
        assert!(listed.contains(tree.to_str().expect("utf-8")), "{listed}");
    }
}

#[test]
fn a_worktree_deleted_by_hand_is_created_again() {
    let f = fixture();
    let item = QueueItemId::new();
    let created = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "fix", 0),
            None,
        )
        .expect("create");
    std::fs::remove_dir_all(&created.path).expect("rm -rf");
    let again = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "fix", 0),
            None,
        )
        .expect("a missing worktree is re-created");
    assert_eq!(
        git(&again.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        created.branch
    );
    let listed = git(&f.repo, &["worktree", "list", "--porcelain"]);
    let entries = listed
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count();
    assert_eq!(entries, 2, "the repository and one item worktree: {listed}");
}

#[test]
fn a_base_that_looks_like_an_option_is_refused() {
    let f = fixture();
    let item = QueueItemId::new();
    let refused = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "fix", 0),
            Some("--no-checkout"),
        )
        .expect_err("option-like base");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(!f.manager.path_for("demo", &item).exists());
}

/// A team run of a new item: its run worktree and the item directory.
fn team_run(f: &Fixture) -> (Worktree, std::path::PathBuf) {
    let item = QueueItemId::new();
    let run = f
        .manager
        .create_team_run(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "team work", 0),
            None,
        )
        .expect("team run");
    (run, f.manager.path_for("demo", &item))
}

fn run_id() -> PipelineRunId {
    PipelineRunId::parse("prun_01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a run id")
}

/// `role`'s worker worktree of stage `build`, attempt `attempt`.
fn worker(f: &Fixture, run: &Worktree, role: &str, attempt: u32) -> (std::path::PathBuf, String) {
    worker_of(f, run, &run_id(), role, attempt)
}

/// As [`worker`], in the pipeline run `id` of the item.
fn worker_of(
    f: &Fixture,
    run: &Worktree,
    id: &PipelineRunId,
    role: &str,
    attempt: u32,
) -> (std::path::PathBuf, String) {
    let root = run.path.parent().expect("item directory");
    let path = root.join(worker_dir("build", role));
    let branch = worker_branch(&run.branch, id, role, attempt);
    ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch).expect("worker");
    (path, branch)
}

fn head(tree: &std::path::Path) -> String {
    git(tree, &["rev-parse", "HEAD"])
}

fn commit_file(tree: &std::path::Path, file: &str, text: &str) {
    std::fs::write(tree.join(file), text).expect("write");
    git(tree, &["add", file]);
    git(tree, &["commit", "--quiet", "-m", &format!("write {file}")]);
}

fn plan(run: &Worktree, roles: &[(&str, &str)]) -> MergePlan {
    MergePlan {
        stage_id: "build".into(),
        attempt: 1,
        into: run.branch.clone(),
        roles: roles
            .iter()
            .map(|(role, branch)| RoleBranch {
                role: (*role).into(),
                branch: (*branch).into(),
            })
            .collect(),
    }
}

/// Merge `plan` in `run`, as the daemon does.
fn merge(f: &Fixture, run: &Worktree, plan: &MergePlan) -> Result<MergeOutcome, ArchonError> {
    merge_team(&f.repo, &run.path, plan).map(|merged| merged.outcome)
}

fn merge_in_progress(tree: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .output()
        .expect("git runs")
        .status
        .success()
}

fn commit_count(tree: &std::path::Path) -> usize {
    git(tree, &["rev-list", "--count", "HEAD"])
        .parse()
        .expect("count")
}

#[test]
fn a_team_run_lives_in_main_under_the_item_directory() {
    let f = fixture();
    let (run, root) = team_run(&f);
    assert_eq!(run.path, root.join("main"));
    assert_eq!(
        git(&run.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        run.branch
    );

    let item = QueueItemId::new();
    f.manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "flat", 0),
            None,
        )
        .expect("single-worktree run");
    let refused = f
        .manager
        .create_team_run(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "flat", 0),
            None,
        )
        .expect_err("a flat layout is not reused as a team run");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
}

#[test]
fn a_worker_forks_from_the_run_branch_beside_the_run_worktree() {
    let f = fixture();
    let (run, root) = team_run(&f);
    commit_file(&run.path, "run.txt", "run\n");
    let (path, branch) = worker(&f, &run, "api", 1);
    assert_eq!(path, root.join("build-api"));
    assert!(!path.starts_with(&run.path), "workers are never nested");
    assert_eq!(branch, format!("{}--api-q69g5fav-a1", run.branch));
    assert_eq!(git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]), branch);
    assert_eq!(
        git(&path, &["rev-parse", "HEAD"]),
        git(&run.path, &["rev-parse", "HEAD"])
    );

    commit_file(&path, "api.txt", "v1\n");
    ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch)
        .expect("an infra retry reuses it");
    assert!(path.join("api.txt").exists());

    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    ensure_worker(&f.repo, &run.path, &path, &next, &run.branch).expect("a clean tree switches");
    assert_eq!(git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]), next);
    assert!(
        !path.join("api.txt").exists(),
        "attempt 2 forks from the run branch"
    );
}

/// Attempt 1 left a tracked edit and untracked files, one of which the run
/// branch has since added: attempt 2 still forks from the run branch.
#[test]
fn a_new_attempt_saves_what_the_last_left_uncommitted() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (path, first) = worker(&f, &run, "api", 1);
    commit_file(&run.path, "plan.txt", "the run's plan\n");
    std::fs::write(path.join("README.md"), "edited\n").expect("write");
    std::fs::write(path.join("plan.txt"), "a stray plan\n").expect("write");
    std::fs::write(path.join("notes.txt"), "notes\n").expect("write");

    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    ensure_worker(&f.repo, &run.path, &path, &next, &run.branch).expect("a dirty tree moves on");
    assert_eq!(git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]), next);
    assert_eq!(
        head(&path),
        head(&run.path),
        "attempt 2 forks from the run branch"
    );
    assert!(git(&path, &["status", "--porcelain"]).is_empty());
    let plan = std::fs::read_to_string(path.join("plan.txt")).expect("read");
    assert_eq!(plan, "the run's plan\n");

    let saved = git(&path, &["log", "-1", "--format=%s", &first]);
    assert_eq!(
        saved,
        format!("archon: save uncommitted work left on {first}")
    );
    let notes = git(&path, &["show", &format!("{first}:notes.txt")]);
    assert_eq!(notes, "notes");
    assert_eq!(
        git(&path, &["show", &format!("{first}:README.md")]),
        "edited"
    );
}

#[test]
fn a_fork_needs_the_run_worktree_committed() {
    let f = fixture();
    let (run, root) = team_run(&f);
    std::fs::write(run.path.join("README.md"), "planned, not committed\n").expect("write");
    let path = root.join(worker_dir("build", "api"));
    let branch = worker_branch(&run.branch, &run_id(), "api", 1);
    let refused = ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch)
        .expect_err("a dirty run worktree");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(refused.to_string().contains("README.md"), "{refused}");
    assert!(
        !path.exists() && !branch_exists(&f.repo, &branch),
        "nothing forked"
    );

    git(&run.path, &["commit", "--quiet", "-am", "plan"]);
    worker(&f, &run, "api", 1);
    assert_eq!(head(&path), head(&run.path));
    std::fs::write(run.path.join("README.md"), "later\n").expect("write");
    ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch)
        .expect("a retry of the same attempt forks nothing");
}

/// A retried item starts a new run on the same run branch and directory:
/// its roles fork new branches from the run branch as it is now.
#[test]
fn another_run_of_the_item_forks_new_branches() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (path, earlier) = worker(&f, &run, "api", 1);
    commit_file(&path, "api.txt", "run 1\n");
    commit_file(&run.path, "fix.txt", "landed since\n");

    let retry = PipelineRunId::new();
    let (again, branch) = worker_of(&f, &run, &retry, "api", 1);
    assert_eq!(again, path, "the role's directory is reused");
    assert_ne!(branch, earlier);
    assert_eq!(
        head(&path),
        head(&run.path),
        "forked from the run branch tip"
    );
    assert!(
        !path.join("api.txt").exists(),
        "run 1's work is not carried over"
    );
    assert!(branch_exists(&f.repo, &earlier), "run 1's branch is kept");
}

#[test]
fn merging_puts_every_role_on_the_run_branch_once() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    commit_file(&api, "api.txt", "api\n");
    commit_file(&ui, "ui.txt", "ui\n");
    let plan = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);

    let before = commit_count(&run.path);
    assert_eq!(merge(&f, &run, &plan).expect("merge"), MergeOutcome::Merged);
    assert!(run.path.join("api.txt").exists() && run.path.join("ui.txt").exists());
    let subjects = git(&run.path, &["log", "--merges", "--format=%s"]);
    assert_eq!(
        subjects,
        format!("Merge role ui from {ui_branch}\nMerge role api from {api_branch}")
    );
    // Two role commits and two merge commits.
    let merged = commit_count(&run.path);
    assert_eq!(merged, before + 4);

    assert_eq!(merge(&f, &run, &plan).expect("again"), MergeOutcome::Merged);
    assert_eq!(
        commit_count(&run.path),
        merged,
        "a repeated merge adds nothing"
    );
}

#[test]
fn a_merge_resumes_after_a_crash_between_roles() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    commit_file(&api, "api.txt", "api\n");
    commit_file(&ui, "ui.txt", "ui\n");
    let plan = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);

    let only_api = MergePlan {
        roles: plan.roles[..1].to_vec(),
        ..plan.clone()
    };
    merge(&f, &run, &only_api).expect("the first role merged");
    // The crash left the second merge half done.
    git(&run.path, &["merge", "--no-ff", "--no-commit", &ui_branch]);
    assert!(merge_in_progress(&run.path));

    assert_eq!(
        merge(&f, &run, &plan).expect("resume"),
        MergeOutcome::Merged
    );
    let merges = git(&run.path, &["log", "--merges", "--format=%s"]);
    assert_eq!(
        merges.lines().count(),
        2,
        "api is not merged twice: {merges}"
    );
    assert!(run.path.join("ui.txt").exists());
}

#[test]
fn a_conflict_is_aborted_and_names_its_files() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    commit_file(&api, "README.md", "api's readme\n");
    commit_file(&ui, "README.md", "ui's readme\n");
    let plan = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);

    let outcome = merge(&f, &run, &plan).expect("a conflict is an outcome");
    assert_eq!(
        outcome,
        MergeOutcome::Conflict {
            role: "ui".into(),
            files: vec!["README.md".into()],
        }
    );
    let status = git(&run.path, &["status", "--porcelain"]);
    assert!(status.is_empty(), "the merge was aborted: {status}");
    assert!(
        !merge_in_progress(&run.path),
        "no merge is left in progress"
    );
    assert_eq!(
        std::fs::read_to_string(run.path.join("README.md")).expect("read"),
        "api's readme\n"
    );
}

#[test]
fn a_missing_worker_branch_is_an_error() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let plan = plan(&run, &[("api", "archon/nope--api-q69g5fav-a1")]);
    let refused = merge(&f, &run, &plan).expect_err("no such branch");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(git(&run.path, &["log", "--merges", "--format=%s"]).is_empty());
}

#[test]
fn an_untracked_file_a_role_adds_is_reported_as_a_conflict() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    commit_file(&api, "new.txt", "api's file\n");
    commit_file(&api, "other.txt", "other\n");
    std::fs::write(run.path.join("new.txt"), "left in main\n").expect("write");
    std::fs::write(run.path.join("unrelated.txt"), "scratch\n").expect("write");
    let plan = plan(&run, &[("api", &api_branch)]);

    let outcome = merge(&f, &run, &plan).expect("an outcome");
    assert_eq!(
        outcome,
        MergeOutcome::Conflict {
            role: "api".into(),
            files: vec!["new.txt".into()],
        }
    );
    assert!(!merge_in_progress(&run.path));
    assert!(git(&run.path, &["log", "--merges", "--format=%s"]).is_empty());
    let kept = std::fs::read_to_string(run.path.join("new.txt")).expect("read");
    assert_eq!(kept, "left in main\n", "the untracked file is untouched");
}

/// The daemon's merges need a git identity: without a usable one it
/// commits as Archon, and no repository hook runs in it.
#[test]
fn a_merge_without_a_git_identity_commits_as_archon_without_hooks() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    commit_file(&api, "api.txt", "api\n");
    git(&f.repo, &["config", "user.name", ""]);
    let hooks = f.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-merge-commit");
    archon_core::testing::write_script(&hook, "#!/bin/sh\nexit 1\n");

    let plan = plan(&run, &[("api", &api_branch)]);
    assert_eq!(merge(&f, &run, &plan).expect("merge"), MergeOutcome::Merged);
    let author = git(&run.path, &["log", "-1", "--format=%an <%ae>"]);
    assert_eq!(author, "Archon <archon@localhost>");
}

/// A role that forgot to commit: its edits and new files are committed on
/// its branch before the merge, so they land on the run branch.
#[test]
fn a_role_s_uncommitted_work_is_committed_and_merged() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    commit_file(&api, "api.txt", "api\n");
    std::fs::write(ui.join("README.md"), "ui's edit\n").expect("write");
    std::fs::write(ui.join("ui.txt"), "ui\n").expect("write");
    let plan = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);

    assert_eq!(merge(&f, &run, &plan).expect("merge"), MergeOutcome::Merged);
    assert!(git(&ui, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        git(&ui, &["log", "-1", "--format=%s"]),
        "archon: commit the work role ui left uncommitted"
    );
    let readme = std::fs::read_to_string(run.path.join("README.md")).expect("read");
    assert_eq!(readme, "ui's edit\n");
    assert!(run.path.join("ui.txt").exists() && run.path.join("api.txt").exists());
    assert_eq!(
        git(&run.path, &["log", "--merges", "--format=%s"])
            .lines()
            .count(),
        2
    );
}

/// A test run's bytecode cache is never committed for the role, and does
/// not make the merge refuse its branch.
#[test]
fn a_roles_python_caches_are_left_out_of_its_leftover_commit() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    std::fs::create_dir_all(api.join("pkg/__pycache__")).expect("mkdir");
    std::fs::write(api.join("pkg/__pycache__/mod.cpython-312.pyc"), "x").expect("write");
    std::fs::write(api.join("loose.pyc"), "x").expect("write");
    std::fs::write(api.join("api.txt"), "api\n").expect("write");
    let plan = plan(&run, &[("api", &api_branch)]);

    assert_eq!(merge(&f, &run, &plan).expect("merge"), MergeOutcome::Merged);
    let tracked = git(&run.path, &["ls-files"]);
    assert!(tracked.contains("api.txt"), "{tracked}");
    assert!(
        !tracked.contains("pyc") && !tracked.contains("__pycache__"),
        "{tracked}"
    );
}

/// Work a role committed on a branch of its own is not on its worker
/// branch: the merge is refused rather than shipping without it.
#[test]
fn a_role_off_its_branch_is_refused_before_anything_merges() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    commit_file(&api, "api.txt", "api\n");
    git(&ui, &["switch", "--quiet", "-c", "feature"]);
    commit_file(&ui, "ui.txt", "ui\n");
    let plan = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);

    let MergeOutcome::Refused { role, reason } = merge(&f, &run, &plan).expect("an outcome") else {
        panic!("expected a refusal");
    };
    assert_eq!(role, "ui");
    assert!(reason.contains("`feature` checked out"), "{reason}");
    assert!(git(&run.path, &["log", "--merges", "--format=%s"]).is_empty());
    assert!(!run.path.join("api.txt").exists(), "nothing merged");
}

#[test]
fn a_role_mid_merge_is_refused_and_left_alone() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    git(&api, &["switch", "--quiet", "-c", "side"]);
    commit_file(&api, "README.md", "side\n");
    git(&api, &["switch", "--quiet", &api_branch]);
    commit_file(&api, "README.md", "main line\n");
    let clash = std::process::Command::new("git")
        .arg("-C")
        .arg(&api)
        .args(["-c", "user.name=T", "-c", "user.email=t@t", "merge", "side"])
        .output()
        .expect("git runs");
    assert!(!clash.status.success(), "the merge conflicts");
    let plan = plan(&run, &[("api", &api_branch)]);

    let MergeOutcome::Refused { role, reason } = merge(&f, &run, &plan).expect("an outcome") else {
        panic!("expected a refusal");
    };
    assert_eq!(role, "api");
    assert!(reason.contains("unmerged files: README.md"), "{reason}");
    assert!(merge_in_progress(&api), "the role's merge is not touched");
    assert!(git(&run.path, &["log", "--merges", "--format=%s"]).is_empty());
}

/// Git refuses to merge a file over an untracked directory, or into an
/// untracked file's place as a directory: both are reported as conflicts.
#[test]
fn untracked_paths_a_role_adds_under_or_over_are_conflicts() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    std::fs::create_dir_all(api.join("docs")).expect("mkdir");
    commit_file(&api, "docs/x.md", "api's docs\n");
    commit_file(&api, "lib", "a file named lib\n");
    std::fs::write(run.path.join("docs"), "a stray file\n").expect("write");
    std::fs::create_dir_all(run.path.join("lib")).expect("mkdir");
    std::fs::write(run.path.join("lib/a.txt"), "stray\n").expect("write");
    let plan = plan(&run, &[("api", &api_branch)]);

    assert_eq!(
        merge(&f, &run, &plan).expect("an outcome, not an error"),
        MergeOutcome::Conflict {
            role: "api".into(),
            files: vec!["docs".into(), "lib/a.txt".into()],
        }
    );
    assert!(!merge_in_progress(&run.path));
}

#[test]
fn conflicting_file_names_are_not_quoted() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    commit_file(&api, "é.txt", "api\n");
    commit_file(&ui, "é.txt", "ui\n");
    let plan = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);

    assert_eq!(
        merge(&f, &run, &plan).expect("an outcome"),
        MergeOutcome::Conflict {
            role: "ui".into(),
            files: vec!["é.txt".into()],
        }
    );
}

/// File names with leading or trailing spaces, or a newline, are listed
/// exactly: in a conflict, in unmerged files, and in the run worktree.
#[test]
fn file_names_with_spaces_and_newlines_are_kept_exact() {
    let f = fixture();
    let (run, root) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    let names = [" lead.txt", "new\nline.txt", "trail.txt "];
    for (tree, text) in [(&api, "api\n"), (&ui, "ui\n")] {
        for name in names {
            std::fs::write(tree.join(name), text).expect("write");
        }
        git(tree, &["add", "--all"]);
        git(tree, &["commit", "--quiet", "-m", "odd names"]);
    }
    let both = plan(&run, &[("api", &api_branch), ("ui", &ui_branch)]);
    assert_eq!(
        merge(&f, &run, &both).expect("an outcome"),
        MergeOutcome::Conflict {
            role: "ui".into(),
            files: names.map(String::from).to_vec(),
        }
    );

    let clash = std::process::Command::new("git")
        .arg("-C")
        .arg(&ui)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "merge",
            &api_branch,
        ])
        .output()
        .expect("git runs");
    assert!(!clash.status.success(), "the merge conflicts");
    let MergeOutcome::Refused { reason, .. } =
        merge(&f, &run, &plan(&run, &[("ui", &ui_branch)])).expect("an outcome")
    else {
        panic!("expected a refusal");
    };
    assert!(reason.ends_with(&names.join(", ")), "{reason:?}");

    std::fs::write(run.path.join(names[2]), "tracked edit\n").expect("write");
    let path = root.join(worker_dir("build", "db"));
    let branch = worker_branch(&run.branch, &run_id(), "db", 1);
    let refused = ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch)
        .expect_err("a dirty run worktree");
    assert!(
        refused.to_string().contains("changes to trail.txt ;"),
        "{refused}"
    );
}

/// A file name that is not UTF-8 is committed and merged like any other,
/// and shown lossily; a nested repository so named is refused, not an
/// error, since no pathspec could leave it out exactly.
#[test]
fn file_names_that_are_not_utf8_are_merged_or_refused_by_role() {
    use std::os::unix::ffi::OsStrExt;
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    std::fs::write(api.join(name), "api\n").expect("write");

    let merged = merge_team(&f.repo, &run.path, &plan(&run, &[("api", &api_branch)]));
    let merged = merged.expect("an outcome, not an error");
    assert_eq!(merged.outcome, MergeOutcome::Merged);
    let files: Vec<&str> = merged
        .saved
        .iter()
        .flat_map(|s| &s.files)
        .map(String::as_str)
        .collect();
    assert_eq!(files, ["caf\u{FFFD}.txt"]);
    assert!(run.path.join(name).exists(), "merged exactly as named");

    let (ui, ui_branch) = worker(&f, &run, "ui", 1);
    init_repo(&ui.join(std::ffi::OsStr::from_bytes(b"vend\xf6r")));
    std::fs::write(ui.join("ui.txt"), "ui\n").expect("write");
    let MergeOutcome::Refused { role, reason } =
        merge(&f, &run, &plan(&run, &[("ui", &ui_branch)])).expect("an outcome, not an error")
    else {
        panic!("expected a refusal");
    };
    assert_eq!(role, "ui");
    assert!(reason.contains("vend\u{FFFD}r/"), "{reason}");
    assert!(
        git(&ui, &["ls-tree", "--name-only", &ui_branch])
            .lines()
            .all(|f| f != "ui.txt")
    );
}

/// No repository hook runs in the daemon's git: not when a role's worktree
/// is added, merged, or moved on to its next attempt.
#[test]
fn no_repository_hook_runs_in_the_daemon_s_git() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let log = f._dir.path().join("hooks.log");
    let hooks = f.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let names = [
        "post-checkout",
        "reference-transaction",
        "pre-commit",
        "post-commit",
        "post-merge",
    ];
    for name in names {
        let hook = hooks.join(name);
        let script = format!("#!/bin/sh\necho {name} >> '{}'\n", log.display());
        archon_core::testing::write_script(&hook, &script);
    }

    let (api, api_branch) = worker(&f, &run, "api", 1);
    std::fs::write(api.join("api.txt"), "api\n").expect("write");
    let one = plan(&run, &[("api", &api_branch)]);
    assert_eq!(merge(&f, &run, &one).expect("merge"), MergeOutcome::Merged);
    std::fs::write(api.join("more.txt"), "more\n").expect("write");
    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    let saved = ensure_worker(&f.repo, &run.path, &api, &next, &run.branch).expect("moves on");
    assert_eq!(committed(saved).files, ["more.txt"]);
    let ran = log
        .exists()
        .then(|| std::fs::read_to_string(&log).expect("read"));
    assert_eq!(ran, None, "hooks ran as the daemon");
}

/// A repository that signs commits with a `gpg.program` of its own does
/// not make the daemon's team merge or leftover commit fail or run it.
#[test]
fn a_repository_s_signing_configuration_is_not_used_by_the_daemon_s_git() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    commit_file(&api, "api.txt", "api\n");
    std::fs::write(api.join("pre.txt"), "pre\n").expect("write");
    let marker = f._dir.path().join("gpg.ran");
    let program = f._dir.path().join("fake-gpg");
    let script = format!("#!/bin/sh\necho ran >> '{}'\nexit 1\n", marker.display());
    archon_core::testing::write_script(&program, &script);
    git(&f.repo, &["config", "commit.gpgSign", "true"]);
    git(
        &f.repo,
        &["config", "gpg.program", program.to_str().expect("UTF-8")],
    );

    let one = plan(&run, &[("api", &api_branch)]);
    let merged_from = head(&run.path);
    assert_eq!(merge(&f, &run, &one).expect("merge"), MergeOutcome::Merged);
    assert_ne!(head(&run.path), merged_from, "the merge commit was made");
    assert!(run.path.join("pre.txt").exists(), "the leftover was merged");
    std::fs::write(api.join("more.txt"), "more\n").expect("write");
    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    let saved = ensure_worker(&f.repo, &run.path, &api, &next, &run.branch).expect("moves on");
    assert_eq!(committed(saved).files, ["more.txt"]);
    assert!(!marker.exists(), "the repository's gpg.program ran");
}

/// A worker directory that is not a worktree, inside some other work
/// tree: nothing is committed in the enclosing one.
#[test]
fn a_worker_directory_that_is_not_a_worktree_is_refused() {
    let f = fixture();
    let (run, root) = team_run(&f);
    let enclosing = root.parent().expect("repository directory");
    git(enclosing, &["init", "--quiet"]);
    let path = root.join(worker_dir("build", "api"));
    std::fs::create_dir_all(&path).expect("mkdir");
    std::fs::write(path.join("stray.txt"), "stray\n").expect("write");
    let branch = worker_branch(&run.branch, &run_id(), "api", 1);

    let refused =
        ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch).expect_err("not a worktree");
    assert!(
        refused.to_string().contains("is not a git worktree"),
        "{refused}"
    );
    let commits = git(enclosing, &["rev-list", "--all"]);
    assert!(commits.is_empty(), "nothing committed around it: {commits}");
}

#[test]
fn a_single_worktree_run_refuses_an_item_laid_out_as_a_team() {
    let f = fixture();
    let item = QueueItemId::new();
    f.manager
        .create_team_run(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "team", 0),
            None,
        )
        .expect("team run");
    let refused = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "team", 0),
            None,
        )
        .expect_err("a team layout is not reused as a single worktree");
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(
        refused.to_string().contains("holds a team run"),
        "{refused}"
    );
}

/// Changes `add --all` leaves nothing to commit for: a role staged an edit
/// and reverted it, and untracked a file it kept. Merging and moving on
/// both go ahead without a commit.
#[test]
fn leftovers_with_nothing_to_commit_are_not_committed() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    commit_file(&api, "api.txt", "api\n");
    let leave_nothing_to_commit = |tree: &std::path::Path| {
        std::fs::write(tree.join("README.md"), "staged\n").expect("write");
        git(tree, &["add", "README.md"]);
        std::fs::write(tree.join("README.md"), "demo\n").expect("write");
        git(tree, &["rm", "--quiet", "--cached", "api.txt"]);
        assert!(!git(tree, &["status", "--porcelain"]).is_empty());
    };
    leave_nothing_to_commit(&api);
    let before = head(&api);

    let plan = plan(&run, &[("api", &api_branch)]);
    assert_eq!(merge(&f, &run, &plan).expect("merge"), MergeOutcome::Merged);
    assert_eq!(head(&api), before, "nothing was committed");
    assert!(run.path.join("api.txt").exists());

    leave_nothing_to_commit(&api);
    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    ensure_worker(&f.repo, &run.path, &api, &next, &run.branch).expect("it moves on");
    assert_eq!(git(&api, &["rev-parse", "--abbrev-ref", "HEAD"]), next);
    assert_eq!(git(&api, &["rev-parse", &api_branch]), before);
}

/// `role`'s worktree with a submodule `sub` committed on its branch.
fn with_submodule(f: &Fixture, run: &Worktree, role: &str) -> (std::path::PathBuf, String) {
    let upstream = init_repo(&f._dir.path().join(format!("upstream-{role}")));
    let (tree, branch) = worker(f, run, role, 1);
    let url = upstream.to_str().expect("UTF-8");
    let add = [
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--quiet",
        url,
        "sub",
    ];
    git(&tree, &add);
    git(&tree, &["commit", "--quiet", "-m", "add sub"]);
    (tree, branch)
}

/// Work inside a submodule cannot land on the role's branch: the merge is
/// refused naming it, with or without other leftovers, and never panics.
#[test]
fn a_dirty_submodule_is_refused_by_name() {
    let f = fixture();
    let (run, _) = team_run(&f);
    for (role, ordinary_change) in [("api", false), ("ui", true)] {
        let (tree, branch) = with_submodule(&f, &run, role);
        std::fs::write(tree.join("sub/README.md"), "edited inside\n").expect("write");
        if ordinary_change {
            std::fs::write(tree.join("notes.txt"), "notes\n").expect("write");
        }
        let plan = plan(&run, &[(role, &branch)]);
        let MergeOutcome::Refused {
            role: refused,
            reason,
        } = merge(&f, &run, &plan).expect("an outcome, not an error")
        else {
            panic!("expected a refusal for {role}");
        };
        assert_eq!(refused, role);
        assert!(reason.contains(": sub. Remove"), "{reason}");
        let committed = git(&tree, &["ls-tree", "--name-only", &branch]);
        assert_eq!(
            committed.contains("notes.txt"),
            ordinary_change,
            "{committed}"
        );
    }
    assert!(git(&run.path, &["log", "--merges", "--format=%s"]).is_empty());
}

/// A submodule holding only untracked files, such as its build output,
/// has nothing to commit: the merge goes ahead and they stay untouched.
#[test]
fn a_submodule_with_only_untracked_files_is_merged() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (tree, branch) = with_submodule(&f, &run, "api");
    std::fs::create_dir_all(tree.join("sub/target")).expect("mkdir");
    std::fs::write(tree.join("sub/target/out.o"), "built\n").expect("write");
    std::fs::write(tree.join("api.txt"), "api\n").expect("write");
    let plan = plan(&run, &[("api", &branch)]);

    let merged = merge_team(&f.repo, &run.path, &plan).expect("merge");
    assert_eq!(merged.outcome, MergeOutcome::Merged);
    let files: Vec<&str> = merged
        .saved
        .iter()
        .flat_map(|s| &s.files)
        .map(String::as_str)
        .collect();
    assert_eq!(files, ["api.txt"], "only the role's own file is committed");
    assert!(tree.join("sub/target/out.o").exists());
    assert!(run.path.join("api.txt").exists());
}

/// A repository a role created inside its worktree is never committed as
/// a gitlink: the merge is refused naming it, and the rest is committed.
#[test]
fn a_nested_repository_is_refused_and_never_committed() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    init_repo(&api.join("vendor/nested"));
    std::fs::write(api.join("api.txt"), "api\n").expect("write");
    let plan = plan(&run, &[("api", &api_branch)]);

    let MergeOutcome::Refused { reason, .. } = merge(&f, &run, &plan).expect("an outcome") else {
        panic!("expected a refusal");
    };
    assert!(reason.contains("vendor/nested/"), "{reason}");
    let committed = git(&api, &["ls-tree", "-r", "--name-only", &api_branch]);
    assert!(committed.contains("api.txt"), "{committed}");
    assert!(!committed.contains("vendor"), "no gitlink: {committed}");
}

/// A role left a cherry-pick stopped on a conflict: the merge is refused,
/// and the next attempt ends it and starts on its new branch.
#[test]
fn a_role_mid_cherry_pick_is_refused_then_moves_on() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    git(&api, &["switch", "--quiet", "-c", "side"]);
    commit_file(&api, "README.md", "side\n");
    commit_file(&api, "side.txt", "side\n");
    git(&api, &["switch", "--quiet", &api_branch]);
    commit_file(&api, "README.md", "main line\n");
    let pick = std::process::Command::new("git")
        .arg("-C")
        .arg(&api)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "cherry-pick",
            "side~1",
            "side",
        ])
        .output()
        .expect("git runs");
    assert!(
        !pick.status.success(),
        "the cherry-pick stops on a conflict"
    );
    let plan = plan(&run, &[("api", &api_branch)]);
    assert!(matches!(
        merge(&f, &run, &plan).expect("an outcome"),
        MergeOutcome::Refused { .. }
    ));

    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    ensure_worker(&f.repo, &run.path, &api, &next, &run.branch).expect("it moves on");
    assert_eq!(git(&api, &["rev-parse", "--abbrev-ref", "HEAD"]), next);
    assert_eq!(head(&api), head(&run.path));
    let saved = git(&api, &["log", "-1", "--format=%s", &api_branch]);
    assert_eq!(
        saved,
        format!("archon: save uncommitted work left on {api_branch}")
    );
}

/// A role pointed its worktree at a repository of its own: the daemon
/// runs no git there, whose config could run commands as the daemon.
#[test]
fn a_worker_of_another_repository_is_refused() {
    let f = fixture();
    let (run, _) = team_run(&f);
    let (api, api_branch) = worker(&f, &run, "api", 1);
    let foreign = init_repo(&f._dir.path().join("foreign"));
    let elsewhere = f._dir.path().join("foreign-wt");
    git(
        &foreign,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "x",
            elsewhere.to_str().expect("UTF-8"),
        ],
    );
    std::fs::copy(elsewhere.join(".git"), api.join(".git")).expect("copy");
    let plan = plan(&run, &[("api", &api_branch)]);

    let refused = merge(&f, &run, &plan).expect_err("a foreign worktree");
    assert!(
        refused.to_string().contains("not of the item's repository"),
        "{refused}"
    );
    let next = worker_branch(&run.branch, &run_id(), "api", 2);
    let refused = ensure_worker(&f.repo, &run.path, &api, &next, &run.branch)
        .expect_err("a foreign worktree");
    assert!(
        refused.to_string().contains("not of the item's repository"),
        "{refused}"
    );
}

/// A team run's worktree was removed but a role's worktree stayed: a
/// single-worktree run of the item still names the team layout.
#[test]
fn a_single_worktree_run_refuses_an_item_with_role_worktrees_left() {
    let f = fixture();
    let item = QueueItemId::new();
    let run = f
        .manager
        .create_team_run(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "team", 0),
            None,
        )
        .expect("team run");
    let root = f.manager.path_for("demo", &item);
    let path = root.join(worker_dir("build", "api"));
    let branch = worker_branch(&run.branch, &run_id(), "api", 1);
    ensure_worker(&f.repo, &run.path, &path, &branch, &run.branch).expect("worker");
    f.manager
        .remove(&f.data, &f.repo, &run.path)
        .expect("remove the run worktree");

    let refused = f
        .manager
        .create(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "team", 0),
            None,
        )
        .expect_err("a team layout is not reused as a single worktree");
    assert!(
        refused.to_string().contains("holds a team run"),
        "{refused}"
    );
}

/// A role pointed its `HEAD` at the next attempt's branch before it
/// existed: what it left belongs to no commit, so its worktree is moved
/// aside unentered, files and all, and the attempt starts afresh.
#[test]
fn a_head_on_a_branch_with_no_commit_is_set_aside() {
    let f = fixture();
    let item = QueueItemId::new();
    let run = f
        .manager
        .create_team_run(
            &f.repo,
            "demo",
            &item,
            &f.manager.branch_name(&item, "team", 0),
            None,
        )
        .expect("team run");
    let path = run
        .path
        .parent()
        .expect("item")
        .join(worker_dir("build", "api"));
    let id = run_id();
    let first = worker_branch(&run.branch, &id, "api", 1);
    ensure_worker(&f.repo, &run.path, &path, &first, &run.branch).expect("worker");
    std::fs::write(path.join("left.txt"), "left\n").expect("write");
    let second = worker_branch(&run.branch, &id, "api", 2);
    let unborn = format!("refs/heads/{second}");
    git(&path, &["symbolic-ref", "HEAD", &unborn]);
    let before = git(&f.repo, &["for-each-ref"]);

    let kept =
        ensure_worker(&f.repo, &run.path, &path, &second, &run.branch).expect("a fresh attempt");
    let Some(KeptWork::SetAside(aside)) = kept else {
        panic!("expected the worktree set aside, got {kept:?}");
    };
    assert!(aside.unborn && aside.submodules.is_empty(), "{aside:?}");
    assert_eq!(aside.head, format!("`{second}`"));
    let described = aside.describe();
    assert!(described.contains("which has no commit"), "{described}");
    assert!(
        described.contains("Nothing it left was committed"),
        "{described}"
    );
    assert!(
        aside.kept_at.join("left.txt").exists(),
        "its files are kept"
    );
    assert!(
        !path.join("left.txt").exists(),
        "the fresh worktree is clean"
    );
    assert_eq!(git(&path, &["symbolic-ref", "--short", "HEAD"]), second);
    assert_eq!(head(&path), head(&run.path), "forked from the run branch");
    let refs = git(&f.repo, &["for-each-ref"]);
    let new_ref = format!("refs/heads/{second}");
    let old: Vec<&str> = refs.lines().filter(|l| !l.ends_with(&new_ref)).collect();
    assert_eq!(old.join("\n"), before, "no other ref moved");
}
