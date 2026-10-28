//! Real repositories: a dependency `dep` forks from `main`, a dependent
//! `child` forks from `dep` and rewrites the dependency's file, and `main`
//! then takes the dependency as a squash, a rebase or a real merge.

use std::path::{Path, PathBuf};

use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    shadow: Shadow,
    dep: String,
    child: String,
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

fn commit(repo: &Path, file: &str, content: &str) -> String {
    std::fs::write(repo.join(file), content).unwrap();
    git(repo, &["add", file]);
    git(repo, &["commit", "--quiet", "-m", file]);
    git(repo, &["rev-parse", "HEAD"])
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    commit(&repo, "base.txt", "base\n");
    git(&repo, &["checkout", "--quiet", "-b", "dep"]);
    commit(&repo, "dep.txt", "one\n");
    let dep = commit(&repo, "dep.txt", "one\ntwo\n");
    git(&repo, &["checkout", "--quiet", "-b", "child"]);
    let child = commit(&repo, "dep.txt", "one\ntwo\nchild\n");
    git(&repo, &["checkout", "--quiet", "main"]);
    let shadow = Shadow::of(&dir.path().join("data"), &repo).unwrap();
    Fixture {
        _dir: dir,
        repo,
        shadow,
        dep,
        child,
    }
}

impl Fixture {
    fn main_moves(&self) {
        commit(&self.repo, "other.txt", "other\n");
    }

    fn squash(&self) -> String {
        git(&self.repo, &["merge", "--quiet", "--squash", "dep"]);
        git(&self.repo, &["commit", "--quiet", "-m", "Dep (#1)"]);
        git(&self.repo, &["rev-parse", "HEAD"])
    }

    /// As GitHub rebases: new commits, by another committer.
    fn rebase(&self) -> String {
        let by = ["-c", "user.name=GitHub", "cherry-pick", "main..dep"];
        git(&self.repo, &by);
        git(&self.repo, &["rev-parse", "HEAD"])
    }

    fn find(&self, base: &str) -> Option<Landing> {
        find(&self.shadow, &self.repo, &self.dep, base).unwrap()
    }

    fn merge(&self, landing: &Landing) -> Merge {
        let identity = Identity {
            name: "Archon".into(),
            email: "archon@localhost".into(),
        };
        let dep = (self.dep.as_str(), landing);
        merge(&self.shadow, &self.repo, "child", dep, &identity).unwrap()
    }

    fn plain_merge_conflicts(&self, base: &str) -> bool {
        let identity = Identity {
            name: "Archon".into(),
            email: "archon@localhost".into(),
        };
        let merge = stack::merge(&self.shadow, &self.repo, "child", base, &identity).unwrap();
        matches!(merge, Merge::Conflict(files) if files == "dep.txt")
    }

    /// The file at `to`, and `to`'s parents.
    fn merged(&self, to: &str, file: &str) -> (String, String) {
        let content = git(&self.repo, &["show", &format!("{to}:{file}")]);
        (content, git(&self.repo, &["rev-parse", &format!("{to}^@")]))
    }
}

/// The dependency squashed into an unmoved base: the base's commit has the
/// head's tree, and the child keeps its own tree where a plain merge conflicts.
#[test]
fn a_squash_onto_an_unmoved_base_merges_as_ours_where_a_plain_merge_conflicts() {
    let f = fixture();
    let squash = f.squash();
    assert!(f.plain_merge_conflicts(&squash), "the bug this fixes");

    let landing = f.find(&squash).expect("landed");
    assert_eq!(landing, Landing::SameTree(squash.clone()));
    let Merge::Merged { from, to } = f.merge(&landing) else {
        panic!("a merge commit");
    };
    assert_eq!(from, f.child);
    let tree = |rev: &str| git(&f.repo, &["rev-parse", &format!("{rev}^{{tree}}")]);
    assert_eq!(tree(&to), tree(&f.child), "content-neutral");
    let (_, parents) = f.merged(&to, "dep.txt");
    assert_eq!(parents, format!("{}\n{squash}", f.child));
}

/// The dependency rebased onto a base that had moved: found by patch id,
/// and merged over its head, so only the base's own change comes in.
#[test]
fn a_rebase_after_the_base_moved_merges_over_the_head_without_conflict() {
    let f = fixture();
    f.main_moves();
    let rebased = f.rebase();
    assert!(f.plain_merge_conflicts(&rebased), "the bug this fixes");

    assert_eq!(
        f.find(&rebased),
        Some(Landing::SamePatches(rebased.clone()))
    );
    let Merge::Merged { to, .. } = f.merge(&Landing::SamePatches(rebased.clone())) else {
        panic!("a merge commit");
    };
    let (content, parents) = f.merged(&to, "dep.txt");
    assert_eq!(content, "one\ntwo\nchild", "the child's change is kept");
    assert_eq!(f.merged(&to, "other.txt").0, "other", "the base's comes in");
    assert_eq!(parents, format!("{}\n{rebased}", f.child));
}

/// A rebase onto an unmoved base ends in the head's tree.
#[test]
fn a_rebase_onto_an_unmoved_base_has_the_head_tree() {
    let f = fixture();
    let rebased = f.rebase();
    assert_eq!(f.find(&rebased), Some(Landing::SameTree(rebased)));
}

/// A squash after the base moved has no commit of the head's tree, but one
/// with the head's whole change; a later base commit is no part of it.
#[test]
fn a_squash_after_the_base_moved_is_found_by_its_patch_id() {
    let f = fixture();
    f.main_moves();
    let squash = f.squash();
    let later = commit(&f.repo, "later.txt", "later\n");
    assert_eq!(f.find(&later), Some(Landing::SamePatches(squash.clone())));
    let Merge::Merged { to, .. } = f.merge(&Landing::SamePatches(squash)) else {
        panic!("a merge commit");
    };
    assert_eq!(f.merged(&to, "dep.txt").0, "one\ntwo\nchild");
    assert_eq!(f.merged(&to, "other.txt").0, "other");
    assert!(
        git(&f.repo, &["ls-tree", "--name-only", &to])
            .lines()
            .all(|f| f != "later.txt")
    );
}

/// A base that took the dependency changed, or that holds the head itself,
/// is no landing: a plain merge of the base is what it needs.
#[test]
fn a_changed_squash_or_a_real_merge_is_no_landing() {
    let f = fixture();
    git(&f.repo, &["merge", "--quiet", "--squash", "dep"]);
    let edited = commit(&f.repo, "dep.txt", "one\nTWO\n");
    assert_eq!(f.find(&edited), None);

    let g = fixture();
    git(
        &g.repo,
        &["merge", "--quiet", "--no-ff", "-m", "Merge dep", "dep"],
    );
    let merged = git(&g.repo, &["rev-parse", "HEAD"]);
    assert_eq!(g.find(&merged), None);
}

/// The landing is merged only into a branch that holds the head it stands
/// for, and a branch holding the landing already is left alone.
#[test]
fn a_landing_merges_only_into_a_branch_holding_the_head_and_only_once() {
    let f = fixture();
    let squash = f.squash();
    let landing = Landing::SameTree(squash);
    let Merge::Merged { from, to } = f.merge(&landing) else {
        panic!("a merge commit");
    };
    git(&f.repo, &["update-ref", "refs/heads/child", &to, &from]);
    assert_eq!(f.merge(&landing), Merge::Contained);

    git(&f.repo, &["branch", "stranger", "main~1"]);
    let identity = Identity {
        name: "Archon".into(),
        email: "archon@localhost".into(),
    };
    let dep = (f.dep.as_str(), &landing);
    let refused = merge(&f.shadow, &f.repo, "stranger", dep, &identity).unwrap_err();
    assert!(refused.to_string().contains("does not hold"), "{refused}");
}
