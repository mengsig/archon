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
        self.check_stacked(reviewed, head, base, &[])
    }

    /// The check of an item stacked on the parent heads `on`.
    fn check_stacked(
        &self,
        reviewed: &str,
        head: &str,
        base: &str,
        on: &[&str],
    ) -> Result<String, String> {
        let on: Vec<String> = on.iter().map(|c| (*c).to_owned()).collect();
        base_merges(&self.shadow, &self.repo, (reviewed, head), base, &on).unwrap()
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

/// A stack parent `dep` squash-merged into the base, which `item` holds and
/// edits further: git's merge conflicts, so archond keeps the branch's tree.
/// `dep` is the parent's head, one commit after the fork.
fn squash_landing(f: &Fixture) -> (String, String, String, String) {
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
    (dep, reviewed, squash, head)
}

#[test]
fn a_stack_parents_squash_merged_with_the_branchs_tree_carries_it_over() {
    let f = fixture();
    let (dep, reviewed, squash, head) = squash_landing(&f);

    let why = f.check_stacked(&reviewed, &head, &squash, &[&dep]);
    let why = why.expect("carried over");
    assert!(
        why.contains("stack parent") && why.contains("changes no file"),
        "{why}"
    );
}

/// The same shape where no stack parent is recorded at that content.
#[test]
fn a_squash_of_no_recorded_stack_parent_is_not_carried_over() {
    let f = fixture();
    let (dep, reviewed, squash, head) = squash_landing(&f);
    let elsewhere = git(&f.repo, &["commit-tree", &f.tree(&dep), "-m", "unread"]);

    for on in [vec![], vec![elsewhere.as_str()], vec![reviewed.as_str()]] {
        let why = f.check_stacked(&reviewed, &head, &squash, &on);
        let why = why.expect_err("refused");
        assert!(why.contains("resolves a conflict"), "{on:?}: {why}");
    }
}

/// A commit of the branch whose content matches a base commit is no landing:
/// keeping the branch's tree over it would land a later deletion unread.
#[test]
fn a_base_merge_kept_as_the_branch_over_a_matching_tree_is_not_carried_over() {
    let f = fixture();
    f.on("main");
    let added = f.commit("y.txt", "on the base\n");
    f.on("item");
    git(&f.repo, &["checkout", "--quiet", &added, "--", "y.txt"]);
    git(&f.repo, &["commit", "--quiet", "-m", "same as the base"]);
    git(&f.repo, &["rm", "--quiet", "y.txt"]);
    git(&f.repo, &["commit", "--quiet", "-m", "drop y"]);
    let reviewed = f.commit("f.txt", "the reviewed change\n");
    git(
        &f.repo,
        &["merge", "--quiet", "--no-edit", "-s", "ours", &added],
    );
    let head = f.head();

    let why = f.check(&reviewed, &head, &added).expect_err("refused");
    assert!(why.contains("resolves a conflict"), "{why}");
}

/// A recreated base branch, a new commit of the same content over the old,
/// merged in as git does.
#[test]
fn a_recreated_base_of_the_same_tree_carries_it_over() {
    let f = fixture();
    let reviewed = reviewed(&f);
    let tree = f.tree(&f.fork);
    let recreated = git(
        &f.repo,
        &["commit-tree", &tree, "-p", &f.fork, "-m", "recreated"],
    );
    let head = f.merge_commit(&f.tree(&reviewed), &reviewed, &recreated);

    let why = f.check(&reviewed, &head, &recreated).expect("carried over");
    assert!(why.contains("with no conflict"), "{why}");
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
