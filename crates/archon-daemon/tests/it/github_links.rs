//! GitHub links over the handlers (CONTRACTS.md §1.3, D1): `link_issue` and
//! `unlink_issue` are idempotent changes, read back by `item_links`,
//! `issue_items` and the item's summary.

use crate::common;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::{
    GithubKind, GithubRef, ItemQuery, LinkRelation, ManualLinkRelation, Reply, Request,
    RequestClass,
};
use common::Harness;

fn issue() -> GithubRef {
    GithubRef {
        host: "github.com".into(),
        repo: "mengsig/archon".into(),
        number: 384,
        kind: GithubKind::Issue,
    }
}

fn link(item: &QueueItemId, reference: GithubRef, linked: bool) -> Request {
    let item = item.clone();
    let relation = ManualLinkRelation::Closes;
    match linked {
        true => Request::LinkIssue {
            item,
            reference,
            relation,
        },
        false => Request::UnlinkIssue {
            item,
            reference,
            relation,
        },
    }
}

async fn item_links(h: &Harness, item: &QueueItemId) -> Vec<archon_core::protocol::GithubLink> {
    match h.cli(Request::ItemLinks { item: item.clone() }).await {
        Ok(Reply::ItemLinks { links, objects }) => {
            assert!(objects.is_empty(), "no cache before github_import");
            links
        }
        other => panic!("item_links: {other:?}"),
    }
}

#[test]
fn linking_is_a_change_an_observer_may_not_make() {
    let item = QueueItemId::new();
    for linked in [true, false] {
        let request = link(&item, issue(), linked);
        assert_eq!(request.class(), RequestClass::Change, "{request:?}");
        assert!(!request.is_read_only(), "{request:?}");
    }
    let reads = [
        Request::ItemLinks { item },
        Request::IssueItems { reference: issue() },
    ];
    for read in reads {
        assert_eq!(read.class(), RequestClass::Read, "{read:?}");
    }
}

#[tokio::test]
async fn an_operator_link_is_ok_twice_and_read_back_everywhere() {
    let h = Harness::new().await;
    let item = h.enqueue("closes an issue").await;
    let shouted = GithubRef {
        repo: "Mengsig/Archon".into(),
        ..issue()
    };
    for reference in [issue(), issue(), shouted] {
        let reply = h.cli(link(&item.id, reference, true)).await;
        assert!(matches!(reply, Ok(Reply::Ok)), "{reply:?}");
    }
    let links = item_links(&h, &item.id).await;
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(
        (links[0].relation, links[0].source),
        (LinkRelation::Closes, LinkSource::Operator)
    );
    assert_eq!(links[0].reference, issue());

    let as_pr = GithubRef {
        kind: GithubKind::PullRequest,
        ..issue()
    };
    match h.cli(Request::IssueItems { reference: as_pr }).await {
        Ok(Reply::IssueItems { links }) => assert_eq!(links.len(), 1, "{links:?}"),
        other => panic!("issue_items: {other:?}"),
    }
    match h
        .cli(Request::ListItemSummaries(ItemQuery::default()))
        .await
    {
        Ok(Reply::ItemSummaries(summaries)) => {
            let summary = summaries.iter().find(|s| s.item.id == item.id).unwrap();
            assert_eq!(
                (summary.issue.as_ref(), summary.pr.as_ref()),
                (Some(&issue()), None)
            );
        }
        other => panic!("summaries: {other:?}"),
    }

    for _ in 0..2 {
        let reply = h.cli(link(&item.id, issue(), false)).await;
        assert!(matches!(reply, Ok(Reply::Ok)), "{reply:?}");
    }
    assert!(item_links(&h, &item.id).await.is_empty());
}

#[tokio::test]
async fn a_link_to_an_unknown_item_or_a_malformed_reference_is_refused() {
    let h = Harness::new().await;
    let stranger = QueueItemId::new();
    let refused = h.cli(link(&stranger, issue(), true)).await;
    assert!(
        matches!(refused, Err(ArchonError::NotFound(_))),
        "{refused:?}"
    );
    let refused = h.cli(Request::ItemLinks { item: stranger }).await;
    assert!(
        matches!(refused, Err(ArchonError::NotFound(_))),
        "{refused:?}"
    );
    let item = h.enqueue("t").await;
    let bad = GithubRef {
        number: 0,
        ..issue()
    };
    let refused = h.cli(link(&item.id, bad, true)).await;
    assert!(
        matches!(refused, Err(ArchonError::Invalid(_))),
        "{refused:?}"
    );
}
