//! A pull request whose head moved after its review merges without a new
//! review when only base merges came since, and is refused otherwise
//! (ARCHITECTURE §18.6). GitHub is a scripted `gh`; `origin` is a bare repo.

use crate::common;
use crate::operator_merge::{
    PR_ONE, delivering, green, merge_calls, merge_item, merge_of, merges, request_merge, shipped,
};

use archon_core::config::Config;
use archon_core::merge::{MergeAuthority, MergeRequest, MergeState};
use archon_core::protocol::Request;
use common::delivery::FakeGh;
use common::{Harness, git};
use serde_json::json;

/// Commit `file` on `main` and push it to `origin`, as a merge into the base.
fn base_moves(h: &Harness, file: &str) -> String {
    std::fs::write(h.repo.join(file), "landed on the base\n").unwrap();
    git(&h.repo, &["add", file]);
    git(&h.repo, &["commit", "--quiet", "-m", "Base change"]);
    git(&h.repo, &["push", "--quiet", "origin", "main"]);
    git(&h.repo, &["rev-parse", "main"])
}

/// A commit of `tree` over `parents`, which no ref names.
fn commit_tree(h: &Harness, tree: &str, parents: &[&str]) -> String {
    let mut args = vec!["commit-tree", tree, "-m", "Merge commit into the branch"];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    git(&h.repo, &args)
}

fn tree(h: &Harness, commit: &str) -> String {
    git(&h.repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// GitHub shows the pull request at `head`, green, over `base` on `main`.
fn github_shows(gh: &FakeGh, head: &str, base: &str) {
    gh.show(head, "MERGEABLE", green());
    let pr = json!({"url": PR_ONE, "baseRefName": "main", "baseRefOid": base});
    gh.headed(1, &pr);
}

fn refusal(merge: &MergeRequest) -> &str {
    match &merge.state {
        MergeState::Refused { reason } => reason,
        other => panic!("refused: {other:?}"),
    }
}

/// Case 2, a refresh in place: the base merged in with no conflict.
#[tokio::test]
async fn a_clean_base_merge_after_the_review_merges_at_the_new_head() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, reviewed) = shipped(&h).await;
    let base = base_moves(&h, "upstream.txt");
    let merged = git(&h.repo, &["merge-tree", "--write-tree", &reviewed, &base]);
    let head = commit_tree(&h, &merged, &[&reviewed, &base]);
    github_shows(&gh, &head, &base);

    let merge = merge_of(h.cli(merge_item(&item, false)).await);
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    assert_eq!(merge.head, head, "the new head merged");
    let carried = merge.carried.as_ref().expect("the review carried over");
    assert_eq!(carried.reviewed, reviewed);
    assert!(carried.why.contains("with no conflict"), "{carried:?}");
    let calls = merge_calls(&gh);
    assert!(
        matches!(calls.as_slice(), [call] if call.contains(&head)),
        "{calls:?}"
    );
    let [stored] = merges(&h, &item).await.try_into().expect("one request");
    assert_eq!(stored.carried, merge.carried, "recorded");
    assert!(stored.line().contains("carried over"), "{}", stored.line());
}

/// Case 1, a parent squash-merged onto an unmoved base: archond merges the
/// squash keeping the branch's tree, so no file changes.
#[tokio::test]
async fn a_squash_landing_merged_over_asks_and_merges_at_the_new_head() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, reviewed) = shipped(&h).await;
    let main = git(&h.repo, &["rev-parse", "main"]);
    let squash = commit_tree(&h, &tree(&h, &reviewed), &[&main]);
    git(
        &h.repo,
        &["push", "--quiet", "origin", &format!("{squash}:main")],
    );
    let head = commit_tree(&h, &tree(&h, &reviewed), &[&reviewed, &squash]);
    github_shows(&gh, &head, &squash);

    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(asked.state, MergeState::AwaitingOperator, "{asked:?}");
    assert_eq!(asked.head, head, "the question names the new head");
    let MergeAuthority::Confirmed { question, .. } = asked.authority.clone() else {
        panic!("asked: {asked:?}")
    };
    let text = h.ctx.store.lock().await.question(&question).unwrap().text;
    assert!(
        text.contains("carries over") && text.contains("already held"),
        "{text}"
    );
    let yes = Request::Answer {
        chat: false,
        question,
        text: "yes".into(),
    };
    h.cli(yes).await.expect("the answer is recorded");

    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    assert!(
        matches!(merge.state, MergeState::Merged { .. }),
        "{merge:?}"
    );
    assert_eq!(merge.head, head);
    assert!(gh.merged());
}

#[tokio::test]
async fn a_plain_commit_after_the_review_is_still_refused() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, reviewed) = shipped(&h).await;
    let base = git(&h.repo, &["rev-parse", "main"]);
    let head = commit_tree(&h, &tree(&h, &base), &[&reviewed]);
    github_shows(&gh, &head, &base);

    let refused = h.orchestrator(request_merge(&item)).await.unwrap_err();
    let refused = refused.to_string();
    assert!(
        refused.contains("does not carry over") && refused.contains("not a merge"),
        "{refused}"
    );
    let merge = merge_of(h.cli(merge_item(&item, false)).await);
    assert!(refusal(&merge).contains("not a merge"), "{merge:?}");
    assert_eq!(merge.head, reviewed, "nothing carried over");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

#[tokio::test]
async fn a_base_merge_that_adds_a_change_is_still_refused() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, reviewed) = shipped(&h).await;
    let base = base_moves(&h, "upstream.txt");
    // The base merged in, with a change of its own no reviewer read.
    let added = base_moves(&h, "unreviewed.txt");
    git(&h.repo, &["reset", "--quiet", "--hard", &base]);
    git(&h.repo, &["push", "--quiet", "--force", "origin", "main"]);
    let merged = git(&h.repo, &["merge-tree", "--write-tree", &reviewed, &added]);
    let head = commit_tree(&h, &merged, &[&reviewed, &base]);
    github_shows(&gh, &head, &base);

    let merge = merge_of(h.cli(merge_item(&item, false)).await);
    let why = refusal(&merge);
    assert!(why.contains("adds a change"), "{why}");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
}

/// The operator confirmed the merge at the reviewed head; a later head the
/// review covers still needs a question of its own.
#[tokio::test]
async fn a_confirmed_merge_does_not_follow_a_later_head() {
    let (h, gh) = delivering(Config::default()).await;
    let (item, reviewed) = shipped(&h).await;
    github_shows(&gh, &reviewed, &git(&h.repo, &["rev-parse", "main"]));
    let asked = merge_of(h.orchestrator(request_merge(&item)).await);
    let MergeAuthority::Confirmed { question, .. } = asked.authority.clone() else {
        panic!("asked: {asked:?}")
    };
    let base = base_moves(&h, "upstream.txt");
    let merged = git(&h.repo, &["merge-tree", "--write-tree", &reviewed, &base]);
    let head = commit_tree(&h, &merged, &[&reviewed, &base]);
    github_shows(&gh, &head, &base);
    let yes = Request::Answer {
        chat: false,
        question,
        text: "yes".into(),
    };
    h.cli(yes).await.expect("the answer is recorded");

    let [merge] = merges(&h, &item).await.try_into().expect("one request");
    assert!(refusal(&merge).contains("ask again"), "{merge:?}");
    assert!(merge_calls(&gh).is_empty() && !gh.merged());
    let again = merge_of(h.orchestrator(request_merge(&item)).await);
    assert_eq!(again.head, head, "a new request carries the review over");
}
