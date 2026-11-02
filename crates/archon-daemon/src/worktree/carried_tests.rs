//! Real repositories: `item` forks from `main` and is reviewed at its head;
//! `main` then moves, and commits land on `item` after the review.

use std::path::{Path, PathBuf};

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    shadow: Shadow,
    /// The fork of `item` from `main`.
    fork: String,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@archon.invalid"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    let shadow = Shadow::of(&dir.path().join("data"), &repo).unwrap();
    let f = Fixture {
        _dir: dir,
        repo,
        shadow,
        fork: String::new(),
    };
    let fork = f.commit("shared.txt", "one\ntwo\nthree\n");
    git(&f.repo, &["branch", "item"]);
    Fixture { fork, ..f }
}

impl Fixture {
    fn commit(&self, file: &str, content: &str) -> String {
        std::fs::write(self.repo.join(file), content).unwrap();
        git(&self.repo, &["add", file]);
        git(&self.repo, &["commit", "--quiet", "-m", file]);
        self.head()
    }

    fn on(&self, branch: &str) {
        git(&self.repo, &["checkout", "--quiet", branch]);
    }

    fn head(&self) -> String {
        git(&self.repo, &["rev-parse", "HEAD"])
    }

    fn tree(&self, commit: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{commit}^{{tree}}")])
    }

    /// A merge commit of `tree` over `ours` and `theirs`, as archond makes one.
    fn merge_commit(&self, tree: &str, ours: &str, theirs: &str) -> String {
        let message = format!("Merge commit '{theirs}' into item");
        let args = [
            "commit-tree",
            tree,
            "-p",
            ours,
            "-p",
            theirs,
            "-m",
            &message,
        ];
        git(&self.repo, &args)
    }

    /// `main` merged into `item` as `git merge` does, with no conflict.
    fn merge_main(&self) -> String {
        self.on("item");
        git(&self.repo, &["merge", "--quiet", "--no-edit", "main"]);
        self.head()
    }

    fn check(&self, reviewed: &str, head: &str, base: &str) -> Result<String, String> {
        base_merges(&self.shadow, &self.repo, (reviewed, head), base).unwrap()
    }
}

/// The reviewed head of `item`: one change of its own to `shared.txt`.
fn reviewed(f: &Fixture) -> String {
    f.on("item");
    f.commit("shared.txt", "one\nTWO\nthree\n")
}

#[test]
fn a_clean_merge_of_the_base_carries_the_review_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    f.on("main");
    let base = f.commit("other.txt", "other\n");
    let head = f.merge_main();

    let why = f.check(&reviewed, &head, &base).expect("carried over");
    assert!(why.contains("with no conflict"), "{why}");
    assert!(
        why.contains(&head[..7]) && why.contains(&base[..7]),
        "{why}"
    );
}

#[test]
fn two_base_merges_in_a_row_carry_it_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    f.on("main");
    f.commit("other.txt", "other\n");
    f.merge_main();
    f.on("main");
    let base = f.commit("more.txt", "more\n");
    let head = f.merge_main();

    let why = f.check(&reviewed, &head, &base).expect("carried over");
    assert_eq!(why.matches("with no conflict").count(), 2, "{why}");
}

/// A dependency squash-merged into the base, which `item` holds and edits
/// further: git's merge conflicts, so archond keeps the branch's tree.
#[test]
fn a_squash_landing_merged_with_the_branchs_tree_carries_it_over() {
    let f = fixture();
    f.on("item");
    let dep = f.commit("dep.txt", "dep\n");
    let reviewed = f.commit("dep.txt", "dep, edited by the item\n");
    f.on("main");
    git(&f.repo, &["checkout", "--quiet", &dep, "--", "dep.txt"]);
    git(&f.repo, &["commit", "--quiet", "-m", "Dep (#1)"]);
    let squash = f.head();
    assert_eq!(f.tree(&squash), f.tree(&dep), "an unmoved base's squash");
    let merged = stack::merge_tree(&f.shadow, None, &reviewed, &squash).unwrap();
    assert!(merged.is_err(), "git's own merge conflicts");
    let head = f.merge_commit(&f.tree(&reviewed), &reviewed, &squash);

    let why = f.check(&reviewed, &head, &squash).expect("carried over");
    assert!(why.contains("already held"), "{why}");
}

/// A recreated base branch, new commits of the same content, merged with the
/// branch's tree.
#[test]
fn a_recreated_base_of_the_same_tree_carries_it_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    let message = ["-m", "recreated"];
    let tree = f.tree(&f.fork);
    let recreated = git(&f.repo, &["commit-tree", &tree, message[0], message[1]]);
    let head = f.merge_commit(&f.tree(&reviewed), &reviewed, &recreated);

    assert!(f.check(&reviewed, &head, &recreated).is_ok());
}

#[test]
fn a_merge_that_resolves_a_conflict_is_not_carried_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    f.on("main");
    let base = f.commit("shared.txt", "one\nzwei\nthree\n");
    let resolved = f.commit("shared.txt", "one\nresolved\nthree\n");
    let tree = f.tree(&resolved);
    let head = f.merge_commit(&tree, &reviewed, &base);

    let why = f.check(&reviewed, &head, &base).expect_err("refused");
    assert!(why.contains("resolves a conflict"), "{why}");
}

#[test]
fn a_merge_that_adds_a_change_is_not_carried_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    f.on("main");
    let base = f.commit("other.txt", "other\n");
    let clean = f.merge_main();
    f.commit("sneaked.txt", "never reviewed\n");
    git(&f.repo, &["reset", "--quiet", "--soft", &clean]);
    let added = git(&f.repo, &["write-tree"]);
    let head = f.merge_commit(&added, &reviewed, &base);

    let why = f.check(&reviewed, &head, &base).expect_err("refused");
    assert!(why.contains("adds a change"), "{why}");
}

#[test]
fn a_plain_commit_after_the_review_is_not_carried_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    let base = f.fork.clone();
    let head = f.commit("more.txt", "more\n");

    let why = f.check(&reviewed, &head, &base).expect_err("refused");
    assert!(why.contains("not a merge"), "{why}");
}

/// A merge of a branch the base does not hold brings in unread commits.
#[test]
fn a_merge_of_a_commit_off_the_base_is_not_carried_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    git(&f.repo, &["checkout", "--quiet", "-b", "side", &f.fork]);
    f.commit("side.txt", "side\n");
    let head = {
        f.on("item");
        git(&f.repo, &["merge", "--quiet", "--no-edit", "side"]);
        f.head()
    };

    let why = f.check(&reviewed, &head, &f.fork).expect_err("refused");
    assert!(why.contains("the base does not hold"), "{why}");
}

/// A plain commit beneath a base merge is found too.
#[test]
fn a_plain_commit_before_a_base_merge_is_not_carried_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    f.commit("more.txt", "more\n");
    f.on("main");
    let base = f.commit("other.txt", "other\n");
    let head = f.merge_main();

    let why = f.check(&reviewed, &head, &base).expect_err("refused");
    assert!(why.contains("not a merge"), "{why}");
}

#[test]
fn a_rewritten_branch_is_not_carried_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    git(&f.repo, &["commit", "--quiet", "--amend", "-m", "amended"]);
    let head = f.head();

    let why = f.check(&reviewed, &head, &f.fork).expect_err("refused");
    assert!(why.contains("rewritten"), "{why}");
}
