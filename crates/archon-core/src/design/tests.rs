use super::*;
use NoteStatus::*;

const ALL: [NoteStatus; 7] = [Queued, Sent, Working, NeedsReply, Done, Failed, Cancelled];

#[test]
fn transition_table_for_changes_and_pages() {
    for kind in [NoteKind::Change, NoteKind::Page] {
        let legal = |from, to| transition_allowed(kind, from, to);
        assert!(legal(Queued, Sent) && legal(Queued, Cancelled));
        assert!(!legal(Queued, Working) && !legal(Queued, Done));
        for to in [Working, NeedsReply, Done, Failed, Cancelled] {
            assert!(legal(Sent, to), "sent -> {to:?}");
        }
        for to in [NeedsReply, Done, Failed, Cancelled] {
            assert!(legal(Working, to));
        }
        for to in [Working, Done, Failed, Cancelled] {
            assert!(legal(NeedsReply, to));
        }
        assert!(!legal(Working, Sent) && !legal(Working, Queued));
    }
}

#[test]
fn a_revert_never_passes_through_sent_or_needs_reply() {
    for from in ALL {
        for to in ALL {
            if matches!(to, Sent | NeedsReply) || matches!(from, Sent | NeedsReply) {
                assert!(!transition_allowed(NoteKind::Revert, from, to));
            }
        }
    }
    assert!(transition_allowed(NoteKind::Revert, Queued, Working));
    assert!(transition_allowed(NoteKind::Revert, Queued, Failed));
    assert!(transition_allowed(NoteKind::Revert, Queued, Cancelled));
    assert!(transition_allowed(NoteKind::Revert, Working, Done));
    assert!(transition_allowed(NoteKind::Revert, Working, Failed));
    assert!(!transition_allowed(NoteKind::Revert, Working, Cancelled));
}

#[test]
fn terminal_states_are_final_and_there_is_no_undone() {
    for kind in [NoteKind::Change, NoteKind::Page, NoteKind::Revert] {
        for from in [Done, Failed, Cancelled] {
            for to in ALL {
                assert!(!transition_allowed(kind, from, to));
            }
        }
    }
    for status in ALL {
        let text = serde_json::to_string(&status).unwrap();
        assert!(!text.contains("undone"));
    }
    assert!(serde_json::from_str::<NoteStatus>("\"undone\"").is_err());
    assert!(is_in_flight(Sent) && is_in_flight(Working) && is_in_flight(NeedsReply));
    assert!(!is_in_flight(Queued) && !is_in_flight(Done));
}

#[test]
fn preview_urls() {
    for ok in [
        "http://127.0.0.1:5173/",
        "http://localhost:3000/x?y=1",
        "http://[::1]:8080/",
        "http://localhost",
    ] {
        assert_eq!(validate_preview_url(ok).unwrap(), ok);
    }
    for bad in [
        "http://0.0.0.0:3000/",
        "http://169.254.169.254/",
        "http://10.0.0.1/",
        "file:///etc/passwd",
        "https://127.0.0.1/",
        "http://evil@127.0.0.1.nip.io",
        "http://u:p@127.0.0.1/",
        "http://127.0.0.1.nip.io/",
        "http://127.0.0.1:99999/",
        "http://127.0.0.1:/",
        "",
    ] {
        assert!(validate_preview_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn note_numbers() {
    assert_eq!(note_id(12), "D12");
    assert_eq!(note_number("D12"), Some(12));
    assert_eq!(note_number("D12-final"), Some(12));
    assert_eq!(note_number("D"), None);
    assert_eq!(note_number("X1"), None);
    assert_eq!(note_number("D1x"), None);
    assert_eq!(next_note_number(None, &["D1", "D1-final"]), 2);
    assert_eq!(next_note_number(None, &["D4-mobile"]), 5);
    assert_eq!(next_note_number(Some(7), &["D3"]), 7);
    assert_eq!(next_note_number(None, &[]), 1);
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
}

fn target(path: &str, selector: Option<&str>, region: bool, r: Rect) -> Target {
    Target {
        path: path.into(),
        selector: selector.map(Into::into),
        role_name: None,
        component: None,
        rect: r,
        viewport: Viewport {
            w: 1440,
            h: 900,
            preset: ViewportPreset::Desktop,
        },
        styles: BTreeMap::new(),
        region,
        steps: Box::default(),
    }
}

fn note(number: u64, kind: NoteKind, status: NoteStatus, target: Option<Target>) -> DesignNote {
    let at = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
    DesignNote {
        item: QueueItemId::new(),
        id: note_id(number),
        number,
        session: DesignSessionId::new(),
        kind,
        text: String::new(),
        details_text: None,
        send_picture: false,
        target,
        viewport: Viewport {
            w: 1,
            h: 1,
            preset: ViewportPreset::Phone,
        },
        status,
        fail_reason: None,
        base_commit: None,
        commit: None,
        no_change: false,
        reverts: None,
        resent_from: None,
        superseded_by: None,
        thread: vec![],
        client_key: String::new(),
        delivery: 0,
        before_asset: None,
        after_asset: None,
        final_asset: None,
        final_included: None,
        captured_commit: None,
        created_by_device: None,
        created_at: at,
        updated_at: at,
    }
}

#[test]
fn iou_boundaries() {
    let a = rect(0.0, 0.0, 100.0, 100.0);
    // intersection 51x100 over union 14900: just above and below 0.5.
    let below = rect(51.0, 0.0, 100.0, 100.0); // 49/151
    assert!(iou(&a, &below) < 0.5);
    assert!(iou(&a, &rect(0.0, 0.0, 100.0, 49.0)) < 0.5);
    assert!((iou(&a, &rect(0.0, 0.0, 100.0, 50.0)) - 0.5).abs() < 1e-12);
    assert_eq!(iou(&a, &rect(200.0, 0.0, 10.0, 10.0)), 0.0);
    let t = |r| target("/", None, true, r);
    assert!(!same_target(&t(a), &t(rect(0.0, 0.0, 100.0, 49.0))));
    assert!(same_target(&t(a), &t(rect(0.0, 0.0, 100.0, 50.0))));
}

#[test]
fn same_target_rules_and_supersession() {
    let r = rect(0.0, 0.0, 10.0, 10.0);
    let el = |p, s| target(p, Some(s), false, r);
    assert!(same_target(&el("/a", "b"), &el("/a", "b")));
    assert!(!same_target(&el("/a", "b"), &el("/a", "c")));
    assert!(!same_target(&el("/a", "b"), &el("/x", "b")));
    assert!(!same_target(
        &el("/a", "b"),
        &target("/a", Some("b"), true, r)
    ));

    let n1 = note(1, NoteKind::Change, Done, Some(el("/a", "b")));
    let n2 = note(2, NoteKind::Change, Done, Some(el("/a", "c")));
    let n3 = note(3, NoteKind::Change, Done, Some(el("/a", "b")));
    let n4 = note(4, NoteKind::Page, Done, None);
    let n5 = note(5, NoteKind::Change, Done, Some(el("/a", "b")));
    let map = auto_supersession(&[&n5, &n1, &n2, &n3, &n4]);
    assert_eq!(map.get("D1").map(String::as_str), Some("D5"));
    assert_eq!(map.get("D3").map(String::as_str), Some("D5"));
    assert!(!map.contains_key("D2") && !map.contains_key("D5") && !map.contains_key("D4"));
}

#[test]
fn text_is_cleaned() {
    assert_eq!(clean_text("  a\u{7}b\nc\td\u{0}  ", 100), "ab\nc\td");
    assert_eq!(clean_text("abcdef", 3), "abc");
    assert_eq!(clean_text("éééé", 2), "éé");
}

#[test]
fn only_done_reverts_undo() {
    let mut done = note(2, NoteKind::Revert, Done, None);
    done.reverts = Some("D1".into());
    let mut failed = note(3, NoteKind::Revert, Failed, None);
    failed.reverts = Some("D0".into());
    let mut working = note(4, NoteKind::Revert, Working, None);
    working.reverts = Some("D9".into());
    let mut change = note(5, NoteKind::Change, Done, None);
    change.reverts = Some("D8".into());
    let map = undone_by(&[done, failed, working, change]);
    assert_eq!(map.len(), 1);
    assert_eq!(map["D1"], "D2");
}

#[test]
fn ids_and_presets() {
    assert!(DesignSessionId::new().0.starts_with("ds_"));
    assert!(DesignAssetId::new().to_string().starts_with("da_"));
    assert_eq!(ViewportPreset::Phone.size(), (390, 844));
    assert_eq!(ViewportPreset::Desktop.size(), (1440, 900));
    assert_eq!(ViewportPreset::Tablet.size(), (834, 1112));
    assert!(ViewportPreset::Tablet.touch() && !ViewportPreset::Desktop.touch());
}
