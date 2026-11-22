//! Protocol 25: live design sessions on the wire.

use serde_json::{Value, json};

use super::*;
use crate::design::{DesignNote, DesignSession};
use crate::error::DesignRefusal;

const ITEM: &str = "qi_01M621D9ZBJPTMDTV26KKQNBE1";
const SESSION: &str = "ds_01M621D9ZBJPTMDTV26KKQNBE1";
const ASSET: &str = "da_01M621D9ZBJPTMDTV26KKQNBE1";

fn rect() -> Value {
    json!({"x": 1.0, "y": 2.0, "w": 3.0, "h": 4.0})
}

/// One wire form of every new request and its class.
fn requests() -> Vec<(Value, RequestClass)> {
    use RequestClass::{Change, OperatorChange, OperatorRead, Read};
    let op = OperatorChange;
    vec![
        (json!({"cmd": "design_get", "item": ITEM}), Read),
        (
            json!({"cmd": "subscribe_design", "session": SESSION}),
            OperatorRead,
        ),
        (
            json!({"cmd": "design_asset", "item": ITEM, "id": ASSET}),
            OperatorRead,
        ),
        (json!({"cmd": "design_frame_ack", "sub": 1, "seq": 9}), op),
        (
            json!({"cmd": "design_input", "session": SESSION, "events": [
                {"type": "mouse", "action": "down", "x": 1.0, "y": 2.0, "button": "left", "click_count": 1, "modifiers": 0},
                {"type": "wheel", "x": 1.0, "y": 2.0, "dx": 0.0, "dy": 5.0, "modifiers": 2},
                {"type": "key", "action": "up", "key": "a", "code": "KeyA", "text": null, "modifiers": 0},
                {"type": "text", "text": "hi"},
                {"type": "touch", "action": "cancel", "points": [{"x": 1.0, "y": 2.0, "id": 3}]},
            ]}),
            op,
        ),
        (
            json!({"cmd": "design_resolve", "session": SESSION, "x": 1.0, "y": 2.0, "depth": -1, "select": true}),
            op,
        ),
        (
            json!({"cmd": "design_region", "session": SESSION, "box": rect()}),
            op,
        ),
        (
            json!({"cmd": "design_upload", "session": SESSION, "media_type": "image/png", "data": "AAAA", "name": "a.png", "sketch": null, "base": ASSET, "blank": true}),
            op,
        ),
        (
            json!({"cmd": "design_viewport", "session": SESSION, "preset": "tablet"}),
            op,
        ),
        (json!({"cmd": "design_reload", "session": SESSION}), op),
        (json!({"cmd": "design_tell_agent", "session": SESSION}), op),
        (
            json!({"cmd": "design_dialog_answer", "session": SESSION, "accept": true, "text": "x"}),
            op,
        ),
        (
            json!({"cmd": "design_note_add", "session": SESSION, "client_key": "k", "text": "t", "asset": ASSET, "send_picture": true, "details_text": "d"}),
            op,
        ),
        (
            json!({"cmd": "design_note_edit", "item": ITEM, "id": "D1", "text": "t"}),
            op,
        ),
        (
            json!({"cmd": "design_note_cancel", "item": ITEM, "id": "D1"}),
            op,
        ),
        (
            json!({"cmd": "design_note_reply", "item": ITEM, "id": "D1", "text": "t"}),
            op,
        ),
        (
            json!({"cmd": "design_note_drop", "item": ITEM, "id": "D1"}),
            op,
        ),
        (
            json!({"cmd": "design_note_undo", "item": ITEM, "id": "D1"}),
            op,
        ),
        (
            json!({"cmd": "design_note_resend", "item": ITEM, "id": "D1"}),
            op,
        ),
        (
            json!({"cmd": "design_finish_capture", "session": SESSION, "cancel_queued": true, "recapture": true}),
            op,
        ),
        (
            json!({"cmd": "design_final_capture", "session": SESSION, "capture": "c", "note": "D1"}),
            op,
        ),
        (
            json!({"cmd": "design_finish_confirm", "session": SESSION, "capture": "c", "include": {"D1": true}, "superseded": {"D1": "D2", "D3": null}, "confirm_no_change": true}),
            op,
        ),
        (
            json!({"cmd": "design_finish_cancel", "session": SESSION}),
            op,
        ),
        (
            json!({"cmd": "design_preview", "url": "http://127.0.0.1:5173/", "caller": "agt_01M621D9ZBJPTMDTV26KKQNBE1"}),
            Change,
        ),
        (
            json!({"cmd": "design_note_report", "id": "D1", "status": "needs_reply", "reply": "q", "commit": null, "no_change": true, "caller": null}),
            Change,
        ),
    ]
}

#[test]
fn every_design_request_round_trips_with_its_wire_name_and_class() {
    let all = requests();
    assert_eq!(all.len(), 25);
    for (wire, class) in all {
        let request: Request =
            serde_json::from_value(wire.clone()).unwrap_or_else(|e| panic!("{wire}: {e}"));
        assert_eq!(request.class(), class, "{wire}");
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        let framed = ClientFrame { id: 7, request };
        let text = serde_json::to_string(&framed).unwrap();
        assert_eq!(serde_json::from_str::<ClientFrame>(&text).unwrap(), framed);
    }
}

#[test]
fn optional_flags_default_when_absent() {
    let finish: Request =
        serde_json::from_value(json!({"cmd": "design_finish_capture", "session": SESSION}))
            .unwrap();
    assert!(matches!(
        finish,
        Request::DesignFinishCapture {
            cancel_queued: false,
            recapture: false,
            ..
        }
    ));
    let report: Request = serde_json::from_value(json!({
        "cmd": "design_note_report", "id": "D1", "status": "done", "reply": null,
        "commit": null, "caller": null,
    }))
    .unwrap();
    assert!(matches!(
        report,
        Request::DesignNoteReport {
            no_change: false,
            ..
        }
    ));
}

fn at() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn note_value() -> Value {
    json!({
        "item": ITEM, "id": "D1", "number": 1, "session": SESSION, "kind": "change",
        "text": "t", "details_text": null, "send_picture": false, "target": null,
        "viewport": {"w": 1440, "h": 900, "preset": "desktop"}, "status": "queued",
        "fail_reason": null, "base_commit": null, "commit": null, "no_change": false,
        "reverts": null, "resent_from": null, "superseded_by": null, "thread": [],
        "client_key": "k", "delivery": 0, "before_asset": null, "after_asset": null,
        "final_asset": null, "final_included": null, "captured_commit": null,
        "created_by_device": null, "created_at": at(), "updated_at": at(),
    })
}

fn note_view_value() -> Value {
    let mut view = note_value();
    let object = view.as_object_mut().unwrap();
    object.insert("undone_by".into(), Value::Null);
    object.insert("read_only".into(), json!(false));
    object.insert("active".into(), json!(true));
    object.insert("approved".into(), json!(false));
    view
}

fn target_value() -> Value {
    json!({
        "path": "/", "selector": "a", "role_name": null, "component": null, "box": rect(),
        "viewport": {"w": 390, "h": 844, "preset": "phone"}, "styles": {}, "region": false,
    })
}

fn session_value() -> Value {
    json!({
        "id": SESSION, "item": ITEM, "run": "prun_01M621D9ZBJPTMDTV26KKQNBE1",
        "stage_id": "design", "attempt": 1, "state": "live", "degraded": false,
        "note_version": 3, "finish_capture": null, "finish_head": null,
        "finish_choices": null, "preview_url": null, "browser_pid": null,
        "browser_started_at": null, "viewport_preset": "desktop", "sandbox_off": false,
        "agent": null, "created_at": at(), "ended_at": null, "end_reason": null,
    })
}

/// Each reply with its `{"reply","data"}` wire form.
#[test]
fn every_design_reply_round_trips() {
    let state = json!({
        "item": ITEM, "session": session_value(), "notes": [note_view_value()],
        "browser": {"status": "missing", "hint": "install chromium"},
        "finish_blocker": null, "images_used": 1, "images_max": 100, "stage_deadline": at(),
    });
    let card = json!({
        "note": "D1", "text": "t", "asset": ASSET, "data_url": "data:image/png;base64,AA",
        "caption": null, "include": true, "superseded_by": null, "not_found": false,
        "left_history": false, "earlier": false,
    });
    // A card from a daemon that predates `earlier` reads as this session's.
    let mut older = card.clone();
    older.as_object_mut().unwrap().remove("earlier");
    let read: FinishCard = serde_json::from_value(older).unwrap();
    assert!(!read.earlier);
    let replies = [
        ("design_state", Some(state)),
        (
            "design_summary",
            Some(json!({"state": "finishing", "note_count": 4})),
        ),
        (
            "design_resolved",
            Some(
                json!({"target": target_value(), "label": "button", "asset": null, "data_url": null}),
            ),
        ),
        ("design_note", Some(note_view_value())),
        (
            "design_finish",
            Some(
                json!({"capture": "c", "head": "abc", "note_version": 2, "cards": [card], "no_change_only": false}),
            ),
        ),
        (
            "design_asset",
            Some(
                json!({"id": ASSET, "data_url": "data:image/png;base64,AA", "width": 1, "height": 2, "caption": null}),
            ),
        ),
        ("design_done", None),
        ("design_agent", Some(json!({"message": "ok"}))),
    ];
    for (name, data) in replies {
        let wire = match data {
            Some(data) => json!({"reply": name, "data": data}),
            None => json!({"reply": name}),
        };
        let reply: Reply =
            serde_json::from_value(wire.clone()).unwrap_or_else(|e| panic!("{wire}: {e}"));
        assert_eq!(serde_json::to_value(&reply).unwrap(), wire);
    }
}

#[test]
fn every_design_event_is_a_design_frame_with_its_wire_name() {
    let events = [
        json!({"event": "frame", "seq": 1, "jpeg_b64": "AA", "w": 2, "h": 3, "at": at()}),
        json!({"event": "selection", "box": rect(), "label": "button"}),
        json!({"event": "session", "state": "live", "degraded": false, "preview_path": "/",
               "commit": null, "browser": {"status": "running"}, "note_version": 1}),
        {
            let mut note = note_view_value();
            note["event"] = json!("note");
            note
        },
        json!({"event": "blocked_hosts", "hosts": ["x.test"]}),
        json!({"event": "agent_progress", "line": "l"}),
        json!({"event": "dialog", "kind": "prompt", "message": "m", "default_prompt": "d"}),
        json!({"event": "toast", "message": "m"}),
        json!({"event": "ended", "reason": "finished"}),
    ];
    for event in events {
        let wire = json!({"frame": "design", "sub": 3, "event": event});
        let frame: ServerFrame =
            serde_json::from_value(wire.clone()).unwrap_or_else(|e| panic!("{wire}: {e}"));
        assert!(matches!(frame, ServerFrame::Design { sub: 3, .. }));
        assert_eq!(serde_json::to_value(&frame).unwrap(), wire);
    }
}

#[test]
fn browser_statuses_use_a_status_tag() {
    for (status, wire) in [
        (BrowserStatus::Idle, json!({"status": "idle"})),
        (
            BrowserStatus::QueuedForSlot,
            json!({"status": "queued_for_slot"}),
        ),
        (
            BrowserStatus::CrashedTooOften,
            json!({"status": "crashed_too_often"}),
        ),
        (BrowserStatus::Unsandboxed, json!({"status": "unsandboxed"})),
        (
            BrowserStatus::SandboxUnavailable { hint: "h".into() },
            json!({"status": "sandbox_unavailable", "hint": "h"}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&status).unwrap(), wire);
    }
}

#[test]
fn the_records_decode_from_their_wire_form() {
    serde_json::from_value::<DesignNote>(note_value()).unwrap();
    serde_json::from_value::<DesignSession>(session_value()).unwrap();
}

#[test]
fn the_design_errors_have_their_code_names() {
    for (refusal, name) in [
        (DesignRefusal::Gone, "gone"),
        (DesignRefusal::ReadOnly, "read_only"),
        (DesignRefusal::Finishing, "finishing"),
        (DesignRefusal::Stale, "stale"),
        (DesignRefusal::Conflict, "conflict"),
        (DesignRefusal::NotYours, "forbidden"),
    ] {
        let error = ArchonError::Design {
            code: refusal,
            message: "no".into(),
        };
        let reply = ErrorReply::from(&error);
        assert_eq!(serde_json::to_value(reply.code).unwrap(), json!(name));
        assert_eq!(reply.message, "no");
    }
}

#[test]
fn the_version_and_feature_are_25() {
    assert_eq!(PROTOCOL_VERSION, 25);
    assert_eq!(feature::DESIGN_SESSIONS, "design_sessions");
    assert!(feature::ALL.contains(&feature::DESIGN_SESSIONS));
    let welcome = negotiate(
        ClientKind::ArchonWeb,
        25,
        "web",
        &[feature::DESIGN_SESSIONS],
    )
    .unwrap();
    assert_eq!(welcome.features, ["design_sessions"]);
}
