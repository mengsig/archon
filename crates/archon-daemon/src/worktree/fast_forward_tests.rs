use super::*;

fn git_in(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["-c", "user.name=t", "-c", "user.email=t@t.invalid"];
    all.extend(["-c", "commit.gpgsign=false"]);
    all.extend(args);
    git(dir, &all).expect("git")
}

fn commit_file(repo: &Path, name: &str, text: &str) -> String {
    std::fs::write(repo.join(name), text).expect("write");
    git_in(repo, &["add", name]);
    git_in(repo, &["commit", "--quiet", "-m", name]);
    git_in(repo, &["rev-parse", "HEAD"])
}

/// A repository whose `main` is checked out one commit behind its
/// `origin/main`; returns the old and the new commit.
fn behind(repo: &Path) -> (String, String) {
    git_in(repo, &["init", "--quiet", "--initial-branch=main"]);
    let old = commit_file(repo, "a.txt", "one\n");
    let new = commit_file(repo, "b.txt", "two\n");
    git_in(repo, &["update-ref", "refs/remotes/origin/main", &new]);
    git_in(repo, &["reset", "--quiet", "--hard", &old]);
    (old, new)
}

fn main_of(repo: &Path) -> String {
    git_in(repo, &["rev-parse", "refs/heads/main"])
}

fn kind(advance: &Advance) -> &'static str {
    match advance {
        Advance::Kept(kept) => kept.kind,
        other => panic!("kept: {other:?}"),
    }
}

#[test]
fn a_clean_checkout_behind_origin_is_fast_forwarded_with_its_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let (old, new) = behind(repo);

    let advance = fast_forward(repo, "main").expect("git");

    assert_eq!(
        advance,
        Advance::Moved {
            from: old,
            to: new.clone()
        }
    );
    assert_eq!(main_of(repo), new);
    assert_eq!(
        std::fs::read_to_string(repo.join("b.txt")).unwrap(),
        "two\n"
    );
    assert!(
        dirty(repo).expect("status").is_none(),
        "nothing left changed"
    );
    assert_eq!(fast_forward(repo, "main").expect("git"), Advance::Current);
}

#[test]
fn a_base_no_work_tree_has_checked_out_moves_its_ref_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let (old, new) = behind(repo);
    git_in(repo, &["checkout", "--quiet", "-b", "other"]);

    let advance = fast_forward(repo, "main").expect("git");

    assert_eq!(
        advance,
        Advance::Moved {
            from: old.clone(),
            to: new.clone()
        }
    );
    assert_eq!(main_of(repo), new);
    assert_eq!(git_in(repo, &["rev-parse", "HEAD"]), old, "other untouched");
}

#[test]
fn a_checkout_with_local_changes_is_left_with_the_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let (old, _) = behind(repo);
    std::fs::write(repo.join("a.txt"), "edited\n").unwrap();

    let advance = fast_forward(repo, "main").expect("git");

    assert_eq!(kind(&advance), "dirty");
    let Advance::Kept(kept) = advance else {
        unreachable!()
    };
    assert!(kept.why.contains("1 changed file"), "{}", kept.why);
    assert_eq!(main_of(repo), old);
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "edited\n"
    );
}

#[test]
fn an_untracked_file_the_fast_forward_would_overwrite_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let (old, _) = behind(repo);
    std::fs::write(repo.join("b.txt"), "mine\n").unwrap();

    let advance = fast_forward(repo, "main").expect("git");

    assert_eq!(kind(&advance), "refused");
    assert_eq!(main_of(repo), old);
    assert_eq!(
        std::fs::read_to_string(repo.join("b.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_base_ahead_of_or_diverged_from_origin_is_never_moved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let (_, new) = behind(repo);
    git_in(repo, &["reset", "--quiet", "--hard", &new]);
    let ahead = commit_file(repo, "c.txt", "three\n");
    assert_eq!(kind(&fast_forward(repo, "main").expect("git")), "ahead");
    assert_eq!(main_of(repo), ahead);

    git_in(repo, &["reset", "--quiet", "--hard", "HEAD~2"]);
    let diverged = commit_file(repo, "d.txt", "four\n");
    assert_eq!(kind(&fast_forward(repo, "main").expect("git")), "diverged");
    assert_eq!(main_of(repo), diverged);
}

#[test]
fn a_base_being_bisected_is_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    let (old, _) = behind(repo);
    git_in(repo, &["bisect", "start"]);

    assert_eq!(kind(&fast_forward(repo, "main").expect("git")), "busy");
    assert_eq!(main_of(repo), old);
}
