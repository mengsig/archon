//! A protocol-24 client of release 1.6.1 decodes what archond sends it.

use serde::Deserialize;
use serde_json::{Value, json};

use super::*;
use crate::id::QueueItemId;
use crate::protocol::{Change, ChangeSubject, TaskEdge, TaskGraph};

/// `TaskEdgeKind` as 1.6.1 shipped it: closed, with no `onto`.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OldEdgeKind {
    DependsOn,
    InitiativeAfter,
    StackedOn,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code, reason = "decoded only to prove it decodes")]
struct OldEdge {
    from: String,
    to: String,
    kind: OldEdgeKind,
    satisfied: bool,
}

fn at() -> crate::Timestamp {
    serde_json::from_value(json!("2026-11-17T12:00:00Z")).unwrap()
}

fn edge(kind: TaskEdgeKind) -> TaskEdge {
    TaskEdge {
        from: QueueItemId::new(),
        to: QueueItemId::new(),
        kind,
        satisfied: true,
    }
}

fn graph() -> Reply {
    Reply::TaskGraph(Box::new(TaskGraph {
        nodes: Vec::new(),
        edges: vec![edge(TaskEdgeKind::DependsOn), edge(TaskEdgeKind::Onto)],
        initiatives: Vec::new(),
        as_of: 7,
        truncated: false,
    }))
}

fn data(reply: &Reply) -> Value {
    let wire = serde_json::to_value(reply).unwrap();
    wire["data"].clone()
}

/// Without this gate the old types fail on the new variants at all.
#[test]
fn the_old_types_reject_the_new_variants() {
    let onto = serde_json::to_value(edge(TaskEdgeKind::Onto)).unwrap();
    assert!(serde_json::from_value::<OldEdge>(onto).is_err());
}

#[test]
fn an_undeclared_client_decodes_the_graph_without_onto_edges() {
    let old = ClientFeatures::declared(&["changes".to_owned()]);
    let edges = data(&old.adapt_reply(graph()))["edges"].clone();
    let edges: Vec<OldEdge> = serde_json::from_value(edges).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].kind, OldEdgeKind::DependsOn);
    let Reply::TaskGraph(kept) = ClientFeatures::all().adapt_reply(graph()) else {
        panic!("a graph stays a graph");
    };
    assert_eq!(kept.edges.len(), 2, "a declaring client keeps onto");
}

/// Change frames and pages pass as archond wrote them, to every client.
#[test]
fn changes_pass_unchanged_to_every_client() {
    let change = Change {
        seq: 6,
        at: at(),
        subject: ChangeSubject::Item {
            item: QueueItemId::new(),
            state: crate::protocol::QueueStateKind::Queued,
            stage: None,
            waiting_reason: None,
        },
    };
    let live = ServerFrame::Change {
        sub: 1,
        change: change.clone(),
    };
    let page = ServerFrame::Reply {
        id: 9,
        result: Ok(Reply::Changes {
            changes: vec![change],
            last_seq: 7,
        }),
    };
    for client in [ClientFeatures::default(), ClientFeatures::all()] {
        assert_eq!(client.adapt(live.clone()), live);
        assert_eq!(client.adapt(page.clone()), page);
    }
}

/// A client of this release reads a variant a newer archond adds.
#[test]
fn this_release_reads_variants_it_does_not_know() {
    let edge: TaskEdge = serde_json::from_value(json!({
        "from": QueueItemId::new(), "to": QueueItemId::new(),
        "kind": "some_later_kind", "satisfied": false
    }))
    .unwrap();
    assert_eq!(edge.kind, TaskEdgeKind::Other);
    let named: TaskEdgeKind = serde_json::from_value(json!("other")).unwrap();
    assert_eq!(named, TaskEdgeKind::Other);
}

#[test]
fn a_hello_declares_each_feature_by_name_and_unknown_names_are_ignored() {
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        ClientFeatures::declared(&names(&client_feature::ALL)),
        ClientFeatures::all()
    );
    let onto = ClientFeatures::declared(&names(&["onto_edges", "later_feature"]));
    assert_eq!(onto, ClientFeatures::all());
    let Reply::TaskGraph(kept) = onto.adapt_reply(graph()) else {
        panic!("a graph stays a graph");
    };
    assert_eq!(kept.edges.len(), 2);
    assert_eq!(ClientFeatures::declared(&[]), ClientFeatures::default());
}
