//! Live design sessions, daemon side (design.md §8, §11): the note loop, the
//! browser's lifecycle, agent reports, caller checks and observers. The
//! browser is a fake; nothing here starts Chromium.

use crate::common::design::{Fixture, design_config, jpeg};
use archon_core::design::{NoteStatus, SessionState};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{AgentNoteStatus, Reply, Request};
use archon_daemon::design::StageEnd;

pub fn refused(result: &Result<Reply, ArchonError>, code: DesignRefusal) {
    match result {
        Err(ArchonError::Design { code: got, .. }) => assert_eq!(*got, code),
        other => panic!("expected {code:?}, got {other:?}"),
    }
}

pub async fn state(f: &Fixture) -> archon_core::protocol::DesignStateView {
    match f
        .operator(Request::DesignGet {
            item: f.item.clone(),
        })
        .await
        .expect("design state")
    {
        Reply::DesignState(view) => view,
        other => panic!("DesignGet replied {other:?}"),
    }
}

pub async fn note(f: &Fixture, id: &str) -> archon_core::design::DesignNote {
    state(f)
        .await
        .notes
        .into_iter()
        .find(|n| n.note.id == id)
        .unwrap_or_else(|| panic!("note {id}"))
        .note
}

pub async fn add(f: &Fixture, key: &str, text: &str) -> archon_core::design::DesignNote {
    add_with(f, key, text, false, None).await
}

pub async fn add_with(
    f: &Fixture,
    key: &str,
    text: &str,
    send_picture: bool,
    details_text: Option<&str>,
) -> archon_core::design::DesignNote {
    let reply = f
        .operator(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: key.into(),
            text: text.into(),
            asset: None,
            send_picture,
            details_text: details_text.map(str::to_owned),
        })
        .await
        .expect("note added");
    match reply {
        Reply::DesignNote(view) => view.note,
        other => panic!("DesignNoteAdd replied {other:?}"),
    }
}

pub async fn report(
    f: &Fixture,
    id: &str,
    status: AgentNoteStatus,
    commit: Option<String>,
    reply: Option<&str>,
) -> Result<Reply, ArchonError> {
    f.as_agent(Request::DesignNoteReport {
        id: id.into(),
        status,
        reply: reply.map(str::to_owned),
        commit,
        no_change: false,
        caller: Some(f.agent.clone()),
    })
    .await
}

pub fn key(f: &Fixture, id: &str, delivery: u32) -> String {
    format!("design:{}:{id}:{delivery}", f.session)
}

/// T-I1: preview, a note, delivery once, the agent's reports, the after picture.
#[tokio::test]
async fn a_note_reaches_the_agent_once_and_settles_with_its_commit() {
    let f = Fixture::new().await;
    assert_eq!(f.factory.launched(), 1, "the browser starts lazily, once");
    assert_eq!(
        f.factory.browser(0).calls(|c| c.navigated.clone()),
        ["http://127.0.0.1:5173/"]
    );
    assert_eq!(state(&f).await.session.unwrap().state, SessionState::Live);

    let added = add(&f, "k1", "Make the save button green").await;
    assert_eq!(added.id, "D1");
    let sent = note(&f, "D1").await;
    assert_eq!(sent.status, NoteStatus::Sent);
    assert!(
        sent.base_commit.is_some(),
        "HEAD is recorded when it is sent"
    );
    let calls = f.h.backend.message_calls();
    assert_eq!(
        calls.iter().filter(|k| **k == key(&f, "D1", 1)).count(),
        1,
        "one keyed delivery"
    );
    let messages = f.h.backend.messages();
    let text = &messages.last().unwrap().1;
    assert!(text.contains("D1") && text.contains("Make the save button green"));

    report(&f, "D1", AgentNoteStatus::Working, None, None)
        .await
        .expect("working");
    assert_eq!(note(&f, "D1").await.status, NoteStatus::Working);
    let commit = f.commit(
        "button.css",
        "button { color: green }\n",
        "Make the save button green\n\nArchon-Note: D1\n",
    );
    report(
        &f,
        "D1",
        AgentNoteStatus::Done,
        Some(commit.clone()),
        Some("Done"),
    )
    .await
    .expect("done");
    let done = note(&f, "D1").await;
    assert_eq!(done.status, NoteStatus::Done);
    assert_eq!(done.commit.as_deref(), Some(commit.as_str()));
    assert!(done.after_asset.is_some(), "an after picture is kept");
}

/// T-I2: notes go in order, one in flight; the same client key adds one note.
#[tokio::test]
async fn notes_are_sent_in_order_one_at_a_time() {
    let f = Fixture::new().await;
    let first = add(&f, "k1", "first").await;
    let again = add(&f, "k1", "first").await;
    assert_eq!(first.id, again.id, "a repeated client key is one note");
    let second = add(&f, "k2", "second").await;
    assert_eq!(second.id, "D2");
    assert_eq!(note(&f, "D1").await.status, NoteStatus::Sent);
    assert_eq!(
        note(&f, "D2").await.status,
        NoteStatus::Queued,
        "one in flight"
    );
    assert!(!f.h.backend.message_calls().contains(&key(&f, "D2", 1)));

    report(&f, "D1", AgentNoteStatus::Working, None, None)
        .await
        .unwrap();
    report(&f, "D1", AgentNoteStatus::Failed, None, Some("cannot"))
        .await
        .unwrap();
    assert_eq!(
        note(&f, "D2").await.status,
        NoteStatus::Sent,
        "the next goes out"
    );
    assert!(f.h.backend.message_calls().contains(&key(&f, "D2", 1)));
}

/// T-I3: a question holds the queue until the operator answers.
#[tokio::test]
async fn a_question_holds_the_queue_until_it_is_answered() {
    let f = Fixture::new().await;
    add(&f, "k1", "first").await;
    add(&f, "k2", "second").await;
    report(
        &f,
        "D1",
        AgentNoteStatus::NeedsReply,
        None,
        Some("Which green?"),
    )
    .await
    .unwrap();
    assert_eq!(note(&f, "D1").await.status, NoteStatus::NeedsReply);
    assert_eq!(note(&f, "D2").await.status, NoteStatus::Queued);

    f.operator(Request::DesignNoteReply {
        item: f.item.clone(),
        id: "D1".into(),
        text: "The brand green".into(),
    })
    .await
    .expect("answered");
    assert_eq!(note(&f, "D1").await.status, NoteStatus::Working);
    let told = f.h.backend.messages();
    assert!(told.iter().any(|(_, t)| t.contains("The brand green")));
    assert_eq!(
        note(&f, "D2").await.status,
        NoteStatus::Queued,
        "still one at a time"
    );
}

/// T-I4: when the stage ends, queued notes are cancelled, the session ends and
/// the browser closes; the old session answers `gone`.
#[tokio::test]
async fn ending_the_stage_ends_the_session_and_closes_the_browser() {
    let f = Fixture::new().await;
    add(&f, "k1", "first").await;
    add(&f, "k2", "second").await;
    f.end(StageEnd::Completed).await;

    let ended =
        f.h.ctx
            .store
            .lock()
            .await
            .design_session(&f.session)
            .unwrap()
            .expect("the session row stays");
    assert_eq!(ended.state, SessionState::Ended);
    assert_eq!(note(&f, "D2").await.status, NoteStatus::Cancelled);
    assert_eq!(
        note(&f, "D1").await.status,
        NoteStatus::Failed,
        "in flight, never settled"
    );
    assert!(f.factory.browser(0).calls(|c| c.closed));

    let result = f
        .operator(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: "k3".into(),
            text: "late".into(),
            asset: None,
            send_picture: false,
            details_text: None,
        })
        .await;
    refused(&result, DesignRefusal::Gone);
}

/// T-I7: the token in the details never reaches the agent when the details
/// text is cleared and the picture is withheld.
#[tokio::test]
async fn a_withheld_secret_is_in_no_message() {
    let f = Fixture::new().await;
    add_with(&f, "k1", "Fix the label", false, Some("")).await;
    for (_, text) in f.h.backend.messages() {
        assert!(!text.contains("sk-test-"), "no token in {text}");
    }
    assert!(
        f.h.backend
            .message_attachments(&key(&f, "D1", 1))
            .is_empty()
    );

    let f = Fixture::new().await;
    add_with(&f, "k1", "Fix the label", false, Some("shows sk-test-123")).await;
    let text = f.h.backend.messages().last().unwrap().1.clone();
    assert!(
        text.contains("sk-test-123"),
        "details the operator kept are sent"
    );
    assert!(
        text.contains("untrusted") || text.contains("not instructions"),
        "and fenced as data: {text}"
    );
}

/// T-I8: a crashed page relaunches the browser; the session stays live.
#[tokio::test]
async fn a_crashed_browser_is_relaunched() {
    let f = Fixture::new().await;
    f.factory.browser(0).crash();
    for _ in 0..100 {
        if f.factory.launched() > 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(f.factory.launched(), 2, "relaunched once");
    assert_eq!(state(&f).await.session.unwrap().state, SessionState::Live);
}

/// T-U14: the branch moved under the note: the report is refused and the note
/// fails with `history_changed`.
#[tokio::test]
async fn a_done_report_after_history_moved_fails_the_note() {
    let f = Fixture::new().await;
    add(&f, "k1", "first").await;
    let base = note(&f, "D1").await.base_commit.expect("base");
    f.commit("a.txt", "a\n", "first change\n\nArchon-Note: D1\n");
    // The agent rewrites the base away.
    crate::common::git(&f.worktree.worktree, &["reset", "--hard", "--quiet", &base]);
    let other = f.commit("b.txt", "b\n", "unrelated\n\nArchon-Note: D1\n");
    crate::common::git(
        &f.worktree.worktree,
        &[
            "commit",
            "--amend",
            "--quiet",
            "--allow-empty",
            "-m",
            "rewritten",
        ],
    );
    let result = report(&f, "D1", AgentNoteStatus::Done, Some(other), None).await;
    assert!(
        result.is_ok(),
        "a history report settles the note, it is not an error"
    );
    let settled = note(&f, "D1").await;
    assert_eq!(settled.status, NoteStatus::Failed);
    assert_eq!(
        settled.fail_reason,
        Some(archon_core::design::FailReason::HistoryChanged)
    );
}

/// MCP caller checks: `design_note` and `design_preview` need the stage
/// agent's own caller identity, and only for the note in flight.
#[tokio::test]
async fn only_the_stage_agent_may_report() {
    let f = Fixture::new().await;
    add(&f, "k1", "first").await;
    let anonymous = f
        .as_agent(Request::DesignNoteReport {
            id: "D1".into(),
            status: AgentNoteStatus::Working,
            reply: None,
            commit: None,
            no_change: false,
            caller: None,
        })
        .await;
    assert!(
        matches!(anonymous, Err(ArchonError::Forbidden(_))),
        "{anonymous:?}"
    );
    let stranger = f
        .as_agent(Request::DesignNoteReport {
            id: "D1".into(),
            status: AgentNoteStatus::Working,
            reply: None,
            commit: None,
            no_change: false,
            caller: Some(archon_core::aether_ids::AetherAgentId::parse("agt_stranger").unwrap()),
        })
        .await;
    assert!(
        matches!(stranger, Err(ArchonError::Forbidden(_))),
        "{stranger:?}"
    );
    // A note not yet sent is not the agent's to report.
    add(&f, "k2", "second").await;
    let queued = report(&f, "D2", AgentNoteStatus::Done, None, None).await;
    refused(&queued, DesignRefusal::NotYours);
    // The operator's client cannot call the agent's tools.
    let operator = f
        .operator(Request::DesignPreview {
            url: "http://127.0.0.1:1/".into(),
            caller: None,
        })
        .await;
    assert!(operator.is_err(), "design_preview is for stage agents");
}

/// Observers (the web client) see a summary and change nothing.
#[tokio::test]
async fn observers_see_a_summary_and_cannot_act() {
    let f = Fixture::new().await;
    add(&f, "k1", "first").await;
    match f
        .observer(Request::DesignGet {
            item: f.item.clone(),
        })
        .await
        .unwrap()
    {
        Reply::DesignSummary { state, note_count } => {
            assert_eq!(state, Some(SessionState::Live));
            assert_eq!(note_count, 1);
        }
        other => panic!("observer got {other:?}"),
    }
    let denied = f
        .observer(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: "x".into(),
            text: "no".into(),
            asset: None,
            send_picture: false,
            details_text: None,
        })
        .await;
    assert!(
        matches!(denied, Err(ArchonError::Forbidden(_))),
        "{denied:?}"
    );
    let denied = f
        .observer(Request::DesignAsset {
            item: f.item.clone(),
            id: archon_core::design::DesignAssetId::new(),
        })
        .await;
    assert!(denied.is_err());
}

/// An upload is checked by its bytes: a picture that is not an image is refused.
#[tokio::test]
async fn an_upload_must_be_an_image() {
    use base64::Engine;
    let f = Fixture::new().await;
    let upload = |bytes: Vec<u8>| Request::DesignUpload {
        session: f.session.clone(),
        media_type: "image/jpeg".into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        name: None,
        sketch: None,
        base: None,
        blank: false,
    };
    assert!(f.operator(upload(b"not an image".to_vec())).await.is_err());
    assert!(f.operator(upload(jpeg(32, 32))).await.is_ok());
}

/// With design sessions off, every design request but the read is refused;
/// the read tells a reconnecting client there is nothing.
#[tokio::test]
async fn disabled_sessions_refuse_requests() {
    let f = Fixture::with(design_config()).await;
    let mut off = crate::common::Harness::new().await;
    assert!(!off.ctx.design.enabled());
    let refused = off
        .cli(Request::DesignReload {
            session: f.session.clone(),
        })
        .await;
    assert!(refused.is_err());
    let read = off
        .cli(Request::DesignGet {
            item: f.item.clone(),
        })
        .await;
    assert!(read.is_ok());
    off.restart();
}
