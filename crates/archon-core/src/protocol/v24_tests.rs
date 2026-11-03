//! Protocol 24's negotiation, classes and wire shapes.

use super::*;

fn request(json: serde_json::Value) -> Request {
    serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"))
}

/// The CLI, MCP and bench ship in archond's binary: one version only.
#[test]
fn every_client_but_the_web_app_needs_an_exact_match() {
    for client in [
        ClientKind::ArchonCli,
        ClientKind::ArchonMcp,
        ClientKind::ArchonBench,
    ] {
        let welcome = negotiate(client, PROTOCOL_VERSION, "1", &[]).unwrap();
        assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
        assert_eq!(welcome.min_protocol_version, PROTOCOL_VERSION);
        for other in [MIN_WEB_PROTOCOL_VERSION, PROTOCOL_VERSION + 1] {
            let refusal = negotiate(client, other, "1", &[]).unwrap_err();
            assert!(
                refusal.starts_with(&format!("archond speaks protocol {PROTOCOL_VERSION}")),
                "{refusal}"
            );
            assert!(!refusal.contains("accepts"), "{refusal}");
        }
    }
}

/// `archon-web` at 23 and 24 connects; 25 is refused with the range.
#[test]
fn the_web_app_may_speak_23_or_24() {
    let served = [feature::CHANGES];
    let web = ClientKind::ArchonWeb;
    let current = negotiate(web, PROTOCOL_VERSION, "web", &served).unwrap();
    assert_eq!(current.protocol_version, 24);
    assert_eq!(current.min_protocol_version, 23);
    assert_eq!(current.features, ["changes"]);
    let older = negotiate(web, MIN_WEB_PROTOCOL_VERSION, "web", &served).unwrap();
    assert_eq!(older.protocol_version, 23);
    assert!(older.features.is_empty(), "23 knows no feature");
    for refused in [22, 25] {
        let refusal = negotiate(web, refused, "web", &served).unwrap_err();
        assert!(
            refusal.contains(&format!("speaks {refused}")) && refusal.contains("accepts 23 to 24"),
            "{refusal}"
        );
    }
}

/// A 23 client reads a 24 welcome: the new fields are additions.
#[test]
fn a_welcome_keeps_the_fields_protocol_23_reads() {
    let welcome = negotiate(ClientKind::ArchonWeb, 23, "web", &[]).unwrap();
    let wire = serde_json::to_value(Reply::Welcome(welcome)).unwrap();
    assert_eq!(wire["reply"], "welcome");
    assert_eq!(wire["data"]["protocol_version"], 23);
    assert!(wire["data"]["daemon_version"].is_string());
    assert!(wire["data"].get("device").is_none(), "{wire}");
}

/// A hello without features is the frame protocol 23 sent.
#[test]
fn a_hello_may_name_features() {
    let bare = request(serde_json::json!({
        "cmd": "hello", "protocol_version": 23, "client": "archon-web", "client_version": "w"
    }));
    let Request::Hello { features, .. } = &bare else {
        panic!("{bare:?}");
    };
    assert!(features.is_empty());
    let named = request(serde_json::json!({
        "cmd": "hello", "protocol_version": 24, "client": "archon-web",
        "client_version": "w", "features": ["changes"]
    }));
    assert!(matches!(named, Request::Hello { features, .. } if features == ["changes"]));
}

fn github_ref() -> serde_json::Value {
    serde_json::json!({"host": "github.com", "repo": "o/r", "number": 1, "kind": "issue"})
}

/// Every protocol 24 request, as a client spells it, with its class.
fn new_requests() -> Vec<(Request, RequestClass)> {
    use RequestClass::{Change, Read};
    let item = QueueItemId::new();
    let run = PipelineRunId::new();
    let link = serde_json::json!({"item": item, "ref": github_ref(), "relation": "closes"});
    let mut link_cmd = link.clone();
    link_cmd["cmd"] = "link_issue".into();
    let mut unlink_cmd = link;
    unlink_cmd["cmd"] = "unlink_issue".into();
    [
        (serde_json::json!({"cmd": "subscribe_changes"}), Read),
        (serde_json::json!({"cmd": "changes_page", "after": 3}), Read),
        (
            serde_json::json!({"cmd": "item_history", "item": item}),
            Read,
        ),
        (serde_json::json!({"cmd": "task_graph"}), Read),
        (serde_json::json!({"cmd": "run_plan", "run": run}), Read),
        (serde_json::json!({"cmd": "item_links", "item": item}), Read),
        (
            serde_json::json!({"cmd": "issue_items", "ref": github_ref()}),
            Read,
        ),
        (
            serde_json::json!({"cmd": "list_github_objects", "repo": "o/r"}),
            Read,
        ),
        (link_cmd, Change),
        (unlink_cmd, Change),
        (
            serde_json::json!({"cmd": "import_issue", "ref": github_ref()}),
            Change,
        ),
    ]
    .into_iter()
    .map(|(json, class)| (request(json), class))
    .collect()
}

#[test]
fn every_new_request_has_its_class_and_round_trips() {
    let requests = new_requests();
    assert_eq!(requests.len(), 11, "CONTRACTS.md §1.3 lists eleven");
    for (request, class) in requests {
        assert_eq!(request.class(), class, "{request:?}");
        let frame = ClientFrame { id: 9, request };
        let wire = serde_json::to_value(&frame).unwrap();
        let back: ClientFrame = serde_json::from_value(wire).unwrap();
        assert_eq!(back, frame);
    }
}

/// `history_expired` carries the range still kept.
#[test]
fn history_expired_names_the_kept_range() {
    let code = ErrorCode::HistoryExpired(HistoryRange {
        first_seq: 10,
        last_seq: 99,
    });
    let wire = serde_json::to_value(ErrorReply {
        code,
        message: "gone".into(),
    })
    .unwrap();
    assert_eq!(
        wire["code"],
        serde_json::json!({"history_expired": {"first_seq": 10, "last_seq": 99}})
    );
    let back: ErrorReply = serde_json::from_value(wire).unwrap();
    assert_eq!(back.code, code);
    assert_eq!(
        serde_json::to_value(ErrorCode::NotFound).unwrap(),
        "not_found"
    );
}

/// The change frame is `{"frame":"change","sub":id,"change":Change}`.
#[test]
fn a_change_frame_names_its_subscription() {
    let frame = ServerFrame::Change {
        sub: 2,
        change: Change {
            seq: 5,
            at: crate::now(),
            subject: ChangeSubject::Question {
                question: crate::id::QuestionId::new(),
                item: None,
                open: true,
            },
        },
    };
    let wire = serde_json::to_value(&frame).unwrap();
    assert_eq!(
        (&wire["frame"], &wire["sub"]),
        (&"change".into(), &2.into())
    );
    assert_eq!(wire["change"]["subject"], "question");
    assert_eq!(serde_json::from_value::<ServerFrame>(wire).unwrap(), frame);
}

/// The 23 query shape is unchanged; the 24 filters are optional additions.
#[test]
fn item_queries_keep_their_23_shape() {
    let bare = serde_json::to_value(ItemQuery::default()).unwrap();
    assert!(bare.get("text").is_none() && bare.get("with_as_of").is_none());
    assert!(!ItemQuery::default().has_item_filters());
    let query: ItemQuery = serde_json::from_value(serde_json::json!({
        "repo": null, "pipeline": null, "tag": null, "limit": null, "text": "fix"
    }))
    .unwrap();
    assert!(query.has_item_filters());
}
