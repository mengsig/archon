//! A protocol-24 client of release 1.6.1 decodes what archond sends it.

use serde::Deserialize;
use serde_json::{Value, json};

use super::*;
use crate::id::QueueItemId;
use crate::protocol::{Change, TaskEdge, TaskGraph};

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

/// `ChangeSubject` as 1.6.1 shipped it: closed, with no `pause`. Its
/// variants' fields are left out; serde ignores them.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "subject", rename_all = "snake_case")]
enum OldSubject {
    Item {},
    Dependency {},
    Run {},
    Stage {},
    Gate {},
    Question {},
    Initiative {},
    Link {},
}

#[derive(Debug, Deserialize)]
#[allow(dead_code, reason = "decoded only to prove it decodes")]
struct OldChange {
    seq: u64,
    #[serde(flatten)]
    subject: OldSubject,
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

fn pause(seq: u64) -> Change {
    Change {
        seq,
        at: at(),
        subject: ChangeSubject::Pause {
            global: true,
            repos: Vec::new(),
            pipelines: Vec::new(),
        },
    }
}

fn item(seq: u64) -> Change {
    Change {
        seq,
        at: at(),
        subject: ChangeSubject::Item {
            item: QueueItemId::new(),
            state: crate::protocol::QueueStateKind::Queued,
            stage: None,
            waiting_reason: None,
        },
    }
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
    let paused = serde_json::to_value(pause(3)).unwrap();
    assert!(serde_json::from_value::<OldChange>(paused).is_err());
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

/// A replayed page keeps `last_seq`, so the old cursor still moves past it.
#[test]
fn an_undeclared_client_decodes_a_replayed_page_without_pauses() {
    let page = Reply::Changes {
        changes: vec![item(1), pause(2), item(3), pause(4)],
        last_seq: 4,
    };
    let adapted = data(&ClientFeatures::default().adapt_reply(page.clone()));
    assert_eq!(adapted["last_seq"], 4);
    let changes: Vec<OldChange> = serde_json::from_value(adapted["changes"].clone()).unwrap();
    assert_eq!(changes.iter().map(|c| c.seq).collect::<Vec<_>>(), [1, 3]);
    let frame = ServerFrame::Reply {
        id: 9,
        result: Ok(page.clone()),
    };
    let Some(ServerFrame::Reply { result: Ok(r), .. }) = ClientFeatures::default().adapt(frame)
    else {
        panic!("a reply frame is never dropped");
    };
    assert!(matches!(r, Reply::Changes { changes, .. } if changes.len() == 2));
    let all = ClientFeatures::declared(&client_feature::ALL.map(str::to_owned));
    assert_eq!(all.adapt_reply(page.clone()), page);
}

#[test]
fn an_undeclared_client_is_sent_no_live_pause_change() {
    let old = ClientFeatures::default();
    let live = ServerFrame::Change {
        sub: 1,
        change: pause(5),
    };
    assert_eq!(old.adapt(live.clone()), None);
    let other = ServerFrame::Change {
        sub: 1,
        change: item(6),
    };
    let Some(ServerFrame::Change { change, .. }) = old.adapt(other) else {
        panic!("other changes pass");
    };
    let decoded: OldChange = serde_json::from_value(serde_json::to_value(change).unwrap()).unwrap();
    assert_eq!(decoded.seq, 6);
    assert_eq!(ClientFeatures::all().adapt(live.clone()), Some(live));
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
    let change: Change = serde_json::from_value(json!({
        "seq": 8, "at": "2026-11-17T12:00:00Z", "subject": "some_later_subject", "x": [1]
    }))
    .unwrap();
    assert_eq!((change.seq, change.subject), (8, ChangeSubject::Other));
    let page: Reply = serde_json::from_value(json!({"reply": "changes", "data": {
        "changes": [{"seq": 9, "at": "2026-11-17T12:00:00Z", "subject": "later"}],
        "last_seq": 9
    }}))
    .unwrap();
    assert!(matches!(page, Reply::Changes { last_seq: 9, .. }));
}

#[test]
fn a_hello_declares_each_feature_by_name_and_unknown_names_are_ignored() {
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        ClientFeatures::declared(&names(&client_feature::ALL)),
        ClientFeatures::all()
    );
    let onto = ClientFeatures::declared(&names(&["onto_edges", "later_feature"]));
    assert_ne!(onto, ClientFeatures::all());
    assert_eq!(
        onto.adapt(ServerFrame::Change {
            sub: 1,
            change: pause(1)
        }),
        None
    );
    let Reply::TaskGraph(kept) = onto.adapt_reply(graph()) else {
        panic!("a graph stays a graph");
    };
    assert_eq!(kept.edges.len(), 2);
    assert_eq!(ClientFeatures::declared(&[]), ClientFeatures::default());
}
