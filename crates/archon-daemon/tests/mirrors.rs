//! The orchestrator's repository mirrors (ARCHITECTURE §13.3): a registered
//! repository is readable under `repos/<name>` with no queue item, follows
//! its base branch, and reaches neither the operator's checkout nor a remote.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use archon_core::id::RepoId;
use archon_core::protocol::{Reply, RepoAdd, RepoRef, Request};
use archon_core::queue::QueueState;
use archon_core::repo::ShipMode;
use archon_core::run::{StageOutcome, StageRun};
use archon_daemon::mirror;
use archon_daemon::triggers::{self, TriggerEvent};
use common::{Harness, PIPELINE, REPO_NAME, completed, git, verdict};

fn mirror_of(h: &Harness) -> PathBuf {
    mirror::mirrors_root(&h.ctx.paths.orchestrator_workspace).join(REPO_NAME)
}

fn git_dir_of(h: &Harness) -> PathBuf {
    mirror::git_dirs_root(&h.ctx.paths.data_dir).join(REPO_NAME)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

async fn synced() -> Harness {
    let h = Harness::new().await;
    mirror::sync_all(&h.ctx).await.expect("mirror pass");
    h
}

/// Git as an agent in `dir` runs it: the directory's own config and hooks.
fn agent_git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn head(dir: &Path) -> String {
    stdout(&agent_git(dir, &["rev-parse", "HEAD"]))
}

/// Commit `file` to the repository's checked-out branch; returns the commit.
fn commit(repo: &Path, file: &str, text: &str) -> String {
    std::fs::write(repo.join(file), text).expect("write");
    git(repo, &["add", file]);
    git(repo, &["commit", "--quiet", "-m", file]);
    git(repo, &["rev-parse", "HEAD"])
}

async fn system_notes(h: &Harness) -> Vec<String> {
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    feed.into_iter()
        .filter_map(|item| match item.entry {
            archon_core::notify::FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect()
}

/// The `(state, detail)` of every mirror event held for the orchestrator.
async fn mirror_news(h: &Harness) -> Vec<(String, String)> {
    let held = triggers::held(&h.ctx).await.expect("held news");
    held.into_iter()
        .filter_map(|event| match event {
            TriggerEvent::Mirror {
                repo,
                state,
                detail,
            } => {
                assert_eq!(repo, REPO_NAME);
                Some((state, detail))
            }
            _ => None,
        })
        .collect()
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never happened: {what}");
}

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(serde_json::json!({"pr_url": "https://example.invalid/pr/1"})),
        },
        _ => completed("implemented"),
    }
}

#[tokio::test]
async fn a_registered_repository_is_mirrored_at_its_base_with_no_remote() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    assert_eq!(head(&mirror), git(&h.repo, &["rev-parse", "main"]));
    assert_eq!(
        std::fs::read_to_string(mirror.join("README.md")).unwrap(),
        "demo\n"
    );
    assert_eq!(stdout(&agent_git(&mirror, &["remote"])), "", "no remote");
    let branch = agent_git(&mirror, &["symbolic-ref", "--quiet", "HEAD"]);
    assert!(!branch.status.success(), "the checkout is detached");
    let refs = stdout(&agent_git(
        &mirror,
        &["for-each-ref", "--format=%(refname)"],
    ));
    assert!(refs.contains("refs/mirror/main"), "{refs}");
    assert!(!refs.contains("refs/heads/"), "no branch to push: {refs}");

    let git_dir = git_dir_of(&h);
    let gitfile = std::fs::read_to_string(mirror.join(".git")).expect("`.git` is a file");
    assert_eq!(gitfile, format!("gitdir: {}\n", git_dir.display()));
    let objects = files_under(&git_dir.join("objects"));
    assert!(!objects.is_empty(), "the mirror holds objects");
    for object in objects {
        let links = std::fs::metadata(&object).unwrap().nlink();
        assert_eq!(links, 1, "{object:?} is a hard link into the repository");
    }
    assert!(
        h.ctx
            .paths
            .orchestrator_workspace
            .join(mirror::SCRATCH_DIR)
            .is_dir()
    );
    assert!(
        system_notes(&h).await.is_empty(),
        "a good refresh is silent"
    );
    assert!(mirror_news(&h).await.is_empty());
}

#[tokio::test]
async fn the_mirror_follows_the_base_branch_when_an_item_finishes() {
    let h = Harness::new().await;
    let mirror = mirror_of(&h);
    let worker = tokio::spawn(mirror::run(h.ctx.clone()));
    let first = git(&h.repo, &["rev-parse", "main"]);
    eventually("the mirror is made at start", || {
        mirror.join("README.md").exists() && head(&mirror) == first
    })
    .await;

    let item = h.enqueue("add a greeting").await;
    h.dispatch().await;
    let merged = commit(&h.repo, "greeting.txt", "hello\n");
    assert_eq!(head(&mirror), first, "nothing asked for a refresh yet");
    for _ in ["implement", "review", "ship"] {
        h.settle_running(passing).await;
        h.dispatch().await;
    }
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    eventually("the mirror reaches the new base", || {
        head(&mirror) == merged
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(mirror.join("greeting.txt")).unwrap(),
        "hello\n"
    );
    worker.abort();
}

#[tokio::test]
async fn a_refresh_resets_tracked_files_and_keeps_untracked_ones() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    std::fs::write(mirror.join("README.md"), "a helper's experiment\n").unwrap();
    std::fs::write(mirror.join("notes.txt"), "a helper's notes\n").unwrap();
    mirror::sync_all(&h.ctx).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(mirror.join("README.md")).unwrap(),
        "demo\n"
    );

    let moved = commit(&h.repo, "README.md", "demo, second edition\n");
    std::fs::write(mirror.join("README.md"), "another experiment\n").unwrap();
    mirror::sync_all(&h.ctx).await.unwrap();
    assert_eq!(head(&mirror), moved);
    assert_eq!(
        std::fs::read_to_string(mirror.join("README.md")).unwrap(),
        "demo, second edition\n"
    );
    assert_eq!(
        std::fs::read_to_string(mirror.join("notes.txt")).unwrap(),
        "a helper's notes\n"
    );
}

/// Register a second repository, `name`, and return its id.
async fn register(h: &Harness, name: &str) -> RepoId {
    let path = common::init_repo_with_origin(&h.dir.path().join(name));
    let reply = h
        .cli(Request::RepoAdd(RepoAdd {
            path,
            name: Some(name.into()),
            ship: Some(ShipMode::Pr),
            base_branch: Some("main".into()),
            default_pipeline: Some(PIPELINE.into()),
            auto_merge: false,
            github_watch: false,
        }))
        .await
        .expect("register");
    let Reply::Repo(repo) = reply else {
        panic!("expected the repository, got {reply:?}");
    };
    repo.id
}

#[tokio::test]
async fn a_pass_refreshes_only_the_mirrors_asked_for() {
    let h = synced().await;
    let other = register(&h, "other").await;
    mirror::refresh_requested(&h.ctx).await.unwrap();
    let mirrors = [mirror_of(&h), mirror_of(&h).with_file_name("other")];
    let readme = |at: usize| std::fs::read_to_string(mirrors[at].join("README.md")).unwrap();
    assert_eq!(readme(1), "demo\n", "a new repository's mirror is made");
    for mirror in &mirrors {
        std::fs::write(mirror.join("README.md"), "a helper's experiment\n").unwrap();
    }

    h.ctx.mirrors.request_repo(&other);
    mirror::refresh_requested(&h.ctx).await.unwrap();
    assert_eq!(readme(1), "demo\n", "the mirror asked for is refreshed");
    assert_eq!(
        readme(0),
        "a helper's experiment\n",
        "the other is left alone"
    );

    h.ctx.mirrors.request();
    mirror::refresh_requested(&h.ctx).await.unwrap();
    assert_eq!(
        readme(0),
        "demo\n",
        "a request for all refreshes every mirror"
    );
}

#[tokio::test]
async fn the_repositorys_hooks_neither_copy_nor_run() {
    let h = Harness::new().await;
    let ran = h.dir.path().join("hook-ran");
    let hooks = h.repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for name in ["post-checkout", "reference-transaction", "pre-push"] {
        let hook = hooks.join(name);
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    mirror::sync_all(&h.ctx).await.unwrap();
    let mirror = mirror_of(&h);
    assert!(mirror.join("README.md").exists(), "the mirror was made");
    assert!(!git_dir_of(&h).join("hooks").exists(), "no hook was copied");
    let checkout = agent_git(&mirror, &["checkout", "--quiet", "--detach", "HEAD"]);
    assert!(checkout.status.success(), "{checkout:?}");
    assert!(!ran.exists(), "a hook of the repository ran for its mirror");
}

#[tokio::test]
async fn nothing_a_helper_writes_in_the_mirrors_git_directory_runs_as_the_daemon() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    let git_dir = git_dir_of(&h);
    let ran = h.dir.path().join("planted-ran");
    let planted = format!("touch {}", ran.display());
    let source = h.repo.to_str().unwrap().to_owned();
    let hooks = h.dir.path().join("planted-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\n{planted}\n")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (key, value) in [
        ("filter.x.smudge", planted.clone()),
        ("filter.x.process", planted.clone()),
        ("core.fsmonitor", planted.clone()),
        ("core.hooksPath", hooks.display().to_string()),
        ("protocol.ext.allow", "always".to_owned()),
        (
            &format!("url.ext::sh -c '{planted}'.insteadOf"),
            source.clone(),
        ),
    ] {
        let set = agent_git(&mirror, &["config", key, &value]);
        assert!(set.status.success(), "{set:?}");
    }
    std::fs::create_dir_all(git_dir.join("info")).unwrap();
    std::fs::write(git_dir.join("info/attributes"), "* filter=x\n").unwrap();
    std::fs::write(
        git_dir.join("objects/info/alternates"),
        format!("{source}/.git/objects\n"),
    )
    .unwrap();

    let moved = commit(&h.repo, "README.md", "demo, second edition\n");
    mirror::sync_all(&h.ctx).await.unwrap();
    assert_eq!(head(&mirror), moved, "the refresh went ahead");
    assert_eq!(
        std::fs::read_to_string(mirror.join("README.md")).unwrap(),
        "demo, second edition\n"
    );
    assert!(!ran.exists(), "a command the helper planted ran");
    let config = std::fs::read_to_string(git_dir.join("config")).unwrap();
    assert!(
        !config.contains("touch"),
        "the config was rewritten: {config}"
    );
    assert!(!git_dir.join("info/attributes").exists());
    assert!(!git_dir.join("objects/info/alternates").exists());
    assert!(
        system_notes(&h).await.is_empty(),
        "a good refresh is silent"
    );
}

#[tokio::test]
async fn a_push_from_a_mirror_or_its_worktree_is_refused() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    let elsewhere = h.dir.path().join("elsewhere.git");
    git(
        h.dir.path(),
        &["init", "--quiet", "--bare", elsewhere.to_str().unwrap()],
    );
    let child = h.dir.path().join("isolated-child");
    let added = agent_git(
        &mirror,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            child.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{added:?}");
    let top = stdout(&agent_git(&mirror, &["rev-parse", "--show-toplevel"]));
    assert_eq!(Path::new(&top), mirror.canonicalize().unwrap());
    let moved = commit(&h.repo, "README.md", "demo, second edition\n");
    mirror::sync_all(&h.ctx).await.unwrap();
    assert_eq!(head(&mirror), moved);
    let listed = stdout(&agent_git(&mirror, &["worktree", "list", "--porcelain"]));
    assert!(
        listed.contains(child.to_str().unwrap()),
        "a refresh keeps worktrees: {listed}"
    );
    let status = agent_git(&child, &["status", "--porcelain"]);
    assert!(status.status.success(), "{status:?}");

    assert!(
        !agent_git(&mirror, &["push"]).status.success(),
        "nowhere to push"
    );
    for from in [&mirror, &child] {
        for target in [&elsewhere, &h.repo] {
            let push = agent_git(
                from,
                &["push", target.to_str().unwrap(), "HEAD:refs/heads/leak"],
            );
            let said = String::from_utf8_lossy(&push.stderr);
            assert!(!push.status.success(), "pushed from {from:?} to {target:?}");
            assert!(said.contains("this mirror is read-only"), "{said}");
        }
    }
    for target in [&elsewhere, &h.repo] {
        let leaked = git(target, &["for-each-ref", "refs/heads/leak"]);
        assert_eq!(leaked, "", "{target:?} received a branch");
    }
}

#[tokio::test]
async fn a_link_where_a_mirror_belongs_is_replaced_and_never_followed() {
    let h = Harness::new().await;
    git(
        &h.repo,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/demo.git",
        ],
    );
    let mirror = mirror_of(&h);
    std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&h.repo, &mirror).unwrap();
    mirror::sync_all(&h.ctx).await.unwrap();
    let kind = std::fs::symlink_metadata(&mirror).unwrap().file_type();
    assert!(
        kind.is_dir() && !kind.is_symlink(),
        "a real mirror took the link's place"
    );

    // A `.git` naming the repository, or a git directory of the old layout,
    // is replaced by one naming the mirror's own.
    let own = format!("gitdir: {}\n", git_dir_of(&h).display());
    let real = format!("gitdir: {}\n", h.repo.join(".git").display());
    std::fs::write(mirror.join(".git"), real).unwrap();
    mirror::sync_all(&h.ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(mirror.join(".git")).unwrap(), own);
    std::fs::remove_file(mirror.join(".git")).unwrap();
    git(&mirror, &["init", "--quiet"]);
    mirror::sync_all(&h.ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(mirror.join(".git")).unwrap(), own);

    assert_eq!(stdout(&agent_git(&mirror, &["remote"])), "");
    assert_eq!(
        git(&h.repo, &["remote"]),
        "origin",
        "the repository kept its remote"
    );
    assert_eq!(git(&h.repo, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(
        git(&h.repo, &["status", "--porcelain", "--untracked-files=no"]),
        ""
    );
}

#[tokio::test]
async fn a_failed_refresh_is_told_once_and_so_is_its_recovery() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    let before = head(&mirror);
    let away = h.dir.path().join("moved-away");
    std::fs::rename(&h.repo, &away).unwrap();
    mirror::sync_all(&h.ctx)
        .await
        .expect("a failed refresh does not fail the pass");
    mirror::sync_all(&h.ctx).await.unwrap();

    let notes = system_notes(&h).await;
    assert_eq!(notes.len(), 1, "one note for one problem: {notes:?}");
    assert!(notes[0].contains("could not refresh the orchestrator's mirror of `demo`"));
    let news = mirror_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    assert_eq!(news[0].0, "stale");
    assert!(
        !triggers::due(&h.ctx).await.unwrap(),
        "it does not wake the orchestrator"
    );
    assert_eq!(
        head(&mirror),
        before,
        "the mirror stays readable where it was"
    );

    std::fs::rename(&away, &h.repo).unwrap();
    mirror::sync_all(&h.ctx).await.unwrap();
    let notes = system_notes(&h).await;
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[1].contains("mirror of `demo` is current again"),
        "{}",
        notes[1]
    );
    let states: Vec<String> = mirror_news(&h)
        .await
        .into_iter()
        .map(|(state, _)| state)
        .collect();
    assert_eq!(states, ["stale", "current"]);
}

#[tokio::test]
async fn a_repository_that_refuses_the_orchestrators_model_gets_no_mirror() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    assert!(mirror.exists());
    let model = &h.ctx.config.orchestrator.model;
    h.repo_config(&format!("[models]\ndeny = [\"{model}\"]\n"));
    mirror::sync_all(&h.ctx).await.unwrap();
    assert!(
        !mirror.exists(),
        "its code is not put before a refused model"
    );
    let notes = system_notes(&h).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("gets no mirror of `demo`"),
        "{}",
        notes[0]
    );
    assert_eq!(mirror_news(&h).await[0].0, "withheld");

    h.repo_config("");
    mirror::sync_all(&h.ctx).await.unwrap();
    assert!(mirror.join("README.md").exists(), "the mirror is back");
}

#[tokio::test]
async fn unregistering_removes_the_mirror_and_whatever_else_lies_there() {
    let h = synced().await;
    let mirror = mirror_of(&h);
    let root = mirror.parent().unwrap().to_owned();
    let repo = h.ctx.store.lock().await.repos().unwrap().remove(0);
    let state_key = format!("mirror_{}", repo.id);
    assert!(h.kv(&state_key).await.is_some(), "its state is remembered");
    std::fs::create_dir_all(root.join(".new-demo/.git")).unwrap();
    std::os::unix::fs::symlink(&h.repo, root.join("stray")).unwrap();

    let remove = Request::RepoRemove {
        repo: RepoRef(REPO_NAME.into()),
    };
    h.cli(remove).await.expect("unregister");
    mirror::sync_all(&h.ctx).await.unwrap();
    let left: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    assert!(!git_dir_of(&h).exists(), "its git directory went too");
    assert_eq!(h.kv(&state_key).await, None);
    assert!(
        h.repo.join("README.md").exists(),
        "the stray link was not followed"
    );
}
