//! The pictures of a note's change, daemon side: the after picture waits for
//! the page to reload, the agent's steps are replayed for both pictures, and
//! two identical pictures are never offered as a comparison. The browser is a
//! fake; nothing here starts Chromium.

use crate::common::design::{Fixture, jpeg};
use crate::design_session::{note, refused, report, state};
use archon_core::design::{DesignNote, NoteStatus, UiStep};
use archon_core::error::{ArchonError, DesignRefusal};
use archon_core::protocol::{AgentNoteStatus, DesignNoteView, Reply, Request};

/// A note about the element under (20, 20): its crop is the before picture.
async fn pick(f: &Fixture, key: &str, text: &str) -> DesignNote {
    let picked = match f
        .operator(Request::DesignResolve {
            session: f.session.clone(),
            x: 20.0,
            y: 20.0,
            depth: 0,
            select: true,
        })
        .await
        .expect("an element is picked")
    {
        Reply::DesignResolved(view) => view,
        other => panic!("DesignResolve replied {other:?}"),
    };
    let asset = picked.asset.expect("the crop is stored");
    match f
        .operator(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: key.into(),
            text: text.into(),
            asset: Some(asset),
            send_picture: false,
            details_text: None,
        })
        .await
        .expect("note added")
    {
        Reply::DesignNote(view) => view.note,
        other => panic!("DesignNoteAdd replied {other:?}"),
    }
}

async fn view(f: &Fixture, id: &str) -> DesignNoteView {
    state(f)
        .await
        .notes
        .into_iter()
        .find(|n| n.note.id == id)
        .unwrap_or_else(|| panic!("note {id}"))
}

fn click(selector: &str) -> UiStep {
    UiStep::Click {
        selector: selector.into(),
    }
}

/// `done` with `commit`, with `steps`.
async fn done(
    f: &Fixture,
    id: &str,
    commit: &str,
    steps: Vec<UiStep>,
    said: Option<&str>,
) -> Result<Reply, ArchonError> {
    f.as_agent(Request::DesignNoteReport {
        id: id.into(),
        status: AgentNoteStatus::Done,
        reply: said.map(str::to_owned),
        commit: Some(commit.into()),
        no_change: false,
        path: None,
        steps,
        caller: Some(f.agent.clone()),
    })
    .await
}

fn message(reply: Result<Reply, ArchonError>) -> String {
    match reply {
        Ok(Reply::DesignAgent { message }) => message,
        other => panic!("expected a message for the agent, got {other:?}"),
    }
}

fn why(reply: Result<Reply, ArchonError>, code: DesignRefusal) -> String {
    refused(&reply, code);
    match reply {
        Err(ArchonError::Design { message, .. }) => message,
        other => panic!("{other:?}"),
    }
}

async fn asset_steps(f: &Fixture, id: &archon_core::design::DesignAssetId) -> Vec<UiStep> {
    f.h.ctx
        .store
        .lock()
        .await
        .design_asset(&f.item, id)
        .unwrap()
        .unwrap()
        .target
        .map(|t| t.steps.into_vec())
        .unwrap_or_default()
}

fn commit_for(f: &Fixture, n: &str) -> String {
    f.commit(
        &format!("{n}.css"),
        &format!("button {{ color: {n} }}\n"),
        &format!("Change {n}\n\nArchon-Note: {n}\n"),
    )
}

/// The after picture is taken on a page that has reloaded and settled, not on
/// the page as it was before the commit.
#[tokio::test]
async fn the_after_picture_waits_for_the_reload() {
    let f = Fixture::new().await;
    let browser = f.factory.browser(0);
    pick(&f, "k1", "Make the save button green").await;
    let commit = commit_for(&f, "D1");
    browser.captures_return([jpeg(24, 24)]);
    let from = browser.calls(|c| c.ops.len());
    done(&f, "D1", &commit, Vec::new(), None)
        .await
        .expect("D1 done");
    let ops = browser.calls(|c| c.ops[from..].to_vec());
    assert_eq!(
        ops,
        ["reload", "settle", "capture"],
        "reload, wait for it, only then take the picture"
    );
    let done = note(&f, "D1").await;
    assert_eq!(done.status, NoteStatus::Done);
    assert!(done.after_asset.is_some(), "a different picture is kept");
}

/// The agent's steps open the state that shows the change; the before and the
/// after picture are both taken there.
#[tokio::test]
async fn the_steps_are_replayed_for_the_before_and_the_after_picture() {
    let f = Fixture::new().await;
    let browser = f.factory.browser(0);
    let added = pick(&f, "k1", "Restyle the new screen button and its dialog").await;
    let first_before = added.before_asset.clone().expect("a before picture");
    let steps = vec![click("button.new-screen")];

    let from = browser.calls(|c| c.ops.len());
    browser.captures_return([jpeg(30, 30)]);
    report_working(&f, "D1", steps.clone()).await;
    assert_eq!(
        browser.calls(|c| c.ops[from..].to_vec()),
        ["reload", "settle", "input 3", "settle", "capture"],
        "the before picture is taken after the click"
    );
    let working = note(&f, "D1").await;
    let before = working.before_asset.clone().expect("a before picture");
    assert_ne!(before, first_before, "the before picture was retaken");
    assert_eq!(asset_steps(&f, &before).await, steps);

    let commit = commit_for(&f, "D1");
    let from = browser.calls(|c| c.ops.len());
    browser.captures_return([jpeg(40, 40)]);
    done(
        &f,
        "D1",
        &commit,
        steps.clone(),
        Some("The dialog is restyled"),
    )
    .await
    .expect("D1 done");
    assert_eq!(
        browser.calls(|c| c.ops[from..].to_vec()),
        ["reload", "settle", "input 3", "settle", "capture"],
        "the after picture is taken in the same state"
    );
    let (clip, _) = browser
        .calls(|c| c.captures.last().cloned())
        .expect("a capture");
    assert_eq!(clip, None, "the whole view: the dialog is not the button");
    let done = note(&f, "D1").await;
    let after = done.after_asset.expect("an after picture");
    assert_eq!(asset_steps(&f, &after).await, steps);
}

async fn report_working(f: &Fixture, id: &str, steps: Vec<UiStep>) {
    let reply = f
        .as_agent(Request::DesignNoteReport {
            id: id.into(),
            status: AgentNoteStatus::Working,
            reply: None,
            commit: None,
            no_change: false,
            path: None,
            steps,
            caller: Some(f.agent.clone()),
        })
        .await
        .expect("working");
    assert!(matches!(reply, Reply::DesignNote(_)), "{reply:?}");
}

/// An after picture identical to the before one is not kept: the agent is
/// asked to retake it, and when the retake is identical too the operator sees
/// a notice with the agent's explanation instead of two equal pictures.
#[tokio::test]
async fn identical_pictures_ask_for_a_retake_then_show_a_notice() {
    let f = Fixture::new().await;
    pick(&f, "k1", "Make the save button green").await;
    let commit = commit_for(&f, "D1");

    let asked = message(done(&f, "D1", &commit, Vec::new(), None).await);
    assert!(
        asked.contains("identical") && asked.contains("steps"),
        "{asked}"
    );
    let first = view(&f, "D1").await;
    assert_eq!(first.note.status, NoteStatus::Done);
    assert!(first.note.after_asset.is_none(), "nothing was kept");
    assert_eq!(
        archon_core::design::no_visible_change(&first.note),
        None,
        "not yet: the agent may retake"
    );

    let told = message(
        done(
            &f,
            "D1",
            &commit,
            Vec::new(),
            Some("Only the hover colour changed"),
        )
        .await,
    );
    assert!(told.contains("still matches"), "{told}");
    let second = view(&f, "D1").await;
    assert!(second.note.after_asset.is_none());
    let notice =
        archon_core::design::no_visible_change(&second.note).expect("a notice for the operator");
    assert!(
        notice.contains("Only the hover colour changed"),
        "the agent's explanation is shown: {notice}"
    );
}

/// A retake in the state that shows the change keeps its picture, and the
/// notice goes with it.
#[tokio::test]
async fn a_retake_in_the_right_state_keeps_its_picture() {
    let f = Fixture::new().await;
    let browser = f.factory.browser(0);
    pick(&f, "k1", "Restyle the dialog").await;
    let commit = commit_for(&f, "D1");
    message(done(&f, "D1", &commit, Vec::new(), None).await);

    browser.captures_return([jpeg(48, 48)]);
    let steps = vec![click("button.new-screen")];
    done(&f, "D1", &commit, steps.clone(), Some("The dialog"))
        .await
        .expect("the retake is taken");
    let shown = view(&f, "D1").await;
    let after = shown.note.after_asset.clone().expect("an after picture");
    assert_eq!(asset_steps(&f, &after).await, steps);
    assert_eq!(archon_core::design::no_visible_change(&shown.note), None);
}

/// A step that finds nothing is said in words the agent can act on.
#[tokio::test]
async fn a_step_that_finds_nothing_is_reported_to_the_agent() {
    let f = Fixture::new().await;
    let browser = f.factory.browser(0);
    pick(&f, "k1", "Restyle the dialog").await;
    let commit = commit_for(&f, "D1");
    browser.finds_no_elements();
    let told = message(done(&f, "D1", &commit, vec![click("button.gone")], None).await);
    assert!(
        told.contains("step 1") && told.contains("button.gone"),
        "{told}"
    );
    assert!(note(&f, "D1").await.after_asset.is_none());
}

/// The agent may retake its own done note, and is told plainly when it may
/// not: never "is not yours".
#[tokio::test]
async fn a_second_report_on_a_done_note_is_a_retake_or_says_why_not() {
    let f = Fixture::new().await;
    pick(&f, "k1", "Make the save button green").await;
    let commit = commit_for(&f, "D1");
    done(&f, "D1", &commit, Vec::new(), None)
        .await
        .expect("D1 done");

    let working = report(&f, "D1", AgentNoteStatus::Working, None, None).await;
    let said = why(working, DesignRefusal::NotYours);
    assert!(
        said.contains("already done") && !said.contains("is not yours"),
        "{said}"
    );

    let other = f.commit(
        "other.css",
        "a { color: red }\n",
        "Other\n\nArchon-Note: D1\n",
    );
    let said = why(
        done(&f, "D1", &other, Vec::new(), None).await,
        DesignRefusal::NotYours,
    );
    assert!(said.contains(&commit[..7.min(commit.len())]), "{said}");

    let said = why(
        report(&f, "D9", AgentNoteStatus::Done, Some(commit), None).await,
        DesignRefusal::NotYours,
    );
    assert!(said.contains("not a note of this item"), "{said}");
}

/// A note the operator sent again or the agent failed is closed: the refusal
/// names its state.
#[tokio::test]
async fn a_closed_note_cannot_be_retaken() {
    let f = Fixture::new().await;
    pick(&f, "k1", "Make the save button green").await;
    report(&f, "D1", AgentNoteStatus::Failed, None, Some("cannot"))
        .await
        .expect("failed");
    let commit = commit_for(&f, "D1");
    let said = why(
        done(&f, "D1", &commit, Vec::new(), None).await,
        DesignRefusal::NotYours,
    );
    assert!(said.contains("failed") && said.contains("closed"), "{said}");
}

/// The operator's own picture is theirs: the agent's steps retake a page
/// capture, never a sketch the operator drew.
#[tokio::test]
async fn the_operators_sketch_is_not_replaced_by_a_retaken_before_picture() {
    use base64::Engine;
    let f = Fixture::new().await;
    let browser = f.factory.browser(0);
    let uploaded = match f
        .operator(Request::DesignUpload {
            session: f.session.clone(),
            media_type: "image/jpeg".into(),
            data: base64::engine::general_purpose::STANDARD.encode(jpeg(32, 32)),
            name: None,
            sketch: Some("[]".into()),
            base: None,
            blank: false,
        })
        .await
        .expect("sketch uploaded")
    {
        Reply::DesignResolved(view) => view.asset.expect("a stored sketch"),
        other => panic!("{other:?}"),
    };
    let added = match f
        .operator(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: "k1".into(),
            text: "Make it look like this".into(),
            asset: Some(uploaded.clone()),
            send_picture: true,
            details_text: None,
        })
        .await
        .expect("note added")
    {
        Reply::DesignNote(view) => view.note,
        other => panic!("{other:?}"),
    };
    assert_eq!(added.before_asset.as_ref(), Some(&uploaded));

    let from = browser.calls(|c| c.captures.len());
    browser.captures_return([jpeg(30, 30)]);
    report_working(&f, "D1", vec![click("button.new-screen")]).await;
    assert_eq!(
        note(&f, "D1").await.before_asset.as_ref(),
        Some(&uploaded),
        "the sketch is still the before picture"
    );
    assert_eq!(
        browser.calls(|c| c.captures.len()),
        from,
        "no picture was taken over it"
    );
}

/// While the session finishes its pictures are frozen. A retake that is
/// already waiting for the item lock when Finish takes it (and moves the
/// session to Finishing) is refused when it gets the lock: it must not run
/// against the pictures Finish just took.
#[tokio::test]
async fn a_retake_waiting_through_finish_is_refused() {
    let f = Fixture::new().await;
    pick(&f, "k1", "Make the save button green").await;
    let commit = commit_for(&f, "D1");
    message(done(&f, "D1", &commit, Vec::new(), None).await);

    let lock = f.h.ctx.design.item_lock_for_tests(&f.item);
    let guard = lock.lock().await;
    let retake = done(
        &f,
        "D1",
        &commit,
        Vec::new(),
        Some("Only the hover changed"),
    );
    let finish = async {
        // The retake has read the session (Live) and waits for the lock.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        {
            let mut store = f.h.ctx.store.lock().await;
            let current = store.design_session(&f.session).unwrap().unwrap();
            store
                .design_session_transition(
                    &f.session,
                    archon_core::design::SessionState::Live,
                    archon_core::design::SessionState::Finishing,
                    Some(archon_queue::store::FinishRecord {
                        capture: "capture-1".into(),
                        head: commit.clone(),
                        note_version: current.note_version,
                        choices: None,
                    }),
                    false,
                )
                .unwrap();
        }
        drop(guard);
    };
    let (reply, ()) = tokio::join!(retake, finish);
    refused(&reply, DesignRefusal::Finishing);
    assert!(
        note(&f, "D1").await.after_asset.is_none(),
        "the frozen pictures are as they were"
    );
}
