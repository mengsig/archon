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
            follows: None,
            refs: Vec::new(),
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
        path: None,
        steps: Vec::new(),
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

fn reject_owed_writes(f: &Fixture) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(&f.h.ctx.paths.store).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_design_owed BEFORE INSERT ON kv
         WHEN NEW.key GLOB 'design_owed:*'
         BEGIN SELECT RAISE(ABORT, 'design outbox unavailable'); END;",
    )
    .unwrap();
    db
}

fn add_request(f: &Fixture, key: &str, text: &str) -> Request {
    Request::DesignNoteAdd {
        session: f.session.clone(),
        client_key: key.into(),
        text: text.into(),
        asset: None,
        send_picture: false,
        details_text: None,
        follows: None,
        refs: Vec::new(),
    }
}

fn answer_request(f: &Fixture, text: &str) -> Request {
    Request::DesignNoteReply {
        item: f.item.clone(),
        id: "D1".into(),
        text: text.into(),
    }
}

#[tokio::test]
async fn a_failed_outbox_write_leaves_the_note_queued_for_the_same_first_delivery() {
    let f = Fixture::new().await;
    let db = reject_owed_writes(&f);
    let result = f.operator(add_request(&f, "first", "Make it green")).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("design outbox unavailable")
    );
    let queued = note(&f, "D1").await;
    assert_eq!(queued.status, NoteStatus::Queued);
    assert_eq!(queued.delivery, 0);
    assert!(queued.base_commit.is_none());
    assert!(f.h.backend.message_calls().is_empty());

    db.execute_batch("DROP TRIGGER reject_design_owed").unwrap();
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        add(&f, "first", "Make it green"),
    )
    .await
    .expect("the retried add releases the item lock before pumping");
    assert_eq!(sent.id, queued.id);
    assert_eq!(sent.status, NoteStatus::Sent);
    assert_eq!(sent.delivery, 1);
    assert_eq!(f.h.backend.message_calls(), [key(&f, "D1", 1)]);
}

#[tokio::test]
async fn an_answer_and_its_notice_commit_together_and_only_one_answer_wins() {
    let f = Fixture::new().await;
    add(&f, "first", "Make it green").await;
    report(
        &f,
        "D1",
        AgentNoteStatus::NeedsReply,
        None,
        Some("Which green?"),
    )
    .await
    .unwrap();
    let before = note(&f, "D1").await;
    let db = reject_owed_writes(&f);
    let failed = f.operator(answer_request(&f, "uncommitted answer")).await;
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .contains("design outbox unavailable")
    );
    let after = note(&f, "D1").await;
    assert_eq!(after.status, NoteStatus::NeedsReply);
    assert_eq!(after.thread.len(), before.thread.len());

    db.execute_batch("DROP TRIGGER reject_design_owed").unwrap();
    let (first, other) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(
            biased;
            f.operator(answer_request(&f, "brand green")),
            f.operator(answer_request(&f, "forest green")),
        )
    })
    .await
    .expect("competing answers finish");
    first.expect("the first answer is accepted");
    assert!(matches!(other, Err(ArchonError::Invalid(_))));
    let working = note(&f, "D1").await;
    assert_eq!(working.status, NoteStatus::Working);
    assert_eq!(working.thread.len(), before.thread.len() + 1);
    assert_eq!(working.thread.last().unwrap().text, "brand green");
    let messages = f.h.backend.messages();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].1.contains("brand green"));
}

#[tokio::test]
async fn a_restart_and_tell_retry_the_same_owed_first_delivery() {
    let mut f = Fixture::with_preview(design_config()).await;
    let db = rusqlite::Connection::open(&f.h.ctx.paths.store).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_design_ack BEFORE DELETE ON kv
         WHEN OLD.key GLOB 'design_owed:*'
         BEGIN SELECT RAISE(ABORT, 'design ack unavailable'); END;",
    )
    .unwrap();
    let failed = f.operator(add_request(&f, "first", "Make it green")).await;
    assert!(
        failed
            .unwrap_err()
            .to_string()
            .contains("design ack unavailable")
    );
    let first = note(&f, "D1").await;
    assert_eq!(first.status, NoteStatus::Sent);
    assert_eq!(
        f.h.backend.messages().len(),
        1,
        "Aether accepted before the ack failed"
    );
    db.execute_batch("DROP TRIGGER reject_design_ack").unwrap();
    drop(db);
    f.h.restart();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        f.operator(Request::DesignTellAgent {
            session: f.session.clone(),
        }),
    )
    .await
    .expect("tell releases its locks")
    .expect("tell retries");
    let retried = note(&f, "D1").await;
    assert_eq!(retried.delivery, 1);
    assert_eq!(retried.base_commit, first.base_commit);
    let key = key(&f, "D1", 1);
    assert_eq!(f.h.backend.message_calls(), [key.clone(), key]);
    assert_eq!(f.h.backend.messages().len(), 1);
}

#[tokio::test]
async fn a_cancelled_note_never_sends_a_legacy_uncommitted_delivery() {
    let f = Fixture::new().await;
    add(&f, "first", "Working").await;
    let waiting = add(&f, "second", "Must not arrive").await;
    assert_eq!(waiting.status, NoteStatus::Queued);
    let kv = archon_queue::store::keys::design_owed(&f.session);
    let stale = serde_json::json!([{
        "key": key(&f, &waiting.id, 1),
        "text": "Must not arrive",
        "asset": null,
        "item": f.item,
    }]);
    f.h.ctx
        .store
        .lock()
        .await
        .kv_set(&kv, &stale.to_string())
        .unwrap();
    f.operator(Request::DesignNoteCancel {
        item: f.item.clone(),
        id: waiting.id.clone(),
    })
    .await
    .expect("cancel the queued note");
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        report(&f, "D1", AgentNoteStatus::Failed, None, None),
    )
    .await
    .expect("the note pump completes")
    .expect("the first note settles");
    assert_eq!(note(&f, &waiting.id).await.status, NoteStatus::Cancelled);
    assert!(f.h.ctx.store.lock().await.kv_get(&kv).unwrap().is_none());
    assert!(
        !f.h.backend
            .message_calls()
            .contains(&key(&f, &waiting.id, 1))
    );
}

#[tokio::test]
async fn a_note_key_retries_original_words_after_an_edit_but_rejects_a_different_request() {
    let f = Fixture::new().await;
    add(&f, "busy", "Working").await;
    let queued = add(&f, "key", "Original").await;
    f.operator(Request::DesignNoteEdit {
        item: f.item.clone(),
        id: queued.id.clone(),
        text: "Edited".into(),
    })
    .await
    .expect("edit the queued note");
    let retry = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        add(&f, "key", "Original"),
    )
    .await
    .expect("retry completes");
    assert_eq!(retry.id, queued.id);
    assert_eq!(retry.text, "Edited");
    assert!(matches!(
        f.operator(add_request(&f, "key", "Edited")).await,
        Err(ArchonError::Invalid(_))
    ));
    assert_eq!(state(&f).await.notes.len(), 2);
}

async fn replace_agent(f: &Fixture) -> archon_core::aether_ids::AetherAgentId {
    use archon_core::ports::AgentBackend;
    use archon_core::run::StageState;
    let mut request = f.h.backend.requests().pop().expect("stage was spawned");
    request.idempotency_key.push_str(":replacement");
    let replacement = f.h.backend.spawn(request).await.unwrap();
    let mut store = f.h.ctx.store.lock().await;
    let mut stage = store.stage_run_by_agent(&f.agent).unwrap().unwrap();
    stage.state = StageState::Cancelled;
    stage.finished_at = Some(archon_core::now());
    store.upsert_stage_run(&stage).unwrap();
    stage.id = archon_core::id::StageRunId::new();
    stage.state = StageState::Running;
    stage.finished_at = None;
    stage.aether_agent = Some(replacement.agent.clone());
    stage.aether_task = Some(replacement.task);
    store.upsert_stage_run(&stage).unwrap();
    drop(store);
    let rebound = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        f.h.ctx.design.stage_started(
            &f.h.ctx,
            &f.item,
            &f.run,
            &f.stage_id,
            f.attempt,
            &replacement.agent,
            f.worktree.clone(),
        ),
    )
    .await
    .expect("the replacement can bind")
    .expect("stage rebinds");
    assert_ne!(replacement.agent, f.agent);
    assert_eq!(rebound, f.session);
    replacement.agent
}

#[tokio::test]
async fn a_rebound_agent_resumes_the_acknowledged_in_flight_note_with_its_thread() {
    let f = Fixture::new().await;
    add(&f, "first", "Make it green").await;
    report(
        &f,
        "D1",
        AgentNoteStatus::NeedsReply,
        None,
        Some("Which green?"),
    )
    .await
    .unwrap();
    let before = note(&f, "D1").await;
    let head = f.head();
    let replacement = replace_agent(&f).await;

    let next = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        add(&f, "second", "Next change"),
    )
    .await
    .expect("queue pumps to the new agent");
    let after = note(&f, "D1").await;
    assert_eq!(next.status, NoteStatus::Queued);
    assert_eq!(after.status, NoteStatus::NeedsReply);
    assert_eq!(after.thread.len(), before.thread.len());
    assert_eq!(after.base_commit, before.base_commit);
    assert_eq!(after.delivery, before.delivery);
    assert_eq!(f.head(), head);
    let messages = f.h.backend.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].0, replacement);
    assert!(messages[1].1.contains("Which green?"));
    assert!(messages[1].1.contains("Wait for the answer"));
    assert!(f.h.backend.message_calls()[1].ends_with(&format!(":agent:{replacement}")));
}

#[tokio::test]
async fn an_owed_note_is_rekeyed_once_for_the_replacement_agent() {
    let f = Fixture::new().await;
    f.h.backend
        .fail_next_message(ArchonError::Unavailable("offline".into()));
    let before = add(&f, "first", "Make it green").await;
    assert_eq!(before.status, NoteStatus::Sent);
    assert!(f.h.backend.messages().is_empty());
    let replacement = replace_agent(&f).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        f.operator(Request::DesignTellAgent {
            session: f.session.clone(),
        }),
    )
    .await
    .expect("tell finishes")
    .expect("tell retries the original delivery");
    let after = note(&f, "D1").await;
    assert_eq!(after.delivery, 1);
    assert_eq!(after.base_commit, before.base_commit);
    assert_eq!(
        f.h.backend.message_calls(),
        [
            key(&f, "D1", 1),
            format!("{}:agent:{replacement}", key(&f, "D1", 1)),
        ]
    );
    let messages = f.h.backend.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, replacement);
    assert!(messages[0].1.contains("Make it green"));
}

#[tokio::test]
async fn an_add_waiting_through_finish_cannot_modify_the_session() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let f = Fixture::new().await;
    let lock = f.h.ctx.design.item_lock_for_tests(&f.item);
    let held = lock.lock().await;
    let mut waiting = std::pin::pin!(f.operator(add_request(&f, "key", "Do not add")));
    poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    {
        let mut store = f.h.ctx.store.lock().await;
        let current = store.design_session(&f.session).unwrap().unwrap();
        store
            .design_session_transition(
                &f.session,
                SessionState::Live,
                SessionState::Finishing,
                Some(archon_queue::store::FinishRecord {
                    capture: "capture-1".into(),
                    head: f.head(),
                    note_version: current.note_version,
                    choices: None,
                }),
                false,
            )
            .unwrap();
    }
    drop(held);
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), waiting)
        .await
        .expect("waiting add finishes");
    refused(&result, DesignRefusal::Finishing);
    assert!(state(&f).await.notes.is_empty());
    assert!(f.h.backend.messages().is_empty());
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
            follows: None,
            refs: Vec::new(),
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
            path: None,
            steps: Vec::new(),
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
            path: None,
            steps: Vec::new(),
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

#[tokio::test]
async fn settled_design_agent_cannot_use_its_still_open_session() {
    let f = Fixture::new().await;
    let mut store = f.h.ctx.store.lock().await;
    let mut stage = store.stage_run_by_agent(&f.agent).unwrap().unwrap();
    stage.state = archon_core::run::StageState::Completed;
    stage.finished_at = Some(archon_core::now());
    store.upsert_stage_run(&stage).unwrap();
    assert!(store.design_current_session(&f.item).unwrap().is_some());
    drop(store);
    let result = f
        .as_agent(Request::DesignHistory {
            caller: Some(f.agent.clone()),
            id: None,
        })
        .await;
    assert!(
        matches!(result, Err(ArchonError::Forbidden(_))),
        "{result:?}"
    );
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
            follows: None,
            refs: Vec::new(),
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

/// F6: a picture the operator takes during the review changes the notes'
/// version, and Confirm read that as the code changing ("refresh the
/// review"). The capture comes back with a new id that Confirm accepts.
#[tokio::test]
async fn a_picture_taken_during_the_review_does_not_refuse_confirm() {
    let f = Fixture::new().await;
    let (id, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let review = |reply| match reply {
        Reply::DesignFinish(view) => view,
        other => panic!("{other:?}"),
    };
    let first = review(
        f.operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect("finish"),
    );
    // The operator looks at the phone layout before pressing Capture: the
    // picture is still taken at the note's preset, which its reference
    // records, and the preview goes back to what they had.
    f.operator(Request::DesignViewport {
        session: f.session.clone(),
        preset: archon_core::design::ViewportPreset::Phone,
    })
    .await
    .expect("phone");
    let shots = f.factory.browser(0).calls(|c| c.captures.len());
    let after = review(
        f.operator(Request::DesignFinalCapture {
            session: f.session.clone(),
            capture: first.capture.clone(),
            note: id.clone(),
        })
        .await
        .expect("picture kept"),
    );
    assert_ne!(
        after.capture, first.capture,
        "re-stamped at the new version"
    );
    let (widths, taken) = f.factory.browser(0).calls(|c| {
        (
            c.viewports
                .iter()
                .rev()
                .take(2)
                .map(|v| v.width)
                .collect::<Vec<_>>(),
            c.captures.len(),
        )
    });
    assert_eq!(taken, shots + 1, "one picture");
    assert_eq!(widths, [390, 1440], "shot at Desktop, then back to Phone");
    let stale = f
        .operator(Request::DesignFinishConfirm {
            session: f.session.clone(),
            capture: first.capture.clone(),
            include: [(id.clone(), true)].into(),
            superseded: Default::default(),
            confirm_no_change: false,
        })
        .await;
    assert!(stale.is_err(), "the old id is stale");
    f.operator(Request::DesignFinishConfirm {
        session: f.session.clone(),
        capture: after.capture,
        include: [(id, true)].into(),
        superseded: Default::default(),
        confirm_no_change: false,
    })
    .await
    .expect("confirmed after the picture");
}

#[tokio::test]
async fn finish_requires_review_ids_and_a_later_eligible_replacement() {
    let f = Fixture::new().await;
    let (d1, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let (d2, _) = crate::design_undo::done_note(&f, "k2", "b.txt", "two\n").await;
    for (from, to) in [
        (d1.as_str(), "D99"),
        (d2.as_str(), d1.as_str()),
        ("D99", d2.as_str()),
    ] {
        let (_, result) = confirm_with(&f, &[(from, to)], false).await;
        assert!(matches!(result, Err(ArchonError::Invalid(_))), "{result:?}");
        assert_eq!(
            state(&f).await.session.unwrap().state,
            SessionState::Finishing
        );
    }
    let (_, result) = confirm_with(&f, &[(&d1, &d2)], false).await;
    assert!(matches!(result, Ok(Reply::DesignDone)), "{result:?}");
}

#[tokio::test]
async fn confirmation_keeps_picture_inclusion_and_the_undelivered_finish() {
    let f = Fixture::new().await;
    let (id, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let review = finish_view(
        f.operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect("capture"),
    );
    assert!(review.cards[0].asset.is_some());

    f.h.backend
        .fail_next_message(ArchonError::Unavailable("test outage".into()));
    let result = f
        .operator(Request::DesignFinishConfirm {
            session: f.session.clone(),
            capture: review.capture,
            include: [(id.clone(), false)].into(),
            superseded: Default::default(),
            confirm_no_change: false,
        })
        .await;
    assert!(matches!(result, Ok(Reply::DesignDone)), "{result:?}");
    let store = f.h.ctx.store.lock().await;
    assert_eq!(
        store.design_session(&f.session).unwrap().unwrap().state,
        SessionState::Confirmed
    );
    assert_eq!(
        store
            .design_note(&f.item, &id)
            .unwrap()
            .unwrap()
            .final_included,
        Some(false)
    );
    let key = archon_queue::store::keys::design_owed(&f.session);
    let raw = store
        .kv_get(&key)
        .unwrap()
        .expect("the Finish notice is owed");
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(payload[0]["key"], format!("design:{}:finish", f.session));
}

fn finish_view(reply: Reply) -> archon_core::protocol::DesignFinishView {
    match reply {
        Reply::DesignFinish(view) => view,
        other => panic!("{other:?}"),
    }
}

/// Finish and confirm the fixture's current session with every card included.
async fn finish_and_confirm(f: &Fixture) -> archon_core::protocol::DesignFinishView {
    let view = finish_view(
        f.operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect("finish"),
    );
    f.operator(Request::DesignFinishConfirm {
        session: f.session.clone(),
        capture: view.capture.clone(),
        include: view.cards.iter().map(|c| (c.note.clone(), true)).collect(),
        superseded: Default::default(),
        confirm_no_change: false,
    })
    .await
    .expect("confirmed");
    view
}

#[tokio::test]
async fn a_confirmed_session_owes_finish_to_its_replacement_agent() {
    let f = Fixture::new().await;
    crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let finish = finish_and_confirm(&f).await;
    let key = archon_queue::store::keys::design_owed(&f.session);
    assert!(f.h.ctx.store.lock().await.kv_get(&key).unwrap().is_none());
    assert!(f.h.backend.messages().last().unwrap().1.contains("Finish:"));
    let replacement = replace_agent(&f).await;
    let store = f.h.ctx.store.lock().await;
    let row = store.design_session(&f.session).unwrap().unwrap();
    assert_eq!(row.agent.as_ref(), Some(&replacement));
    assert_eq!(row.state, SessionState::Confirmed);
    let owed: Vec<serde_json::Value> = serde_json::from_str(
        &store
            .kv_get(&key)
            .unwrap()
            .expect("the new agent is owed Finish"),
    )
    .unwrap();
    assert_eq!(owed.len(), 1);
    assert_eq!(
        owed[0]["key"],
        format!("design:{}:finish:agent:{replacement}", f.session)
    );
    let text = owed[0]["text"].as_str().unwrap();
    assert!(text.contains("Stop the dev server without touching the worktree"));
    assert!(text.contains(&finish.capture));
    assert!(text.contains(row.finish_head.as_deref().unwrap()));
}

/// GATE: after Approve the screen said "This design session ended", and a
/// reopen "No design session yet". The design state keeps the item's last
/// session, ended with the gate's decision, and its notes read-only.
#[tokio::test]
async fn an_answered_gate_leaves_its_decision_on_the_design_screen() {
    let f = Fixture::new().await;
    crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    finish_and_confirm(&f).await;
    f.h.ctx
        .design
        .gate_decided(&f.h.ctx, &f.run, &f.stage_id, f.attempt, "approved")
        .await
        .expect("decided");
    let view = state(&f).await;
    let session = view.session.expect("the ended session is still shown");
    assert_eq!(session.id, f.session);
    assert_eq!(session.state, SessionState::Ended);
    assert_eq!(session.end_reason.as_deref(), Some("approved"));
    assert!(
        view.notes.iter().all(|n| n.read_only),
        "nothing changes after the end"
    );
    assert!(view.stage_deadline.is_none());
}

/// S5: after "Ask for changes" Confirm of the revised session was refused,
/// "D1 belongs to an earlier design session", because Finish wrote final
/// pictures onto the earlier session's rows, and the item stuck at "Waiting
/// for your approval". The revised session pictures and confirms only its
/// own notes; the earlier ones are carried as they were confirmed.
#[tokio::test]
async fn a_revised_session_confirms_with_only_its_own_notes() {
    let mut f = Fixture::new().await;
    let (d1, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    finish_and_confirm(&f).await;
    let earlier = note(&f, &d1).await;
    assert!(
        earlier.final_asset.is_some(),
        "D1 was pictured and confirmed"
    );
    f.h.ctx
        .design
        .gate_decided(&f.h.ctx, &f.run, &f.stage_id, f.attempt, "revised")
        .await
        .expect("decided");
    // The kept agent resumes in attempt 2's new session.
    next_session(&mut f).await;
    let (d2, _) = crate::design_undo::done_note(&f, "k2", "b.txt", "two\n").await;
    let shots = f.factory.launched();
    let view = finish_and_confirm(&f).await;
    let card = |id: &str| view.cards.iter().find(|c| c.note == id).expect("a card");
    assert!(
        card(&d1).earlier && !card(&d1).not_found,
        "D1 is carried, not missing"
    );
    assert!(
        card(&d1).data_url.is_some(),
        "with the picture confirmed before"
    );
    assert!(!card(&d2).earlier);
    assert!(shots >= 1);
    assert_eq!(
        note(&f, &d1).await,
        earlier,
        "the earlier session's row is never written again"
    );
    assert!(note(&f, &d2).await.final_included.is_some(), "D2 confirmed");
    assert_eq!(
        state(&f).await.session.unwrap().state,
        SessionState::Confirmed,
        "the revised session reaches its gate"
    );
    // A picture of the earlier note is refused in plain words, not as a write.
    let refused_capture = f
        .operator(Request::DesignFinalCapture {
            session: f.session.clone(),
            capture: view.capture.clone(),
            note: d1.clone(),
        })
        .await;
    assert!(refused_capture.is_err());
}

/// Start the item's next design session (attempt + 1), live, as a revise or
/// a rerun after a failed acceptance leaves it.
async fn next_session(f: &mut Fixture) {
    use archon_core::run::StageState;
    let mut store = f.h.ctx.store.lock().await;
    let mut stage = store.stage_run_by_agent(&f.agent).unwrap().unwrap();
    stage.state = StageState::Completed;
    stage.finished_at = Some(archon_core::now());
    store.upsert_stage_run(&stage).unwrap();
    stage.id = archon_core::id::StageRunId::new();
    stage.attempt += 1;
    stage.started_at = archon_core::now();
    stage.finished_at = None;
    stage.state = StageState::Running;
    store.upsert_stage_run(&stage).unwrap();
    drop(store);
    let next =
        f.h.ctx
            .design
            .stage_started(
                &f.h.ctx,
                &f.item,
                &f.run,
                &f.stage_id,
                f.attempt + 1,
                &f.agent,
                f.worktree.clone(),
            )
            .await
            .expect("the next session starts");
    f.session = next;
    f.attempt += 1;
    f.as_agent(Request::DesignPreview {
        url: crate::common::design::PREVIEW.into(),
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("preview reported");
}

/// Finish and Confirm with `superseded` choices, Confirm's flag as given.
async fn confirm_with(
    f: &Fixture,
    superseded: &[(&str, &str)],
    confirm_no_change: bool,
) -> (
    archon_core::protocol::DesignFinishView,
    Result<Reply, ArchonError>,
) {
    let view = finish_view(
        f.operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect("finish"),
    );
    let confirmed = f
        .operator(Request::DesignFinishConfirm {
            session: f.session.clone(),
            capture: view.capture.clone(),
            include: Default::default(),
            superseded: superseded
                .iter()
                .map(|(a, b)| ((*a).to_owned(), Some((*b).to_owned())))
                .collect(),
            confirm_no_change,
        })
        .await;
    (view, confirmed)
}

/// Item A: a design approved with "D1 superseded by D2", then reopened (the
/// operator answered a failed acceptance). Finish there was refused, "nothing
/// to approve"; after a no-op note the review offered a plain Confirm, which
/// was refused on every click, "confirm that you want to approve with no
/// change", with no control to say so: the review read the notes without the
/// carried replacement, Confirm with it. Now Finish opens a review that asks
/// to approve as it is, and that answer is accepted.
#[tokio::test]
async fn a_session_reopened_after_approval_can_be_approved_as_it_is() {
    let mut f = Fixture::new().await;
    let (d1, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let (d2, _) = crate::design_undo::done_note(&f, "k2", "b.txt", "two\n").await;
    let (_, first) = confirm_with(&f, &[(&d1, &d2)], false).await;
    first.expect("the first session is confirmed");
    f.h.ctx
        .design
        .gate_decided(&f.h.ctx, &f.run, &f.stage_id, f.attempt, "approved")
        .await
        .expect("decided");
    {
        let mut store = f.h.ctx.store.lock().await;
        let mut item = store.item(&f.item).unwrap();
        item.intent = Some(Box::new(archon_core::intent::IntentBinding {
            id: "approved".into(),
            version: 1,
            requirements: vec![d2.clone()],
        }));
        store.update_item(&item).unwrap();
    }
    next_session(&mut f).await;
    let (view, refused) = confirm_with(&f, &[], false).await;
    assert!(
        view.no_change_only,
        "the review asks to approve as it is: {view:?}"
    );
    let refused = refused.expect_err("Confirm needs that answer").to_string();
    assert!(refused.contains("approve the design as it is"), "{refused}");
    f.operator(Request::DesignFinishCancel {
        session: f.session.clone(),
    })
    .await
    .expect("back to editing");
    let (view, confirmed) = confirm_with(&f, &[], true).await;
    assert!(view.no_change_only);
    confirmed.expect("approved as it is");
    assert_eq!(
        state(&f).await.session.unwrap().state,
        SessionState::Confirmed
    );
}

/// A design approved "as it is" binds a requirement named from the note
/// counter, which no note has. Reopened after that approval, the session
/// still offers to approve as it is: the approval came from a design gate,
/// so it is not a first session with nothing to approve.
#[tokio::test]
async fn a_session_reopened_after_an_approval_as_it_is_can_be_approved_again() {
    let mut f = Fixture::new().await;
    // The first session approved as it is (its note-less record is all a
    // later session sees of it).
    f.h.ctx
        .design
        .gate_decided(&f.h.ctx, &f.run, &f.stage_id, f.attempt, "approved")
        .await
        .expect("decided");
    {
        let mut store = f.h.ctx.store.lock().await;
        let record: archon_core::intent::ApprovedIntent = serde_json::from_value(serde_json::json!({
            "id": "as-it-is", "version": 1,
            "source": {"kind": "gate", "item": f.item.to_string(), "gate": format!("design:{}", f.attempt)},
            "operator_words": "Approved as it is", "artifacts": [], "images": [],
            "requirements": [{"id": "D7", "text": "No change", "kind": "behaviour", "owner": "operator", "images": []}]
        }))
        .unwrap();
        store.save_approved_intent(&record).unwrap();
        let mut item = store.item(&f.item).unwrap();
        item.intent = Some(Box::new(archon_core::intent::IntentBinding {
            id: record.id.clone(),
            version: 1,
            requirements: vec!["D7".into()],
        }));
        store.update_item(&item).unwrap();
    }
    next_session(&mut f).await;
    let (view, confirmed) = confirm_with(&f, &[], true).await;
    assert!(
        view.no_change_only,
        "the review asks to approve as it is: {view:?}"
    );
    confirmed.expect("approved as it is again");
    assert_eq!(
        state(&f).await.session.unwrap().state,
        SessionState::Confirmed
    );
}

/// A session with no note and nothing approved before still has nothing to
/// approve.
#[tokio::test]
async fn a_first_session_with_no_note_has_nothing_to_approve() {
    let f = Fixture::new().await;
    let refused = f
        .operator(Request::DesignFinishCapture {
            session: f.session.clone(),
            cancel_queued: false,
            recapture: false,
        })
        .await
        .expect_err("nothing to approve")
        .to_string();
    assert!(refused.contains("nothing to approve"), "{refused}");
}

/// A replacement carried from an earlier review lapses when the replacing
/// note is undone: D1 is active again, as with an automatic supersession.
#[tokio::test]
async fn undoing_a_carried_replacement_brings_the_replaced_note_back() {
    let mut f = Fixture::new().await;
    let (d1, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let (d2, _) = crate::design_undo::done_note(&f, "k2", "b.txt", "two\n").await;
    let (_, first) = confirm_with(&f, &[(&d1, &d2)], false).await;
    first.expect("confirmed with D1 replaced by D2");
    f.h.ctx
        .design
        .gate_decided(&f.h.ctx, &f.run, &f.stage_id, f.attempt, "revised")
        .await
        .expect("decided");
    next_session(&mut f).await;
    f.operator(Request::DesignNoteUndo {
        item: f.item.clone(),
        id: d2.clone(),
    })
    .await
    .expect("undo queued");
    for _ in 0..200 {
        if note(&f, &d2).await.status == NoteStatus::Done
            && state(&f).await.notes.iter().any(|n| {
                n.note.reverts.as_deref() == Some(d2.as_str()) && n.note.status == NoteStatus::Done
            })
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (view, confirmed) = confirm_with(&f, &[], false).await;
    let card = view
        .cards
        .iter()
        .find(|c| c.note == d1)
        .expect("D1 is shown");
    assert_eq!(card.superseded_by, None, "the replacement lapsed");
    assert!(
        view.cards.iter().all(|c| c.note != d2),
        "D2 is undone and no longer counts"
    );
    confirmed.expect("D1 is approved again");
}

#[tokio::test]
async fn a_carried_replacement_lapses_when_its_commit_leaves_history() {
    let mut f = Fixture::new().await;
    let (d1, first_commit) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
    let (d2, _) = crate::design_undo::done_note(&f, "k2", "b.txt", "two\n").await;
    let (_, first) = confirm_with(&f, &[(&d1, &d2)], false).await;
    first.expect("confirmed with D1 replaced by D2");
    f.h.ctx
        .design
        .gate_decided(&f.h.ctx, &f.run, &f.stage_id, f.attempt, "revised")
        .await
        .expect("decided");
    crate::common::git(
        &f.worktree.worktree,
        &["reset", "--hard", "--quiet", &first_commit],
    );
    next_session(&mut f).await;

    let (view, confirmed) = confirm_with(&f, &[], false).await;
    let first = view.cards.iter().find(|card| card.note == d1).unwrap();
    let second = view.cards.iter().find(|card| card.note == d2).unwrap();
    assert!(!first.left_history);
    assert_eq!(first.superseded_by, None);
    assert!(second.left_history);
    confirmed.expect("D1 remains in force");
}

mod stream {
    use super::*;
    use archon_core::protocol::{DesignEvent, ServerFrame, WelcomeDevice};
    use archon_remote::DesignSubscription;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn subscribe(
        f: &Fixture,
        id: u64,
    ) -> (mpsc::Receiver<ServerFrame>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(64);
        let sub = DesignSubscription {
            id,
            request: id,
            session: f.session.clone(),
            device: WelcomeDevice {
                id: "device-1".into(),
                name: "phone".into(),
                role: archon_core::protocol::DeviceRole::Operator,
            },
        };
        let task = {
            let ctx = f.h.ctx.clone();
            tokio::spawn(async move {
                let (_acks, receiver) = tokio::sync::watch::channel(0);
                archon_daemon::design::stream(ctx, sub, receiver, tx).await;
            })
        };
        (rx, task)
    }

    async fn event(rx: &mut mpsc::Receiver<ServerFrame>) -> DesignEvent {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(8), rx.recv())
                .await
                .expect("an event in time")
                .expect("the stream is open");
            if let ServerFrame::Design { event, .. } = frame {
                return event;
            }
        }
    }

    async fn frame_seq(rx: &mut mpsc::Receiver<ServerFrame>) -> u64 {
        loop {
            if let DesignEvent::Frame { seq, .. } = event(rx).await {
                return seq;
            }
        }
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..200 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn a_relaunched_browser_advances_the_open_stream_sequence() {
        let f = Fixture::new().await;
        let (mut rx, task) = subscribe(&f, 1);
        until("initial feed", || {
            f.factory.browser(0).calls(|c| c.screencasts_started == 1)
        })
        .await;
        f.factory.browser(0).frame(10);
        assert_eq!(frame_seq(&mut rx).await, 10);
        f.factory.browser(0).crash();
        until("replacement feed", || {
            f.factory.launched() == 2 && f.factory.browser(1).calls(|c| c.screencasts_started == 1)
        })
        .await;
        f.factory.browser(1).frame(1);
        assert_eq!(frame_seq(&mut rx).await, 11);
        task.abort();
    }

    #[tokio::test]
    async fn containment_ends_the_attempt_without_a_crash_relaunch() {
        let f = Fixture::new().await;
        let (mut rx, task) = subscribe(&f, 1);
        until("initial feed", || {
            f.factory.browser(0).calls(|c| c.screencasts_started == 1)
        })
        .await;
        f.factory.browser(0).stop_for_containment();
        let reason = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let DesignEvent::Ended { reason } = event(&mut rx).await {
                    break reason;
                }
            }
        })
        .await
        .expect("containment promptly closes the stream");
        assert!(reason.contains("containment"));
        task.await.unwrap();
        let row =
            f.h.ctx
                .store
                .lock()
                .await
                .design_session(&f.session)
                .unwrap()
                .unwrap();
        assert_eq!(row.end_reason.as_deref(), Some(reason.as_str()));
        let retry = f
            .as_agent(Request::DesignPreview {
                url: crate::common::design::PREVIEW.into(),
                caller: Some(f.agent.clone()),
            })
            .await
            .expect_err("a stopped attempt cannot reopen its preview");
        assert!(retry.to_string().contains("containment"));
        assert_eq!(f.factory.launched(), 1);
    }

    #[tokio::test]
    async fn late_subscribers_get_the_current_frame_and_cleanup_survives_an_abort() {
        let f = Fixture::new().await;
        let service = f.h.ctx.design.clone();
        let (mut first, first_task) = subscribe(&f, 1);
        until("the screencast", || {
            f.factory.browser(0).calls(|c| c.screencasts_started) >= 1
        })
        .await;
        until("a counted subscriber", || {
            service.subscriber_count(&f.session) == 1
        })
        .await;
        f.factory.browser(0).frame(1);
        assert_eq!(frame_seq(&mut first).await, 1);

        // A second tab on a static page gets the frame at once.
        let (mut second, second_task) = subscribe(&f, 2);
        let started = Instant::now();
        assert_eq!(frame_seq(&mut second).await, 1);
        assert!(started.elapsed() < Duration::from_secs(1));
        until("two subscribers", || {
            service.subscriber_count(&f.session) == 2
        })
        .await;

        // The device socket ends: the stream task is aborted, and the
        // guard still gives both subscribers back.
        first_task.abort();
        second_task.abort();
        let _ = first_task.await;
        let _ = second_task.await;
        until("no subscribers", || {
            service.subscriber_count(&f.session) == 0
        })
        .await;

        // A reconnect gets the frame again.
        let (mut third, third_task) = subscribe(&f, 3);
        assert_eq!(frame_seq(&mut third).await, 1);
        assert_eq!(service.subscriber_count(&f.session), 1);
        third_task.abort();
        let _ = third_task.await;
        until("no subscribers after the reconnect", || {
            service.subscriber_count(&f.session) == 0
        })
        .await;
    }

    #[tokio::test]
    async fn without_an_ack_the_latest_frame_wins_after_the_wait() {
        let f = Fixture::new().await;
        let (mut rx, task) = subscribe(&f, 1);
        until("the screencast", || {
            f.factory.browser(0).calls(|c| c.screencasts_started) >= 1
        })
        .await;
        f.factory.browser(0).frame(1);
        assert_eq!(frame_seq(&mut rx).await, 1);
        let sent = Instant::now();
        f.factory.browser(0).frame(2);
        f.factory.browser(0).frame(3);
        // Frame 1 was never acked: the next one waits, then the latest goes.
        assert_eq!(frame_seq(&mut rx).await, 3);
        assert!(sent.elapsed() >= Duration::from_millis(1500));
        task.abort();
    }

    #[tokio::test]
    async fn an_ended_event_ends_the_stream() {
        let f = Fixture::new().await;
        let (mut rx, task) = subscribe(&f, 1);
        let _ = event(&mut rx).await;
        f.end(StageEnd::Cancelled).await;
        loop {
            if matches!(event(&mut rx).await, DesignEvent::Ended { .. }) {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the stream task ends")
            .expect("no panic");
        assert_eq!(f.h.ctx.design.subscriber_count(&f.session), 0);
    }

    /// With one slot, a second watched session waits for it and starts once
    /// the first session ends and frees it.
    #[tokio::test]
    async fn a_session_queued_for_a_slot_starts_when_one_frees() {
        let mut config = design_config();
        config.design.max_sessions = 1;
        let f = Fixture::with(config).await;
        assert_eq!(f.factory.launched(), 1, "the first session holds the slot");

        let item = crate::common::design::enqueue_live(&f.h, "design the cancel button").await;
        f.h.dispatch().await;
        let run = f.h.run(&item.id).await;
        let stage =
            f.h.running()
                .await
                .into_iter()
                .find(|s| s.run == run.id)
                .expect("the second item's stage runs");
        let agent = stage.aether_agent.clone().expect("an agent");
        let worktree = crate::common::design::add_worktree(&f.h.repo, f.h.dir.path(), "design-2");
        let second =
            f.h.ctx
                .design
                .stage_started(
                    &f.h.ctx,
                    &item.id,
                    &run.id,
                    &stage.stage_id,
                    stage.attempt,
                    &agent,
                    worktree,
                )
                .await
                .expect("second session starts");
        f.as_agent(Request::DesignPreview {
            url: crate::common::design::PREVIEW.into(),
            caller: Some(agent.clone()),
        })
        .await
        .expect("preview reported");

        let (tx, _rx) = mpsc::channel(64);
        let sub = DesignSubscription {
            id: 7,
            request: 7,
            session: second.clone(),
            device: WelcomeDevice {
                id: "device-1".into(),
                name: "phone".into(),
                role: archon_core::protocol::DeviceRole::Operator,
            },
        };
        let task = {
            let ctx = f.h.ctx.clone();
            tokio::spawn(async move {
                let (_acks, receiver) = tokio::sync::watch::channel(0);
                archon_daemon::design::stream(ctx, sub, receiver, tx).await;
            })
        };
        let service = f.h.ctx.design.clone();
        until("the second subscriber", || {
            service.subscriber_count(&second) == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(f.factory.launched(), 1, "no slot: the second session waits");

        f.end(StageEnd::Cancelled).await;
        let factory = f.factory.clone();
        until("the queued session's browser", || factory.launched() == 2).await;
        task.abort();
    }

    /// S5: after "Ask for changes" the open screen subscribed to the next
    /// session while its app was still starting; the preview report then
    /// started no browser, and the screen waited on "Waiting for the first
    /// picture" until it was reopened. The report starts it for a watcher.
    #[tokio::test]
    async fn a_session_watched_before_its_app_is_up_starts_its_preview_on_the_report() {
        let f = Fixture::new().await;
        let item = crate::common::design::enqueue_live(&f.h, "design the cancel button").await;
        f.h.dispatch().await;
        let run = f.h.run(&item.id).await;
        let stage =
            f.h.running()
                .await
                .into_iter()
                .find(|s| s.run == run.id)
                .expect("the second item's stage runs");
        let agent = stage.aether_agent.clone().expect("an agent");
        let worktree = crate::common::design::add_worktree(&f.h.repo, f.h.dir.path(), "design-2");
        let second =
            f.h.ctx
                .design
                .stage_started(
                    &f.h.ctx,
                    &item.id,
                    &run.id,
                    &stage.stage_id,
                    stage.attempt,
                    &agent,
                    worktree,
                )
                .await
                .expect("second session starts");
        let (tx, mut rx) = mpsc::channel(64);
        let sub = DesignSubscription {
            id: 7,
            request: 7,
            session: second.clone(),
            device: WelcomeDevice {
                id: "device-1".into(),
                name: "phone".into(),
                role: archon_core::protocol::DeviceRole::Operator,
            },
        };
        let task = {
            let ctx = f.h.ctx.clone();
            tokio::spawn(async move {
                let (_acks, receiver) = tokio::sync::watch::channel(0);
                archon_daemon::design::stream(ctx, sub, receiver, tx).await;
            })
        };
        let service = f.h.ctx.design.clone();
        until("the watcher", || service.subscriber_count(&second) == 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            f.factory.launched(),
            1,
            "nothing to show before the app is up"
        );
        f.as_agent(Request::DesignPreview {
            url: crate::common::design::PREVIEW.into(),
            caller: Some(agent.clone()),
        })
        .await
        .expect("preview reported");
        let factory = f.factory.clone();
        until("the watched preview's browser", || factory.launched() == 2).await;
        until("its frames", || {
            factory.browser(1).calls(|c| c.screencasts_started) >= 1
        })
        .await;
        f.factory.browser(1).frame(1);
        assert_eq!(
            frame_seq(&mut rx).await,
            1,
            "the open screen gets the picture"
        );
        task.abort();
    }

    /// Confirming a design parks its browser: the session stays Confirmed for
    /// its gate, the browser closes, its slot goes to a queued session, and
    /// a later look does not start it again.
    #[tokio::test]
    async fn a_confirmed_design_parks_its_browser_and_hands_on_its_slot() {
        use archon_core::protocol::Reply;
        let mut config = design_config();
        config.design.max_sessions = 1;
        let f = Fixture::with(config).await;
        let (id, _) = crate::design_undo::done_note(&f, "k1", "a.txt", "one\n").await;
        // The operator watches the design they will confirm.
        let (mut first_rx, first_task) = subscribe(&f, 9);

        let item = crate::common::design::enqueue_live(&f.h, "design the cancel button").await;
        f.h.dispatch().await;
        let run = f.h.run(&item.id).await;
        let stage =
            f.h.running()
                .await
                .into_iter()
                .find(|s| s.run == run.id)
                .expect("the second item's stage runs");
        let agent = stage.aether_agent.clone().expect("an agent");
        let worktree = crate::common::design::add_worktree(&f.h.repo, f.h.dir.path(), "design-2");
        let second =
            f.h.ctx
                .design
                .stage_started(
                    &f.h.ctx,
                    &item.id,
                    &run.id,
                    &stage.stage_id,
                    stage.attempt,
                    &agent,
                    worktree,
                )
                .await
                .expect("second session starts");
        f.as_agent(Request::DesignPreview {
            url: crate::common::design::PREVIEW.into(),
            caller: Some(agent.clone()),
        })
        .await
        .expect("preview reported");
        let (tx, _rx) = mpsc::channel(64);
        let sub = DesignSubscription {
            id: 8,
            request: 8,
            session: second.clone(),
            device: WelcomeDevice {
                id: "device-1".into(),
                name: "phone".into(),
                role: archon_core::protocol::DeviceRole::Operator,
            },
        };
        let task = {
            let ctx = f.h.ctx.clone();
            tokio::spawn(async move {
                let (_acks, receiver) = tokio::sync::watch::channel(0);
                archon_daemon::design::stream(ctx, sub, receiver, tx).await;
            })
        };
        let service = f.h.ctx.design.clone();
        until("the second subscriber", || {
            service.subscriber_count(&second) == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(f.factory.launched(), 1, "no slot: the second session waits");

        let view = match f
            .operator(Request::DesignFinishCapture {
                session: f.session.clone(),
                cancel_queued: false,
                recapture: false,
            })
            .await
            .expect("capture")
        {
            Reply::DesignFinish(view) => view,
            other => panic!("{other:?}"),
        };
        f.operator(Request::DesignFinishConfirm {
            session: f.session.clone(),
            capture: view.capture.clone(),
            include: [(id, true)].into(),
            superseded: Default::default(),
            confirm_no_change: false,
        })
        .await
        .expect("confirmed");

        let factory = f.factory.clone();
        until("the queued session's browser", || factory.launched() == 2).await;
        assert!(
            f.factory.browser(0).calls(|c| c.closed),
            "the confirmed design's browser is closed"
        );
        assert_eq!(
            state(&f).await.session.unwrap().state,
            SessionState::Confirmed,
            "the session waits for its gate"
        );
        // A later look at the confirmed design does not start its browser.
        let _ = f
            .operator(Request::DesignReload {
                session: f.session.clone(),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(f.factory.launched(), 2, "no browser for a confirmed design");

        // The confirmed design's stream still hears its state and its end.
        loop {
            if let DesignEvent::Session {
                state: SessionState::Confirmed,
                ..
            } = event(&mut first_rx).await
            {
                break;
            }
        }
        f.end(StageEnd::Cancelled).await;
        loop {
            if matches!(event(&mut first_rx).await, DesignEvent::Ended { .. }) {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(5), first_task)
            .await
            .expect("the confirmed design's stream ends")
            .expect("no panic");
        task.abort();
    }
}

/// The agent's first preview report makes the session live and posts the
/// "Design session ready" notice for the item, once.
#[tokio::test]
async fn the_first_preview_report_posts_a_ready_notice_once() {
    let f = Fixture::new().await;
    f.as_agent(Request::DesignPreview {
        url: crate::common::design::PREVIEW.into(),
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("a second report");
    let feed =
        f.h.ctx
            .store
            .lock()
            .await
            .feed_after(0, 10_000)
            .expect("feed");
    let ready: Vec<_> = feed
        .into_iter()
        .filter_map(|item| match item.entry {
            archon_core::notify::FeedEntry::Notification(n)
                if n.text.starts_with("Design session ready") =>
            {
                Some(n)
            }
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 1, "{ready:?}");
    assert_eq!(ready[0].item.as_ref(), Some(&f.item));
    assert_eq!(
        ready[0].kind,
        archon_core::notify::NotificationKind::Blocker,
        "the operator's action is asked for; it is no decision"
    );
    assert!(ready[0].text.chars().count() < 160, "{}", ready[0].text);
}

/// Startup recovery ends a live session whose run is already over, so no
/// browser or slot is kept for a dead item.
#[tokio::test]
async fn recovery_ends_sessions_of_finished_runs() {
    let f = Fixture::new().await;
    f.operator(Request::CancelItem {
        item: f.item.clone(),
        reason: None,
    })
    .await
    .expect("cancel");
    archon_daemon::design::recover(&f.h.ctx)
        .await
        .expect("recovery");
    let row =
        f.h.ctx
            .store
            .lock()
            .await
            .design_session(&f.session)
            .expect("read")
            .expect("row");
    assert_eq!(row.state, SessionState::Ended);
    assert!(f.factory.browser(0).calls(|c| c.closed));
}

/// An edit waits for the delivery pump's lock, so the agent and the store
/// never disagree about a note's words.
#[tokio::test]
async fn a_queued_note_edit_takes_the_item_lock() {
    let f = Fixture::new().await;
    let n = add(&f, "k-edit", "make it blue").await;
    let held = f.h.ctx.design.item_lock_for_tests(&f.item);
    let _guard = held.lock().await;
    let edit = f.operator(Request::DesignNoteEdit {
        item: f.item.clone(),
        id: n.id.clone(),
        text: "make it green".into(),
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), edit)
            .await
            .is_err(),
        "the edit ran while the item lock was held"
    );
}

/// A page note with no page of its own, when nothing took the preview
/// elsewhere, is pictured as the preview shows it, never reloaded at a bare
/// path that would drop its query or fragment (`/orders?id=42`, `/#/settings`).
#[tokio::test]
async fn a_page_note_alone_is_pictured_as_shown_without_moving_the_preview() {
    let f = Fixture::new().await;
    add(&f, "k1", "Make the header bigger").await;
    let commit = f.commit(
        "header.css",
        "h1 { font-size: 2rem }\n",
        "Make the header bigger\n\nArchon-Note: D1\n",
    );
    f.as_agent(Request::DesignNoteReport {
        id: "D1".into(),
        status: AgentNoteStatus::Done,
        reply: Some("Bigger header".into()),
        commit: Some(commit),
        no_change: false,
        path: None,
        steps: Vec::new(),
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("done");
    let browser = f.factory.browser(0);
    let before = browser.calls(|c| c.navigated.len());
    let (view, _) = confirm_with(&f, &[], false).await;
    let card = view.cards.iter().find(|c| c.note == "D1").unwrap();
    assert!(card.data_url.is_some(), "a final picture: {card:?}");
    let navigated = browser.calls(|c| c.navigated[before..].to_vec());
    assert!(
        navigated.is_empty(),
        "the preview stays as shown: {navigated:?}"
    );
}

/// Item A: a New screen note's change lives on a page of its own. Its final
/// picture showed `/` (whatever the preview showed at Finish). The agent now
/// names that page with `path` when it reports done: the preview goes there,
/// the after picture records it, and Finish pictures the note there, whole.
#[tokio::test]
async fn a_page_note_done_with_a_path_is_pictured_on_that_page() {
    let f = Fixture::new().await;
    let added = add(&f, "k1", "Add a settings screen like my sketch").await;
    assert!(added.target.is_none(), "a page note");
    let report_with = |status, commit: Option<String>, path: &str| {
        f.as_agent(Request::DesignNoteReport {
            id: "D1".into(),
            status,
            reply: Some("Added /settings/new".into()),
            commit,
            no_change: false,
            path: Some(path.into()),
            steps: Vec::new(),
            caller: Some(f.agent.clone()),
        })
    };
    let early = report_with(AgentNoteStatus::Working, None, "/settings/new").await;
    assert!(early.is_err(), "a path goes with done");
    let unchanged = f
        .as_agent(Request::DesignNoteReport {
            id: "D1".into(),
            status: AgentNoteStatus::Done,
            reply: Some("Nothing to change".into()),
            commit: None,
            no_change: true,
            path: Some("/settings/new".into()),
            steps: Vec::new(),
            caller: Some(f.agent.clone()),
        })
        .await;
    assert!(
        unchanged.is_err(),
        "a path names where a change shows, so never with no change"
    );
    let commit = f.commit(
        "settings.html",
        "<h1>Settings</h1>\n",
        "Add a settings screen\n\nArchon-Note: D1\n",
    );
    let bad = report_with(AgentNoteStatus::Done, Some(commit.clone()), "settings").await;
    assert!(bad.is_err(), "a path starts with /");
    report_with(AgentNoteStatus::Done, Some(commit), "/settings/new")
        .await
        .expect("done");
    let page = format!("{}settings/new", crate::common::design::PREVIEW);
    let browser = f.factory.browser(0);
    assert_eq!(
        browser.calls(|c| c.navigated.last().cloned()).as_deref(),
        Some(page.as_str()),
        "the operator's preview shows the new screen"
    );
    let done = note(&f, "D1").await;
    let after = done.after_asset.clone().expect("an after picture");
    let shown =
        f.h.ctx
            .store
            .lock()
            .await
            .design_asset(&f.item, &after)
            .unwrap()
            .unwrap()
            .target
            .expect("the after picture records its page");
    assert_eq!(shown.path, "/settings/new");
    // A second page note, done with no page of its own.
    add(&f, "k2", "Make the header bigger").await;
    let commit = f.commit(
        "header.css",
        "h1 { font-size: 2rem }\n",
        "Make the header bigger\n\nArchon-Note: D2\n",
    );
    f.as_agent(Request::DesignNoteReport {
        id: "D2".into(),
        status: AgentNoteStatus::Done,
        reply: Some("Bigger header".into()),
        commit: Some(commit),
        no_change: false,
        path: None,
        steps: Vec::new(),
        caller: Some(f.agent.clone()),
    })
    .await
    .expect("D2 done");
    // The operator wanders back to the home page before Finish (the stand-in
    // browser shows `/`).
    let navigated_before = browser.calls(|c| c.navigated.len());
    let (view, _) = confirm_with(&f, &[], false).await;
    let card = view.cards.iter().find(|c| c.note == "D1").unwrap();
    assert!(card.data_url.is_some(), "a final picture: {card:?}");
    let navigated = browser.calls(|c| c.navigated[navigated_before..].to_vec());
    let at = navigated
        .iter()
        .position(|u| u == &page)
        .unwrap_or_else(|| panic!("D1's final picture is taken on its page: {navigated:?}"));
    let home = crate::common::design::PREVIEW.to_owned();
    assert!(
        navigated[at + 1..].contains(&home),
        "D2, with no page of its own, is pictured on the page shown at Finish, \
         not on D1's: {navigated:?}"
    );
    assert_eq!(
        navigated.last(),
        Some(&home),
        "the preview goes back to the page the operator left it on"
    );
    let last_capture = browser.calls(|c| c.captures.last().cloned()).unwrap();
    assert_eq!(last_capture.0, None, "the whole view, not a crop");
}
