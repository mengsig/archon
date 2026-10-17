//! Merges the operator asked for (ARCHITECTURE §18.6): one open request per
//! item, a confirmation question recorded with its request, ended ones final.

mod common;

use archon_core::error::ArchonError;
use archon_core::id::{MergeRequestId, QuestionId};
use archon_core::merge::{MERGE_OPTIONS, MergeAuthority, MergeRequest, MergeState};
use archon_core::notify::{FeedEntry, Question};
use archon_core::run::PipelineRun;
use common::*;

fn merge(run: &PipelineRun, repo: &archon_core::repo::Repo, state: MergeState) -> MergeRequest {
    MergeRequest {
        id: MergeRequestId::new(),
        item: run.item.clone(),
        run: run.id.clone(),
        repo: repo.id.clone(),
        pr_url: "https://github.com/o/r/pull/7".into(),
        head: "0123456789abcdef".into(),
        authority: MergeAuthority::Cli,
        ignore_checks: false,
        reason: None,
        waived_by: None,
        waited: false,
        checks: None,
        state,
        requested_at: t(30),
        updated_at: t(30),
    }
}

fn confirmation(run: &PipelineRun) -> Question {
    Question {
        id: QuestionId::new(),
        number: 0,
        text: "Merge it?".into(),
        options: MERGE_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
        item: Some(run.item.clone()),
        aether: None,
        asked_by: None,
        asked_at: t(30),
        answer: None,
        answered_at: None,
    }
}

#[test]
fn one_request_per_item_stays_open_and_an_ended_one_is_final() {
    let (mut store, repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let mut first = merge(&run, &repo, MergeState::Merging);
    store.insert_merge(&first).unwrap();
    // Later, so the record's order never rests on ids made in one millisecond.
    let second = MergeRequest {
        requested_at: t(32),
        updated_at: t(32),
        ..merge(&run, &repo, MergeState::Merging)
    };
    let refused = store.insert_merge(&second).unwrap_err();
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused}"
    );
    assert_eq!(store.open_merges().unwrap(), vec![first.clone()]);

    first.state = MergeState::Queued;
    store.update_merge(&first).unwrap();
    assert_eq!(store.open_merges().unwrap(), vec![first.clone()]);
    assert!(store.insert_merge(&second).is_err());

    first.state = serde_json::from_value(serde_json::json!({
        "state": "merged", "method": "squash"
    }))
    .unwrap();
    first.updated_at = t(31);
    store.update_merge(&first).unwrap();
    assert!(store.open_merges().unwrap().is_empty());
    first.state = MergeState::Declined;
    let refused = store.update_merge(&first).unwrap_err();
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused}"
    );
    store.insert_merge(&second).unwrap();
    let all = store.merges_for_item(&run.item).unwrap();
    assert_eq!(all.len(), 2, "the ended request stays on record");
    assert_eq!(all[0].state.kind(), "merged");
}

#[test]
fn a_confirmation_question_is_recorded_with_its_request() {
    let (mut store, repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let question = confirmation(&run);
    let waiting = merge(&run, &repo, MergeState::AwaitingOperator);
    let (merge, feed_item) = store.ask_merge(&question, waiting).unwrap();
    let number = merge.waiting_on().expect("it waits on the question");
    assert!(number >= 1, "the store numbers the question");
    let FeedEntry::Question(asked) = &feed_item.entry else {
        panic!("the question is posted: {feed_item:?}");
    };
    assert_eq!(
        (asked.id.clone(), asked.number),
        (question.id.clone(), number)
    );
    assert_eq!(store.merge_by_question(&question.id).unwrap(), Some(merge));
    assert!(
        store
            .merge_by_question(&QuestionId::new())
            .unwrap()
            .is_none()
    );

    let another = confirmation(&run);
    let refused = store
        .ask_merge(
            &another,
            self::merge(&run, &repo, MergeState::AwaitingOperator),
        )
        .unwrap_err();
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused}"
    );
    let open: Vec<_> = store.questions(true).unwrap();
    assert!(
        open.iter().all(|q| q.id != another.id),
        "a refused request leaves no question behind"
    );
}

#[test]
fn deciding_a_merge_records_its_answer_and_feed_atomically() {
    let (mut store, repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let question = confirmation(&run);
    let (mut request, _) = store
        .ask_merge(&question, merge(&run, &repo, MergeState::AwaitingOperator))
        .unwrap();
    request.state = MergeState::Merging;
    request.updated_at = t(31);
    let via = archon_core::notify::AnswerVia::ChatCommand;
    let answers = store
        .decide_merge(&request, "yes ignore-checks", via)
        .unwrap();
    assert_eq!(answers.len(), 1);
    assert!(
        matches!(&answers[0].entry, FeedEntry::Answer { question: id, text, via: source }
        if id == &question.id && text == "yes ignore-checks" && *source == via)
    );
    assert_eq!(
        store.question(&question.id).unwrap().answer.as_deref(),
        Some("yes ignore-checks")
    );
    assert_eq!(
        store.merge_by_question(&question.id).unwrap(),
        Some(request.clone())
    );
    let seq = store.last_feed_seq().unwrap();
    assert!(store.decide_merge(&request, "no", via).is_err());
    request.state = MergeState::Refused {
        reason: "head moved".into(),
    };
    store.update_merge(&request).unwrap();
    assert_eq!(
        store.last_feed_seq().unwrap(),
        seq,
        "the recorded answer stays unchanged"
    );
    assert_eq!(
        store.question(&question.id).unwrap().answer.as_deref(),
        Some("yes ignore-checks")
    );
}

#[test]
fn a_failed_decision_rolls_back_the_answer_and_feed() {
    let (mut store, repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let question = confirmation(&run);
    let (waiting, _) = store
        .ask_merge(&question, merge(&run, &repo, MergeState::AwaitingOperator))
        .unwrap();
    let mut decision = waiting.clone();
    decision.state = MergeState::Declined;
    decision.updated_at = t(29); // Violates the merge table's timestamp constraint.
    let seq = store.last_feed_seq().unwrap();
    assert!(
        store
            .decide_merge(&decision, "no", Default::default())
            .is_err()
    );
    assert!(store.question(&question.id).unwrap().is_open());
    assert_eq!(store.last_feed_seq().unwrap(), seq);
    assert_eq!(
        store.merge_by_question(&question.id).unwrap(),
        Some(waiting)
    );
    decision.updated_at = t(31);
    assert!(
        store
            .decide_merge(&decision, " ", Default::default())
            .is_err()
    );
    assert!(store.question(&question.id).unwrap().is_open());
}

#[test]
fn settlement_closes_a_confirmation_with_its_reason() {
    let (mut store, repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let question = confirmation(&run);
    let (mut request, _) = store
        .ask_merge(&question, merge(&run, &repo, MergeState::AwaitingOperator))
        .unwrap();
    request.state = MergeState::Refused {
        reason: "the reviewed head moved".into(),
    };
    request.updated_at = t(31);
    store.update_merge(&request).unwrap();
    let answered = store.question(&question.id).unwrap();
    assert!(answered.answer.unwrap().contains("the reviewed head moved"));
    assert_eq!(answered.answered_at, Some(t(31)));
    assert!(matches!(
        store.feed_after(1, 10).unwrap()[0].entry,
        FeedEntry::Answer { .. }
    ));
}

#[test]
fn legacy_questions_are_reconciled_idempotently_and_closure_failures_roll_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut store = archon_queue::Store::open(&path).unwrap();
    let repo = repo("legacy");
    store.insert_repo(&repo).unwrap();
    let item = store
        .insert_item(&item(Some(&repo), "legacy"), None)
        .unwrap();
    let run = start(&mut store, &item.id);
    let question = confirmation(&run);
    let (waiting, _) = store
        .ask_merge(&question, merge(&run, &repo, MergeState::AwaitingOperator))
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_answer BEFORE INSERT ON feed
        BEGIN SELECT RAISE(ABORT, 'answer refused'); END;",
    )
    .unwrap();
    let mut settled = waiting.clone();
    settled.state = MergeState::Declined;
    settled.updated_at = t(31);
    assert!(store.update_merge(&settled).is_err());
    assert_eq!(
        store.merge_by_question(&question.id).unwrap(),
        Some(waiting)
    );
    assert!(store.question(&question.id).unwrap().is_open());
    conn.execute(
        "UPDATE merge_requests SET state = 'declined', updated_at = ?1, data = ?2",
        rusqlite::params![
            t(31).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            serde_json::to_string(&settled).unwrap()
        ],
    )
    .unwrap();
    assert!(store.reconcile_merge_questions().is_err());
    assert!(store.question(&question.id).unwrap().is_open());
    conn.execute_batch("DROP TRIGGER fail_answer").unwrap();
    store.reconcile_merge_questions().unwrap();
    assert!(!store.question(&question.id).unwrap().is_open());
    let seq = store.last_feed_seq().unwrap();
    store.reconcile_merge_questions().unwrap();
    assert_eq!(store.last_feed_seq().unwrap(), seq);
}

#[test]
fn recorded_answers_survive_legacy_decisions_and_reconciliation_keeps_waiting_questions() {
    let (mut store, repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let question = confirmation(&run);
    let (mut request, _) = store
        .ask_merge(&question, merge(&run, &repo, MergeState::AwaitingOperator))
        .unwrap();
    store.reconcile_merge_questions().unwrap();
    assert!(store.question(&question.id).unwrap().is_open());
    let answered = store.answer_question(&question.id, "no", t(31)).unwrap();
    request.state = MergeState::Declined;
    request.updated_at = t(32);
    let seq = store.last_feed_seq().unwrap();
    let posted = store
        .decide_merge(&request, "no", Default::default())
        .unwrap();
    assert!(posted.is_empty(), "an existing answer is not posted twice");
    assert_eq!(store.question(&question.id).unwrap(), answered);
    store.reconcile_merge_questions().unwrap();
    assert_eq!(store.last_feed_seq().unwrap(), seq);
}
