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

    first.state = MergeState::Merged {
        method: "squash".into(),
    };
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
