use super::*;

use std::path::PathBuf;

fn run(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@archon.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    wt: PathBuf,
    shadow: Shadow,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/App.css"), "a { color: red }\n").unwrap();
        std::fs::write(repo.join("notes.md"), "one\ntwo\nthree\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "--quiet", "-m", "base"]);
        let wt = root.join("wt");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );
        let shadow = Shadow::of(&root.join("data"), &repo).unwrap();
        Self {
            _dir: dir,
            root,
            repo,
            wt,
            shadow,
        }
    }

    fn g(&self) -> NoteGit<'_> {
        NoteGit {
            shadow: &self.shadow,
            repo: &self.repo,
            worktree: &self.wt,
            branch: "feature",
        }
    }

    fn commit(&self, file: &str, text: &str, message: &str) -> String {
        std::fs::write(self.wt.join(file), text).unwrap();
        run(&self.wt, &["add", file]);
        run(&self.wt, &["commit", "--quiet", "-m", message]);
        run(&self.wt, &["rev-parse", "HEAD"])
    }

    fn state(&self) -> (String, String, String) {
        (
            run(&self.wt, &["rev-parse", "HEAD"]),
            run(&self.wt, &["ls-files", "-s"]),
            run(&self.wt, &["diff", "--stat"]),
        )
    }
}

fn who() -> Identity {
    Identity {
        name: "Archon".into(),
        email: "archon@localhost".into(),
    }
}

fn tree_of(repo: &Path, commit: &str) -> String {
    run(repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}

#[test]
fn shape_accepts_exactly_one_commit_on_the_base() {
    let f = Fixture::new();
    let base = head(&f.g()).unwrap();
    assert_eq!(check_shape(&f.g(), &base, None).unwrap(), Shape::Accepted);
    let c1 = f.commit("src/App.css", "a { color: blue }\n", "blue");
    assert_eq!(
        check_shape(&f.g(), &base, Some(&c1)).unwrap(),
        Shape::Accepted
    );
    assert!(matches!(
        check_shape(&f.g(), &base, None).unwrap(),
        Shape::HistoryChanged(_)
    ));
    assert!(matches!(
        check_shape(&f.g(), &base, Some(&base)).unwrap(),
        Shape::HistoryChanged(_)
    ));
    let c2 = f.commit("notes.md", "x\n", "more");
    assert!(matches!(
        check_shape(&f.g(), &base, Some(&c2)).unwrap(),
        Shape::HistoryChanged(_)
    ));
    // Reset below base plus one commit: parent is not base.
    run(&f.wt, &["reset", "--quiet", "--hard", &base]);
    run(&f.wt, &["commit", "--quiet", "--allow-empty", "-m", "e1"]);
    let e1 = head(&f.g()).unwrap();
    run(&f.wt, &["commit", "--quiet", "--allow-empty", "-m", "e2"]);
    let e2 = head(&f.g()).unwrap();
    assert!(matches!(
        check_shape(&f.g(), &e1, Some(&e2)).unwrap(),
        Shape::Accepted
    ));
    assert!(matches!(
        check_shape(&f.g(), &base, Some(&e2)).unwrap(),
        Shape::HistoryChanged(_)
    ));
    // A merge commit.
    run(&f.wt, &["checkout", "--quiet", "-b", "side", &base]);
    run(&f.wt, &["commit", "--quiet", "--allow-empty", "-m", "s"]);
    run(&f.wt, &["checkout", "--quiet", "feature"]);
    run(&f.wt, &["merge", "--quiet", "--no-ff", "-m", "m", "side"]);
    let m = head(&f.g()).unwrap();
    let Shape::HistoryChanged(why) = check_shape(&f.g(), &base, Some(&m)).unwrap() else {
        panic!("a merge is refused")
    };
    assert!(why.contains("merge"));
}

#[test]
fn tidy_names_the_fix() {
    let f = Fixture::new();
    assert_eq!(check_tidy(&f.g(), None).unwrap(), Tidy::Tidy);
    std::fs::write(f.wt.join("src/App.css"), "changed\n").unwrap();
    let Tidy::Untidy(text) = check_tidy(&f.g(), None).unwrap() else {
        panic!()
    };
    assert!(
        text.contains("uncommitted changes in src/App.css"),
        "{text}"
    );
    run(&f.wt, &["checkout", "--quiet", "--", "src/App.css"]);
    std::fs::write(f.wt.join("notes.txt"), "x").unwrap();
    let Tidy::Untidy(text) = check_tidy(&f.g(), None).unwrap() else {
        panic!()
    };
    assert!(text.contains("untracked file notes.txt"), "{text}");
    std::fs::remove_file(f.wt.join("notes.txt")).unwrap();
    std::fs::create_dir_all(f.wt.join(".archon/x")).unwrap();
    std::fs::write(f.wt.join(".archon/x/y"), "x").unwrap();
    assert_eq!(check_tidy(&f.g(), None).unwrap(), Tidy::Tidy);
    let Tidy::Untidy(text) = check_tidy(&f.g(), Some("D1")).unwrap() else {
        panic!()
    };
    assert!(text.contains("git commit --amend"), "{text}");
    run(
        &f.wt,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "x\n\nArchon-Note: D1",
        ],
    );
    assert_eq!(check_tidy(&f.g(), Some("D1")).unwrap(), Tidy::Tidy);
    assert!(matches!(
        check_tidy(&f.g(), Some("D2")).unwrap(),
        Tidy::Untidy(_)
    ));
}

#[test]
fn the_revert_is_what_git_revert_makes() {
    let f = Fixture::new();
    let c = f.commit("src/App.css", "a { color: blue }\n", "Make it blue");
    f.commit("notes.md", "one\ntwo\nthree\nfour\n", "later work");
    let before = head(&f.g()).unwrap();
    let RevertPrep::Ready { pre, new } = prepare_revert(&f.g(), &c, "D2", &who()).unwrap() else {
        panic!("ready")
    };
    assert_eq!(pre, before);
    assert_eq!(head(&f.g()).unwrap(), pre, "preparing moves nothing");

    // Porcelain only in a scratch clone.
    let scratch = f.root.join("scratch");
    run(
        &f.root,
        &["clone", "--quiet", f.repo.to_str().unwrap(), "scratch"],
    );
    run(&scratch, &["checkout", "--quiet", "-B", "x", &pre]);
    run(&scratch, &["revert", "--no-edit", &c]);
    let theirs = run(&scratch, &["rev-parse", "HEAD"]);
    assert_eq!(tree_of(&f.repo, &new), tree_of(&scratch, &theirs));
    let expected = format!(
        "{}\n\nArchon-Note: D2",
        run(&scratch, &["log", "-1", "--format=%B", "HEAD"])
    );
    assert_eq!(run(&f.repo, &["log", "-1", "--format=%B", &new]), expected);
    assert_eq!(
        expected,
        format!("Revert \"Make it blue\"\n\nThis reverts commit {c}.\n\nArchon-Note: D2")
    );
    assert_eq!(
        run(&f.repo, &["rev-list", "--parents", "-n1", &new]),
        format!("{new} {pre}")
    );
    assert_eq!(
        run(&f.repo, &["log", "-1", "--format=%an <%ae>", &new]),
        "Archon <archon@localhost>"
    );

    assert_eq!(
        apply_revert(&f.g(), &pre, &new).unwrap(),
        RevertApplied::Applied
    );
    assert_eq!(head(&f.g()).unwrap(), new);
    assert_eq!(
        std::fs::read_to_string(f.wt.join("src/App.css")).unwrap(),
        "a { color: red }\n"
    );
    assert_eq!(check_tidy(&f.g(), Some("D2")).unwrap(), Tidy::Tidy);
}

#[test]
fn a_conflict_writes_nothing() {
    let f = Fixture::new();
    let c = f.commit("src/App.css", "a { color: blue }\n", "blue");
    f.commit("src/App.css", "a { color: green }\n", "green");
    let before = f.state();
    let RevertPrep::Conflict(files) = prepare_revert(&f.g(), &c, "D2", &who()).unwrap() else {
        panic!("conflict")
    };
    assert_eq!(files, ["src/App.css"]);
    assert_eq!(f.state(), before);
    assert_eq!(
        std::fs::read_to_string(f.wt.join("src/App.css")).unwrap(),
        "a { color: green }\n"
    );
}

#[test]
fn a_moved_branch_is_history_changed() {
    let f = Fixture::new();
    let c = f.commit("src/App.css", "a { color: blue }\n", "blue");
    let RevertPrep::Ready { pre, new } = prepare_revert(&f.g(), &c, "D2", &who()).unwrap() else {
        panic!()
    };
    run(
        &f.wt,
        &["commit", "--quiet", "--allow-empty", "-m", "agent moved on"],
    );
    let state = f.state();
    assert!(matches!(
        apply_revert(&f.g(), &pre, &new).unwrap(),
        RevertApplied::HistoryChanged(_)
    ));
    assert_eq!(f.state(), state, "files and branch stay as they were");
}

#[test]
fn a_dirty_worktree_or_a_foreign_commit_is_refused() {
    let f = Fixture::new();
    let c = f.commit("src/App.css", "a { color: blue }\n", "blue");
    std::fs::write(f.wt.join("notes.md"), "dirty\n").unwrap();
    assert_eq!(
        prepare_revert(&f.g(), &c, "D2", &who()).unwrap(),
        RevertPrep::Refused("the worktree has uncommitted changes".into())
    );
    run(&f.wt, &["checkout", "--quiet", "--", "notes.md"]);
    std::fs::write(f.wt.join("stray.txt"), "x").unwrap();
    assert!(matches!(
        prepare_revert(&f.g(), &c, "D2", &who()).unwrap(),
        RevertPrep::Refused(_)
    ));
    std::fs::remove_file(f.wt.join("stray.txt")).unwrap();
    // A commit no longer in the history.
    let base = run(&f.wt, &["rev-parse", "HEAD~1"]);
    run(&f.wt, &["reset", "--quiet", "--hard", &base]);
    assert_eq!(
        prepare_revert(&f.g(), &c, "D2", &who()).unwrap(),
        RevertPrep::Refused(format!("{c}'s change is no longer in the code history"))
    );
}

/// Hooks, fsmonitor, a clean/smudge filter and a merge driver, each touching
/// a marker of its own.
fn plant(f: &Fixture) -> PathBuf {
    let marks = f.root.join("marks");
    std::fs::create_dir(&marks).unwrap();
    let touch = |name: &str| format!("touch {}", marks.join(name).display());
    let hooks = f.repo.join(".git/hooks");
    for hook in [
        "pre-commit",
        "post-checkout",
        "post-commit",
        "post-merge",
        "reference-transaction",
        "pre-merge-commit",
        "commit-msg",
        "post-index-change",
    ] {
        let path = hooks.join(hook);
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", touch(hook))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let set = |k: &str, v: String| run(&f.repo, &["config", k, &v]);
    set("core.fsmonitor", touch("fsmonitor"));
    set("filter.planted.clean", touch("clean"));
    set("filter.planted.smudge", touch("smudge"));
    set(
        "merge.planted.driver",
        format!("{} && false", touch("driver")),
    );
    std::fs::write(
        f.repo.join(".git/info/attributes"),
        "* filter=planted\n* merge=planted\n",
    )
    .unwrap();
    marks
}

#[test]
fn planted_programs_never_run() {
    let f = Fixture::new();
    let c = f.commit("src/App.css", "a { color: blue }\n", "blue");
    f.commit("notes.md", "one\ntwo\nthree\nfour\n", "later");
    let marks = plant(&f);
    let g = f.g();
    head(&g).unwrap();
    let base = run(&f.repo, &["rev-parse", &format!("{c}^")]);
    check_shape(&g, &base, Some(&c)).unwrap();
    check_tidy(&g, Some("D1")).unwrap();
    std::fs::write(f.wt.join("src/App.css"), "dirty\n").unwrap();
    check_tidy(&g, None).unwrap();
    run_restore(&f);
    let prep = prepare_revert(&g, &c, "D2", &who()).unwrap();
    let RevertPrep::Ready { pre, new } = prep else {
        panic!("{prep:?}")
    };
    apply_revert(&g, &pre, &new).unwrap();
    reconcile_revert(&g, &pre, &new).unwrap();
    let ran: Vec<_> = std::fs::read_dir(&marks)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(ran.is_empty(), "programs ran: {ran:?}");
}

/// Put `src/App.css` back to HEAD's without any planted filter (the file
/// is written directly).
fn run_restore(f: &Fixture) {
    let blob = DaemonGit::sealed_in(&f.wt)
        .args(["show", "HEAD:src/App.css"])
        .output()
        .unwrap()
        .stdout;
    std::fs::write(f.wt.join("src/App.css"), blob).unwrap();
}

/// What `stack::advance` does before its ref update.
fn checkout(f: &Fixture, from: &str, to: &str) {
    let index = clean::index_of(&f.wt).unwrap();
    f.shadow
        .git(Some(&f.wt), Config::Operator)
        .unwrap()
        .env("GIT_INDEX_FILE", index.to_str().unwrap())
        .args(["read-tree", "-m", "-u", from, to])
        .text()
        .unwrap();
}

#[test]
fn reconcile_settles_each_state() {
    let f = Fixture::new();
    let c = f.commit("src/App.css", "a { color: blue }\n", "blue");
    let RevertPrep::Ready { pre, new } = prepare_revert(&f.g(), &c, "D2", &who()).unwrap() else {
        panic!()
    };

    // Nothing applied.
    assert_eq!(
        reconcile_revert(&f.g(), &pre, &new).unwrap(),
        Reconciled::NotApplied
    );
    assert_eq!(head(&f.g()).unwrap(), pre);

    // Crash after the checkout, before the ref update.
    checkout(&f, &pre, &new);
    assert_eq!(head(&f.g()).unwrap(), pre);
    assert_eq!(
        reconcile_revert(&f.g(), &pre, &new).unwrap(),
        Reconciled::Completed
    );
    assert_eq!(head(&f.g()).unwrap(), new);
    assert_eq!(check_tidy(&f.g(), None).unwrap(), Tidy::Tidy);

    // Already done.
    assert_eq!(
        reconcile_revert(&f.g(), &pre, &new).unwrap(),
        Reconciled::Done
    );

    // Back to the crash state, then the agent edits the files.
    run(&f.repo, &["update-ref", "refs/heads/feature", &pre, &new]);
    checkout(&f, &new, &pre);
    std::fs::write(f.wt.join("notes.md"), "edited\n").unwrap();
    assert!(matches!(
        reconcile_revert(&f.g(), &pre, &new).unwrap(),
        Reconciled::HistoryChanged(_)
    ));
    assert_eq!(
        std::fs::read_to_string(f.wt.join("notes.md")).unwrap(),
        "edited\n"
    );

    // HEAD somewhere else entirely.
    run(&f.wt, &["checkout", "--quiet", "--", "notes.md"]);
    run(
        &f.wt,
        &["commit", "--quiet", "--allow-empty", "-m", "elsewhere"],
    );
    assert!(matches!(
        reconcile_revert(&f.g(), &pre, &new).unwrap(),
        Reconciled::HistoryChanged(_)
    ));
}
