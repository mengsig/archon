mod common;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::pull_request::PullRequestRef;
use archon_queue::RunPullRequest;
use common::*;

fn pr(number: u64) -> PullRequestRef {
    PullRequestRef::from_url(&format!("https://github.com/o/r/pull/{number}")).unwrap()
}

#[test]
fn runs_record_the_pull_request_they_delivered_to_and_may_share_one() {
    let mut store = store();
    let first = store.insert_item(&item(None, "first"), None).unwrap();
    let second = store.insert_item(&item(None, "second"), None).unwrap();
    let (a, b) = (start(&mut store, &first.id), start(&mut store, &second.id));
    assert_eq!(store.run_pull_request(&a.id).unwrap(), None);

    store.record_run_pull_request(&a.id, &pr(7), None).unwrap();
    store
        .record_run_pull_request(&b.id, &pr(7), Some("PR_kw7"))
        .unwrap();
    store.record_run_pull_request(&b.id, &pr(7), None).unwrap();
    let known = |node: Option<&str>| RunPullRequest {
        pr: pr(7),
        node_id: node.map(str::to_owned),
    };
    assert_eq!(
        store.run_pull_request(&a.id).unwrap(),
        Some(known(Some("PR_kw7"))),
        "every run delivered to the pull request learns its node id"
    );
    assert_eq!(
        store.run_pull_request(&b.id).unwrap(),
        Some(known(Some("PR_kw7"))),
        "a known node id is kept"
    );

    let third = store.insert_item(&item(None, "third"), None).unwrap();
    let c = start(&mut store, &third.id);
    store.record_run_pull_request(&c.id, &pr(7), None).unwrap();
    assert_eq!(
        store.run_pull_request(&c.id).unwrap(),
        Some(known(Some("PR_kw7"))),
        "a run delivered later learns the known node id"
    );

    store.record_run_pull_request(&b.id, &pr(8), None).unwrap();
    let moved = store.run_pull_request(&b.id).unwrap().unwrap();
    assert_eq!(
        (moved.pr, moved.node_id),
        (pr(8), None),
        "another pull request"
    );
}

#[test]
fn a_pull_request_of_no_run_or_without_a_node_id_is_refused() {
    let mut store = store();
    let unknown = store.record_run_pull_request(&PipelineRunId::new(), &pr(1), None);
    assert!(
        matches!(unknown, Err(ArchonError::NotFound(_))),
        "{unknown:?}"
    );
    let queued = store.insert_item(&item(None, "x"), None).unwrap();
    let run = start(&mut store, &queued.id);
    let empty = store.record_run_pull_request(&run.id, &pr(1), Some(" "));
    assert!(matches!(empty, Err(ArchonError::Invalid(_))), "{empty:?}");
    assert_eq!(store.run_pull_request(&run.id).unwrap(), None);
}

#[test]
fn a_run_keeps_the_last_review_archond_verified_and_an_old_one_has_none() {
    let mut store = store();
    let first = store.insert_item(&item(None, "first"), None).unwrap();
    let run = start(&mut store, &first.id);
    assert_eq!(
        store.run_review(&run.id).unwrap(),
        None,
        "none verified yet"
    );
    let review = |head: &str| archon_queue::RunReview {
        head: head.repeat(40),
        tree: "b".repeat(40),
    };
    store.record_run_review(&run.id, &review("a")).unwrap();
    store.record_run_review(&run.id, &review("c")).unwrap();
    assert_eq!(store.run_review(&run.id).unwrap(), Some(review("c")));

    let short = archon_queue::RunReview {
        head: "abc".into(),
        tree: "b".repeat(40),
    };
    let refused = store.record_run_review(&run.id, &short).unwrap_err();
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    let nowhere = store.record_run_review(&PipelineRunId::new(), &review("a"));
    assert!(nowhere.is_err(), "a review of no run is refused");
    assert_eq!(store.run_review(&run.id).unwrap(), Some(review("c")));

    store
        .kv_set(&format!("run_review:{}", run.id), "{\"head\": 1}")
        .unwrap();
    let malformed = store.run_review(&run.id).unwrap_err();
    assert!(matches!(malformed, ArchonError::Store(_)), "{malformed:?}");
}
