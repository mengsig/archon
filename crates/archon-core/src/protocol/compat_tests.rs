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
    assert_ne!(onto, ClientFeatures::all());
    let Reply::TaskGraph(kept) = onto.adapt_reply(graph()) else {
        panic!("a graph stays a graph");
    };
    assert_eq!(kept.edges.len(), 2);
    assert_eq!(ClientFeatures::declared(&[]), ClientFeatures::default());
}

/// An undeclared client's component is joined without the onto links it
/// never sees; other requests and declared clients pass unchanged.
#[test]
fn an_undeclared_client_asks_for_a_component_without_onto_links() {
    let component = Request::TaskGraph {
        filter: crate::protocol::TaskFilter {
            component_of: Some(QueueItemId::new()),
            ..Default::default()
        },
        limit: None,
    };
    let Request::TaskGraph { filter, .. } =
        ClientFeatures::default().adapt_request(component.clone())
    else {
        panic!("a graph request stays a graph request");
    };
    assert!(filter.without_onto, "the old client's component skips onto");
    assert_eq!(
        ClientFeatures::all().adapt_request(component.clone()),
        component
    );
    assert_eq!(
        ClientFeatures::default().adapt_request(Request::Shutdown),
        Request::Shutdown
    );
    let wire = serde_json::to_value(&filter).unwrap();
    assert!(
        wire.get("without_onto").is_none(),
        "the flag never reaches the wire"
    );
}

#[test]
fn catalog_outages_reach_older_clients_with_a_known_reason_code() {
    let change = Change {
        seq: 7,
        at: at(),
        subject: ChangeSubject::Item {
            item: QueueItemId::new(),
            state: crate::protocol::QueueStateKind::Queued,
            stage: None,
            waiting_reason: Some(WaitingReasonCode::ModelCatalogUnavailable),
        },
    };
    let frame = ServerFrame::Change { sub: 1, change };
    let old = serde_json::to_value(ClientFeatures::default().adapt(frame.clone())).unwrap();
    assert!(old.to_string().contains("aether_disconnected"));
    assert_eq!(ClientFeatures::all().adapt(frame.clone()), frame);
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum OldProposalMode {
    Lavish,
    Off,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OldStageKind {
    Propose,
    Work,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OldRepoTiers {
    integration_branch: String,
}

#[test]
fn old_web_releases_decode_live_proposal_settings_and_repo_tiers() {
    let repo: Repo = serde_json::from_value(json!({
        "id": crate::id::RepoId::new(), "name": "example", "path": "/example",
        "ship": "local", "added_at": at(), "autonomy_max": {"proposal": "live"},
        "tiers": {"integration_branch": "next", "design": {"allow_hosts": ["static.example.com"]}}
    }))
    .unwrap();
    let reply = Reply::Repo(repo);
    for version in [23, 24] {
        let old = data(&ClientFeatures::for_protocol(version, &[]).adapt_reply(reply.clone()));
        let proposal =
            serde_json::from_value::<OldProposalMode>(old["autonomy_max"]["proposal"].clone())
                .unwrap();
        let tiers = serde_json::from_value::<OldRepoTiers>(old["tiers"].clone()).unwrap();
        assert_eq!(proposal, OldProposalMode::Lavish);
        assert_eq!(tiers.integration_branch, "next");
    }
    for version in [25, 26] {
        let current = ClientFeatures::for_protocol(version, &[]).adapt_reply(reply.clone());
        assert_eq!(current, reply);
        assert_eq!(data(&current)["autonomy_max"]["proposal"], "live");
    }
}

#[test]
fn an_old_run_plan_decodes_the_design_stage_as_a_proposal() {
    let plan = serde_json::from_value(json!({
        "run": crate::id::PipelineRunId::new(), "pipeline": "adaptive", "revision": "r1",
        "edited": false, "stages": [
            {"id": "design", "kind": "design", "fan_out": 1, "gate": "none", "timeout_minutes": 60, "max_loops": 0},
            {"id": "work", "kind": "work", "fan_out": 1, "gate": "none", "timeout_minutes": 60, "max_loops": 0}
        ]
    })).unwrap();
    let reply = Reply::RunPlan(plan);
    let old = data(&ClientFeatures::for_protocol(24, &[]).adapt_reply(reply.clone()));
    let kinds: Vec<OldStageKind> = old["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| serde_json::from_value(stage["kind"].clone()).unwrap())
        .collect();
    assert_eq!(kinds, [OldStageKind::Propose, OldStageKind::Work]);
    assert_eq!(old["stages"][0]["id"], "design");
    let current = ClientFeatures::for_protocol(25, &[]).adapt_reply(reply.clone());
    assert_eq!(current, reply);
}

#[test]
fn protocol_23_does_not_receive_reply_variants_from_the_feature_handshake() {
    let declared = [client_feature::ENQUEUE_ADVICE.to_owned()];
    assert!(!ClientFeatures::for_protocol(23, &declared).enqueue_advice);
    for version in 24..=26 {
        assert!(ClientFeatures::for_protocol(version, &declared).enqueue_advice);
        assert!(!ClientFeatures::for_protocol(version, &[]).enqueue_advice);
    }
}
