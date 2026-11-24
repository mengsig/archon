//! Protocol 26: design iterations on the wire, defaulted for older senders.

use serde_json::json;

use super::*;

const ITEM: &str = "qi_01M621D9ZBJPTMDTV26KKQNBE1";
const SESSION: &str = "ds_01M621D9ZBJPTMDTV26KKQNBE1";

#[test]
fn the_version_and_feature_are_26() {
    assert_eq!(PROTOCOL_VERSION, 26);
    assert_eq!(MIN_WEB_PROTOCOL_VERSION, 23);
    assert!(feature::ALL.contains(&feature::DESIGN_ITERATIONS));
    let both = [feature::DESIGN_SESSIONS, feature::DESIGN_ITERATIONS];
    let at = |version| {
        negotiate(ClientKind::ArchonWeb, version, "web", &both)
            .unwrap()
            .features
    };
    assert_eq!(at(26), ["design_sessions", "design_iterations"]);
    assert_eq!(at(25), ["design_sessions"], "25 knows no iterations");
    assert!(at(24).is_empty());
    assert!(at(23).is_empty());
}

#[test]
fn a_note_add_without_iterations_reads_as_before() {
    let old = json!({"cmd": "design_note_add", "session": SESSION, "client_key": "k",
        "text": "t", "asset": null, "send_picture": false, "details_text": null});
    let request: Request = serde_json::from_value(old.clone()).unwrap();
    assert!(matches!(
        &request,
        Request::DesignNoteAdd { follows: None, refs, .. } if refs.is_empty()
    ));
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        old,
        "nothing new is sent"
    );
    let new = json!({"cmd": "design_note_add", "session": SESSION, "client_key": "k",
        "text": "t", "asset": null, "send_picture": false, "details_text": null,
        "follows": "D3", "refs": ["D5"]});
    let request: Request = serde_json::from_value(new.clone()).unwrap();
    assert_eq!(serde_json::to_value(&request).unwrap(), new);
    assert_eq!(request.class(), RequestClass::OperatorChange);
}

#[test]
fn continue_from_and_history_have_their_classes() {
    let wire = json!({"cmd": "design_note_continue_from", "item": ITEM, "id": "D3",
        "text": "bigger", "client_key": "c1", "refs": []});
    let request: Request = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(request.class(), RequestClass::OperatorChange);
    assert_eq!(serde_json::to_value(&request).unwrap(), wire);
    let short: Request = serde_json::from_value(
        json!({"cmd": "design_note_continue_from", "item": ITEM, "id": "D3"}),
    )
    .unwrap();
    assert!(matches!(
        short,
        Request::DesignNoteContinueFrom {
            text: None,
            client_key: None,
            ..
        }
    ));
    let history: Request =
        serde_json::from_value(json!({"cmd": "design_history", "caller": null})).unwrap();
    assert!(matches!(history, Request::DesignHistory { id: None, .. }));
    assert_eq!(history.class(), RequestClass::Change);
}

#[test]
fn a_finish_card_and_note_view_of_an_older_daemon_still_read() {
    let card: FinishCard = serde_json::from_value(json!({"note": "D1", "text": "t",
        "asset": null, "data_url": null, "caption": null, "include": true,
        "superseded_by": null, "not_found": false, "left_history": false}))
    .unwrap();
    assert!(card.history.is_empty());
    let entry = FinishHistoryEntry {
        note: "D1".into(),
        text: "first".into(),
        set_aside: false,
    };
    let with = FinishCard {
        history: vec![entry],
        ..card
    };
    let value = serde_json::to_value(&with).unwrap();
    assert_eq!(value["history"][0]["note"], "D1");
}
