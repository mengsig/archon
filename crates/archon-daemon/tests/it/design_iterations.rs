//! Design iterations (protocol 26): follow-ups as chains of versions,
//! references to earlier notes, continuing a chain from an older version,
//! and the Finish review's one card per chain.

use crate::common::design::Fixture;
use crate::design_session::{report, state};
use archon_core::design::{DesignNote, FailReason, NoteStatus};
use archon_core::error::ArchonError;
use archon_core::protocol::{AgentNoteStatus, DesignNoteView, Reply, Request};

async fn add_follow(
    f: &Fixture,
    key: &str,
    text: &str,
    follows: Option<&str>,
    refs: &[&str],
) -> Result<DesignNote, ArchonError> {
    match f
        .operator(Request::DesignNoteAdd {
            session: f.session.clone(),
            client_key: key.into(),
            text: text.into(),
            asset: None,
            send_picture: false,
            details_text: None,
            follows: follows.map(str::to_owned),
            refs: refs.iter().map(|r| (*r).to_owned()).collect(),
        })
        .await?
    {
        Reply::DesignNote(view) => Ok(view.note),
        other => panic!("{other:?}"),
    }
}

/// A note (or follow-up) done with a real commit of `file`.
async fn done(f: &Fixture, key: &str, file: &str, body: &str, follows: Option<&str>) -> String {
    let added = add_follow(f, key, &format!("change {file}"), follows, &[])
        .await
        .expect("added");
    let commit = f.commit(
        file,
        body,
        &format!("change {file}\n\nArchon-Note: {}\n", added.id),
    );
    report(f, &added.id, AgentNoteStatus::Working, None, None)
        .await
        .unwrap();
    report(f, &added.id, AgentNoteStatus::Done, Some(commit), None)
        .await
        .unwrap();
    added.id
}

async fn view(f: &Fixture, id: &str) -> DesignNoteView {
    state(f)
        .await
        .notes
        .into_iter()
        .find(|n| n.note.id == id)
        .unwrap_or_else(|| panic!("no note {id}"))
}

fn message_for(f: &Fixture, id: &str) -> String {
    f.h.backend
        .messages()
        .into_iter()
        .map(|(_, text)| text)
        .rfind(|t| t.starts_with(&format!("Design note {id} ")))
        .unwrap_or_else(|| panic!("no message for {id}"))
}

/// Wait until every revert note of `originals` settled (done or failed).
async fn settled(f: &Fixture, originals: &[&str]) {
    for _ in 0..200 {
        let notes = state(f).await.notes;
        let all = originals.iter().all(|o| {
            notes.iter().any(|n| {
                n.note.reverts.as_deref() == Some(*o)
                    && matches!(
                        n.note.status,
                        NoteStatus::Done | NoteStatus::Failed | NoteStatus::Cancelled
                    )
            })
        });
        if all {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the reverts of {originals:?} never settled");
}

/// R1: a follow-up is a new note linked to its version; the chain shows in
/// every view and the agent's message names the earlier versions.
#[tokio::test]
async fn a_follow_up_is_a_new_version_of_its_chain() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "a.txt", "one\n", None).await;
    let second = done(&f, "k2", "a.txt", "two\n", Some(&first)).await;
    assert_ne!(first, second);
    let v2 = view(&f, &second).await;
    assert_eq!(v2.note.follows.as_deref(), Some(first.as_str()));
    assert_eq!(v2.chain.as_deref(), Some(first.as_str()));
    assert_eq!((v2.version, v2.versions), (2, 2));
    let v1 = view(&f, &first).await;
    assert_eq!(v1.chain.as_deref(), Some(first.as_str()));
    assert_eq!((v1.version, v1.versions), (1, 2));
    let message = message_for(&f, &second);
    assert!(
        message.contains(&format!(
            "this note continues {first} (version 2 of chain {first})"
        )),
        "{message}"
    );
    assert!(
        message.contains(&format!("  - {first}: \"change a.txt\" (commit ")),
        "{message}"
    );
    // The agent reads the same chain through design_history.
    let history = match f
        .as_agent(Request::DesignHistory {
            id: Some(second.clone()),
            caller: Some(f.agent.clone()),
        })
        .await
        .expect("history")
    {
        Reply::DesignAgent { message } => message,
        other => panic!("{other:?}"),
    };
    assert!(
        history.contains(&format!("Chain {first} (2 versions)")),
        "{history}"
    );
    assert!(
        history.contains(&format!("{second} (follows {first})")),
        "{history}"
    );
}

/// R2 and R8: references are checked, kept, and described to the agent.
#[tokio::test]
async fn references_reach_the_agent_and_are_limited() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "a.txt", "one\n", None).await;
    let referencing = add_follow(&f, "k2", "like the first", None, &[&first])
        .await
        .expect("added");
    assert_eq!(referencing.refs, vec![first.clone()]);
    let message = message_for(&f, &referencing.id);
    assert!(message.contains("Referenced changes"), "{message}");
    assert!(
        message.contains(&format!("  - {first}: \"change a.txt\"")),
        "{message}"
    );
    let unknown = add_follow(&f, "k3", "x", None, &["D99"]).await.unwrap_err();
    assert!(unknown.to_string().contains("no note D99"), "{unknown}");
    let six: Vec<String> = (0..6).map(|n| format!("D{}", n + 1)).collect();
    let refs: Vec<&str> = six.iter().map(String::as_str).collect();
    // Unknown ids are refused before the count; with known ones the count
    // is what refuses.
    assert!(add_follow(&f, "k4", "x", None, &refs).await.is_err());
}

/// A follow-up on a version with newer versions in the code is refused:
/// continuing from it sets them aside first.
#[tokio::test]
async fn a_follow_up_on_an_older_version_needs_a_continue() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "a.txt", "one\n", None).await;
    done(&f, "k2", "b.txt", "two\n", Some(&first)).await;
    let refused = add_follow(&f, "k3", "again", Some(&first), &[])
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("continue from"), "{refused}");
}

/// R5: continuing from an older version reverts only the chain's newer
/// versions, newest first, with new commits (no reset), sets them aside and
/// queues the follow-up on the chosen version.
#[tokio::test]
async fn continuing_from_a_version_sets_the_newer_ones_aside() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "a.txt", "one\n", None).await;
    let second = done(&f, "k2", "b.txt", "two\n", Some(&first)).await;
    let third = done(&f, "k3", "c.txt", "three\n", Some(&second)).await;
    let other = done(&f, "k4", "d.txt", "other\n", None).await;
    let before = f.head();
    let reply = f
        .operator(Request::DesignNoteContinueFrom {
            item: f.item.clone(),
            id: first.clone(),
            text: Some("make it red".into()),
            client_key: Some("c1".into()),
            refs: Vec::new(),
        })
        .await
        .expect("continue queued");
    let Reply::DesignNote(follow) = reply else {
        panic!("{reply:?}")
    };
    assert_eq!(follow.note.follows.as_deref(), Some(first.as_str()));
    settled(&f, &[&third, &second]).await;
    let notes = state(&f).await.notes;
    let revert_of = |id: &str| {
        notes
            .iter()
            .find(|n| n.note.reverts.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no revert of {id}"))
            .note
            .clone()
    };
    let (r3, r2) = (revert_of(&third), revert_of(&second));
    assert_eq!(r3.status, NoteStatus::Done);
    assert_eq!(r2.status, NoteStatus::Done);
    assert!(
        r3.number < r2.number,
        "the newest version is reverted first"
    );
    for id in [&second, &third] {
        assert!(view(&f, id).await.set_aside, "{id} is set aside");
    }
    assert!(!view(&f, &first).await.set_aside);
    assert!(!view(&f, &other).await.set_aside, "other chains stay");
    let wt = &f.worktree.worktree;
    assert!(wt.join("a.txt").exists() && wt.join("d.txt").exists());
    assert!(!wt.join("b.txt").exists() && !wt.join("c.txt").exists());
    let log = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &before, &f.head()])
        .current_dir(wt)
        .status()
        .unwrap();
    assert!(log.success(), "new revert commits on top, never a reset");
    // The follow-up then goes to the agent, building on the chosen version.
    for _ in 0..100 {
        if view(&f, &follow.note.id).await.note.status == NoteStatus::Sent {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let message = message_for(&f, &follow.note.id);
    assert!(message.contains(&format!("continues {first}")), "{message}");
    // The tree has branched: the follow-up and the set-aside version both
    // follow from the chain's first note's line.
    let fv = view(&f, &follow.note.id).await;
    assert_eq!(fv.chain.as_deref(), Some(first.as_str()));
    assert_eq!(fv.versions, 4);
    // A repeat of the same continue changes nothing.
    let again = f
        .operator(Request::DesignNoteContinueFrom {
            item: f.item.clone(),
            id: first.clone(),
            text: Some("make it red".into()),
            client_key: Some("c1".into()),
            refs: Vec::new(),
        })
        .await
        .expect("repeat answered");
    let Reply::DesignNote(same) = again else {
        panic!("{again:?}")
    };
    assert_eq!(same.note.id, follow.note.id);
}

/// R5: a revert that conflicts with another chain's later change fails with
/// the undo message, nothing changes, and the queued follow-up is cancelled.
#[tokio::test]
async fn a_continue_that_conflicts_stops_cleanly() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "b.txt", "base\n", None).await;
    let second = done(&f, "k2", "a.txt", "two\n", Some(&first)).await;
    done(&f, "k3", "a.txt", "three\n", None).await;
    let before = f.head();
    let reply = f
        .operator(Request::DesignNoteContinueFrom {
            item: f.item.clone(),
            id: first.clone(),
            text: Some("smaller".into()),
            client_key: None,
            refs: Vec::new(),
        })
        .await
        .expect("continue queued");
    let Reply::DesignNote(follow) = reply else {
        panic!("{reply:?}")
    };
    settled(&f, &[&second]).await;
    let revert = state(&f)
        .await
        .notes
        .into_iter()
        .find(|n| n.note.reverts.as_deref() == Some(second.as_str()))
        .unwrap()
        .note;
    assert_eq!(revert.status, NoteStatus::Failed);
    assert_eq!(revert.fail_reason, Some(FailReason::UndoConflict));
    assert!(
        revert
            .thread
            .iter()
            .any(|t| t.text.starts_with("Can't undo automatically")),
        "{:?}",
        revert.thread
    );
    assert_eq!(f.head(), before, "no commit was made");
    assert_eq!(
        view(&f, &follow.note.id).await.note.status,
        NoteStatus::Cancelled
    );
    assert!(!view(&f, &second).await.set_aside);
}

/// Continuing needs a done version in the code and something to do.
#[tokio::test]
async fn continue_refuses_what_it_cannot_do() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "a.txt", "one\n", None).await;
    let refused = f
        .operator(Request::DesignNoteContinueFrom {
            item: f.item.clone(),
            id: first.clone(),
            text: None,
            client_key: None,
            refs: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("already the newest"),
        "{refused}"
    );
    let missing = f
        .operator(Request::DesignNoteContinueFrom {
            item: f.item.clone(),
            id: "D99".into(),
            text: None,
            client_key: None,
            refs: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("D99"), "{missing}");
}

/// R6: Finish shows one card per chain, the newest version in the code,
/// with the earlier versions as its history.
#[tokio::test]
async fn finish_shows_one_card_per_chain_with_its_history() {
    let f = Fixture::new().await;
    let first = done(&f, "k1", "a.txt", "one\n", None).await;
    let second = done(&f, "k2", "b.txt", "two\n", Some(&first)).await;
    let alone = done(&f, "k3", "c.txt", "three\n", None).await;
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
    let ids: Vec<&str> = view.cards.iter().map(|c| c.note.as_str()).collect();
    assert_eq!(ids, [second.as_str(), alone.as_str()]);
    let card = &view.cards[0];
    assert_eq!(card.history.len(), 1);
    assert_eq!(card.history[0].note, first);
    assert_eq!(card.history[0].text, "change a.txt");
    assert!(view.cards[1].history.is_empty());
}
