mod common;
use std::num::NonZeroU64;

use archon_core::error::ArchonError;
use archon_core::github::{GithubRepo, GithubThread};
use archon_core::id::QueueItemId;
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::{ChangeSubject, GithubKind, GithubLink, GithubRef, LinkRelation};
use archon_core::pull_request::PullRequestRef;
use archon_core::queue::QueueState;
use archon_queue::Store;
use common::*;

fn pr(number: u64) -> PullRequestRef {
    PullRequestRef::from_url(&format!("https://github.com/o/r/pull/{number}")).unwrap()
}

fn issue(number: u64) -> GithubRef {
    GithubRef {
        host: "github.com".into(),
        repo: "o/r".into(),
        number,
        kind: GithubKind::Issue,
    }
}

fn operator_link(item: &QueueItemId, reference: GithubRef, relation: LinkRelation) -> GithubLink {
    GithubLink {
        item: item.clone(),
        reference,
        relation,
        source: LinkSource::Operator,
        created_at: t(9),
    }
}

/// The `link` changes after `after`, as (item, number, relation).
fn link_changes(store: &Store, after: u64) -> Vec<(QueueItemId, u64, LinkRelation)> {
    let changes = store.changes_after(after, 1000).unwrap();
    changes
        .into_iter()
        .filter_map(|change| match change.subject {
            ChangeSubject::Link {
                item,
                number,
                relation,
                ..
            } => Some((item, number, relation)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_watcher_item_links_its_origin_with_one_change() {
    let mut store = store();
    let mut asked = item(None, "from github");
    asked.origin = Some(GithubThread {
        repository: GithubRepo::parse("O/R").unwrap(),
        number: NonZeroU64::new(12).unwrap(),
    });
    let stored = store.insert_item(&asked, None).unwrap();
    let links = store.item_links(&stored.id).unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0].reference, issue(12));
    assert_eq!(
        (links[0].relation, links[0].source),
        (LinkRelation::Origin, LinkSource::Watcher)
    );
    assert_eq!(links[0].created_at, stored.created_at);
    assert_eq!(
        link_changes(&store, 0),
        vec![(stored.id.clone(), 12, LinkRelation::Origin)]
    );
    let (summary_issue, summary_pr) = store.item_github_refs(&stored.id).unwrap();
    assert_eq!((summary_issue, summary_pr), (Some(issue(12)), None));

    let plain = store.insert_item(&item(None, "local"), None).unwrap();
    assert!(
        store.item_links(&plain.id).unwrap().is_empty(),
        "no origin, no link"
    );
}

#[test]
fn an_operator_link_is_idempotent_and_each_change_is_streamed_once() {
    let mut store = store();
    let target = store.insert_item(&item(None, "t"), None).unwrap();
    let start = store.last_change_seq().unwrap();
    let closes = operator_link(&target.id, issue(5), LinkRelation::Closes);
    assert!(store.link_github(&closes).unwrap(), "a new link");
    assert!(
        !store.link_github(&closes).unwrap(),
        "the same link twice is one"
    );
    let shouted = GithubRef {
        host: "GitHub.com".into(),
        repo: "O/R".into(),
        ..issue(5)
    };
    let shouted = operator_link(&target.id, shouted, LinkRelation::Closes);
    assert!(
        !store.link_github(&shouted).unwrap(),
        "case does not make another link"
    );
    let mentioned = operator_link(&target.id, issue(5), LinkRelation::Mentioned);
    assert!(
        store.link_github(&mentioned).unwrap(),
        "another relation is another link"
    );
    assert_eq!(store.item_links(&target.id).unwrap().len(), 2);

    assert!(
        store
            .unlink_github(&target.id, &issue(5), LinkRelation::Closes)
            .unwrap()
    );
    assert!(
        !store
            .unlink_github(&target.id, &issue(5), LinkRelation::Closes)
            .unwrap()
    );
    let changes = link_changes(&store, start);
    let id = target.id.clone();
    assert_eq!(
        changes,
        vec![
            (id.clone(), 5, LinkRelation::Closes),
            (id.clone(), 5, LinkRelation::Mentioned),
            (id, 5, LinkRelation::Closes),
        ],
        "a change per link added or removed, none for a repeat"
    );
}

#[test]
fn a_link_names_a_known_item_and_a_well_formed_reference() {
    let mut store = store();
    let stranger = QueueItemId::new();
    let refused = store.link_github(&operator_link(&stranger, issue(1), LinkRelation::Closes));
    assert!(
        matches!(refused, Err(ArchonError::NotFound(_))),
        "{refused:?}"
    );
    let refused = store.unlink_github(&stranger, &issue(1), LinkRelation::Closes);
    assert!(
        matches!(refused, Err(ArchonError::NotFound(_))),
        "{refused:?}"
    );
    assert!(matches!(
        store.item_links(&stranger),
        Err(ArchonError::NotFound(_))
    ));

    let target = store.insert_item(&item(None, "t"), None).unwrap();
    let before = store.last_change_seq().unwrap();
    let bad = GithubRef {
        repo: "not-a-repo".into(),
        ..issue(1)
    };
    let refused = store.link_github(&operator_link(
        &target.id,
        bad.clone(),
        LinkRelation::Closes,
    ));
    assert!(
        matches!(refused, Err(ArchonError::Invalid(_))),
        "{refused:?}"
    );
    assert!(matches!(
        store.issue_items(&bad),
        Err(ArchonError::Invalid(_))
    ));
    assert_eq!(
        store.last_change_seq().unwrap(),
        before,
        "a refused link writes nothing"
    );
}

#[test]
fn a_delivered_link_follows_the_runs_pull_requests() {
    let mut store = store();
    let target = store.insert_item(&item(None, "t"), None).unwrap();
    let run = start(&mut store, &target.id);
    let start_seq = store.last_change_seq().unwrap();
    store
        .record_run_pull_request(&run.id, &pr(7), None)
        .unwrap();
    store
        .record_run_pull_request(&run.id, &pr(7), Some("PR_kw7"))
        .unwrap();
    let delivered = |store: &Store| -> Vec<u64> {
        let links = store.item_links(&target.id).unwrap();
        assert!(
            links.iter().all(|l| l.source == LinkSource::Ship),
            "{links:?}"
        );
        links.iter().map(|l| l.reference.number).collect()
    };
    assert_eq!(delivered(&store), vec![7]);
    let id = target.id.clone();
    assert_eq!(
        link_changes(&store, start_seq),
        vec![(id.clone(), 7, LinkRelation::Delivered)]
    );
    let (_, summary_pr) = store.item_github_refs(&target.id).unwrap();
    assert_eq!(summary_pr.unwrap().reference, GithubRef::from(&pr(7)));

    store
        .record_run_pull_request(&run.id, &pr(8), None)
        .unwrap();
    assert_eq!(
        delivered(&store),
        vec![8],
        "the run now delivers to another one"
    );
    let moved = link_changes(&store, start_seq);
    assert_eq!(
        moved[1..],
        [
            (id.clone(), 8, LinkRelation::Delivered),
            (id, 7, LinkRelation::Delivered)
        ]
    );
}

#[test]
fn a_pull_request_another_run_still_delivers_to_stays_linked() {
    let mut store = store();
    let target = store.insert_item(&item(None, "t"), None).unwrap();
    let first = start(&mut store, &target.id);
    store
        .record_run_pull_request(&first.id, &pr(7), None)
        .unwrap();
    let failed = QueueState::Failed {
        run: Some(first.id.clone()),
        error: "retry".into(),
    };
    settle_run(&mut store, &first, failed, t(20));
    store
        .transition_item(&target.id, QueueState::Queued, t(21))
        .unwrap();
    let second = start(&mut store, &target.id);
    store
        .record_run_pull_request(&second.id, &pr(7), None)
        .unwrap();
    store
        .record_run_pull_request(&second.id, &pr(9), None)
        .unwrap();
    let mut numbers: Vec<u64> = store
        .item_links(&target.id)
        .unwrap()
        .iter()
        .map(|l| l.reference.number)
        .collect();
    numbers.sort();
    assert_eq!(numbers, vec![7, 9], "the first run still delivers to 7");
}

#[test]
fn an_issue_lists_every_item_linked_to_its_number() {
    let mut store = store();
    let a = store.insert_item(&item(None, "a"), None).unwrap();
    let b = store.insert_item(&item(None, "b"), None).unwrap();
    store
        .link_github(&operator_link(&a.id, issue(3), LinkRelation::Closes))
        .unwrap();
    store
        .link_github(&operator_link(&b.id, issue(3), LinkRelation::Mentioned))
        .unwrap();
    store
        .link_github(&operator_link(&b.id, issue(4), LinkRelation::Mentioned))
        .unwrap();
    let as_pr = GithubRef {
        kind: GithubKind::PullRequest,
        ..issue(3)
    };
    for reference in [issue(3), as_pr] {
        let links = store.issue_items(&reference).unwrap();
        let items: Vec<_> = links.iter().map(|l| l.item.clone()).collect();
        assert_eq!(items.len(), 2, "{links:?}");
        assert!(items.contains(&a.id) && items.contains(&b.id), "{items:?}");
    }
    assert_eq!(
        store.item_github_refs(&a.id).unwrap().0,
        Some(issue(3)),
        "the issue it closes"
    );
    assert_eq!(
        store.item_github_refs(&b.id).unwrap().0,
        None,
        "a mention is no summary issue"
    );
}
