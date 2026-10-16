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
    assert_eq!(store.run_pull_request(&a.id).unwrap(), Some(known(None)));
    assert_eq!(
        store.run_pull_request(&b.id).unwrap(),
        Some(known(Some("PR_kw7"))),
        "a known node id is kept"
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
