//! Operator gates, their questions, and directives (ARCHITECTURE §18).

mod common;

use archon_core::autonomy::{Directive, DirectiveSource, MAX_DIRECTIVES};
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, ReportId};
use archon_core::notify::{AnswerVia, FeedEntry, Question};
use archon_core::queue::QueueState;
use archon_core::run::{GateDecision, GateSubject, OperatorGate, PipelineRun, RunState};
use archon_queue::Store;
use common::*;

fn gate(run: &PipelineRun, attempt: u32) -> OperatorGate {
    OperatorGate {
        run: run.id.clone(),
        item: run.item.clone(),
        stage_id: "propose".into(),
        attempt,
        subject: GateSubject::Proposal,
        summary: "use a trait".into(),
        page: Some(".archon/reports/proposal/index.html".into()),
        location: None,
        report: None,
        question: None,
        decision: None,
        withdrawn: None,
        opened_at: t(20 + i64::from(attempt)),
        decided_at: None,
    }
}

#[test]
fn a_gate_opens_once_and_its_decision_is_final() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let first = gate(&run, 1);
    assert_eq!(store.open_gate(&first).unwrap(), first);
    let mut replayed = first.clone();
    replayed.summary = "a replay".into();
    assert_eq!(
        store.open_gate(&replayed).unwrap(),
        first,
        "the stored gate wins"
    );

    let mut asked = first.clone();
    asked.question = Some(QuestionId::new());
    asked.report = Some(ReportId::new());
    store.update_gate(&asked).unwrap();
    let question = asked.question.clone().unwrap();
    assert_eq!(
        store.gate_by_question(&question).unwrap(),
        Some(asked.clone())
    );
    let report = asked.report.clone().unwrap();
    assert_eq!(
        store.open_gate_by_report(&report).unwrap(),
        Some(asked.clone())
    );
    assert_eq!(store.open_gates().unwrap(), vec![asked.clone()]);

    let mut decided = asked.clone();
    decided.decision = Some(GateDecision::Revise {
        feedback: "smaller".into(),
    });
    decided.decided_at = Some(t(30));
    store.update_gate(&decided).unwrap();
    assert!(store.open_gates().unwrap().is_empty());
    assert_eq!(store.open_gate_by_report(&report).unwrap(), None);
    let mut flipped = decided.clone();
    flipped.decision = Some(GateDecision::Approve { note: None });
    let err = store.update_gate(&flipped).unwrap_err();
    assert!(matches!(err, ArchonError::InvalidTransition(_)), "{err}");

    let mut second = gate(&run, 2);
    second.report = Some(report.clone());
    store.open_gate(&second).unwrap();
    assert_eq!(
        store.open_gate_by_report(&report).unwrap(),
        Some(second.clone())
    );
    assert_eq!(
        store.gates_for_run(&run.id).unwrap(),
        vec![decided.clone(), second.clone()]
    );
    assert_eq!(
        store.gates_for_item(&run.item).unwrap(),
        vec![decided, second]
    );
}

#[test]
fn gates_need_a_live_run_and_a_consistent_shape() {
    let (mut store, _repo, items) = seeded(2);
    let run = start(&mut store, &items[0].id);
    let mut stranger = gate(&run, 1);
    stranger.item = items[1].id.clone();
    assert!(
        store.open_gate(&stranger).is_err(),
        "the item must own the run"
    );
    let mut decided = gate(&run, 1);
    decided.decision = Some(GateDecision::Approve { note: None });
    decided.decided_at = Some(t(25));
    assert!(matches!(
        store.open_gate(&decided),
        Err(ArchonError::Invalid(_))
    ));
    let mut undated = gate(&run, 1);
    undated.decision = Some(GateDecision::Approve { note: None });
    assert!(matches!(
        store.update_gate(&undated),
        Err(ArchonError::Invalid(_))
    ));

    let mut withdrawn = gate(&run, 1);
    withdrawn.withdrawn = Some("the run ended".into());
    withdrawn.decided_at = Some(t(26));
    assert!(matches!(
        store.open_gate(&withdrawn),
        Err(ArchonError::Invalid(_))
    ));

    store.open_gate(&gate(&run, 1)).unwrap();
    let finished = PipelineRun {
        state: RunState::Cancelled { reason: None },
        finished_at: Some(t(40)),
        ..run.clone()
    };
    let cancelled = QueueState::Cancelled { reason: None };
    settle_run(&mut store, &finished, cancelled, t(40));
    let closed = store.gate(&run.id, "propose", 1).unwrap().unwrap();
    assert_eq!(closed.withdrawn.as_deref(), Some("the run was cancelled"));
    assert_eq!(closed.decided_at, Some(t(40)));
    assert!(
        store.open_gates().unwrap().is_empty(),
        "finishing a run withdraws its gates"
    );
    let mut decided = closed.clone();
    decided.withdrawn = None;
    decided.decision = Some(GateDecision::Approve { note: None });
    let err = store.update_gate(&decided).unwrap_err();
    assert!(matches!(err, ArchonError::InvalidTransition(_)), "{err}");
    let late = store.open_gate(&gate(&run, 2)).unwrap_err();
    assert!(matches!(late, ArchonError::InvalidTransition(_)), "{late}");
}

#[test]
fn directives_expire_clear_and_are_capped() {
    let mut store = store();
    let lasting = Directive::new("ship docs fixes", DirectiveSource::Operator, None, t(0)).unwrap();
    let today =
        Directive::new("merge freely", DirectiveSource::Orchestrator, Some(1), t(1)).unwrap();
    store.insert_directive(&lasting).unwrap();
    store.insert_directive(&today).unwrap();
    assert_eq!(
        store.directives(t(2)).unwrap(),
        vec![lasting.clone(), today.clone()]
    );
    assert_eq!(
        store.directives(t(2 + 3600)).unwrap(),
        vec![lasting.clone()],
        "expired"
    );

    assert_eq!(store.clear_directives(Some(&lasting.id)).unwrap(), 1);
    let missing = store.clear_directives(Some(&lasting.id)).unwrap_err();
    assert!(matches!(missing, ArchonError::NotFound(_)), "{missing}");

    for n in 0..MAX_DIRECTIVES - 1 {
        let text = format!("rule {n}");
        let directive = Directive::new(&text, DirectiveSource::Operator, None, t(5)).unwrap();
        store.insert_directive(&directive).unwrap();
    }
    let extra = Directive::new("one more", DirectiveSource::Operator, None, t(6)).unwrap();
    assert!(matches!(
        store.insert_directive(&extra),
        Err(ArchonError::Invalid(_))
    ));
    // The expired one is pruned on insert, which makes room.
    let later = Directive::new("one more", DirectiveSource::Operator, None, t(2 + 3600)).unwrap();
    store.insert_directive(&later).unwrap();
    assert_eq!(store.clear_directives(None).unwrap(), MAX_DIRECTIVES as u32);
    assert!(store.directives(t(9000)).unwrap().is_empty());
}

fn gate_question(run: &PipelineRun, at: i64) -> Question {
    Question {
        id: QuestionId::new(),
        number: 0,
        text: "Approve the direction?".into(),
        options: vec!["yes".into(), "reject".into()],
        item: Some(run.item.clone()),
        aether: None,
        asked_by: None,
        asked_at: t(at),
        answer: None,
        answered_at: None,
    }
}

/// Opens gate `attempt` of `run` and links a freshly asked question to it.
fn asked_gate(store: &mut Store, run: &PipelineRun, attempt: u32) -> (OperatorGate, Question) {
    let question = store.insert_question(&gate_question(run, 25)).unwrap();
    let mut asked = store.open_gate(&gate(run, attempt)).unwrap();
    asked.question = Some(question.id.clone());
    store.update_gate(&asked).unwrap();
    (asked, question)
}

/// A store on disk with one running item, reopened by the test.
fn on_disk(dir: &tempfile::TempDir) -> (Store, PipelineRun) {
    let mut store = Store::open(&dir.path().join("archon.db")).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let queued = store
        .insert_item(&item(Some(&repo), "gated"), None)
        .unwrap();
    let run = start(&mut store, &queued.id);
    (store, run)
}

fn answers(store: &Store) -> Vec<(QuestionId, String)> {
    let feed = store.feed_after(0, usize::MAX).unwrap();
    feed.into_iter()
        .filter_map(|f| match f.entry {
            FeedEntry::Answer { question, text, .. } => Some((question, text)),
            _ => None,
        })
        .collect()
}

#[test]
fn finishing_a_run_closes_its_gate_questions_in_the_same_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, run) = on_disk(&dir);
    let (_, open) = asked_gate(&mut store, &run, 1);
    let (_, replied) = asked_gate(&mut store, &run, 2);
    // Answered in chat, but the decision was never persisted.
    store.answer_question(&replied.id, "yes", t(27)).unwrap();
    store.open_gate(&gate(&run, 3)).unwrap();
    let other = store.insert_question(&gate_question(&run, 26)).unwrap();

    let cancelled = PipelineRun {
        state: RunState::Cancelled { reason: None },
        finished_at: Some(t(40)),
        ..run.clone()
    };
    let finished = store
        .finish_run(&cancelled, QueueState::Cancelled { reason: None }, t(40))
        .unwrap();
    assert_eq!(finished.item.state.kind(), "cancelled");
    let text = "withdrawn: the run was cancelled".to_owned();
    let published: Vec<_> = finished.answers.iter().map(|f| &f.entry).collect();
    let expected = FeedEntry::Answer {
        question: open.id.clone(),
        text: text.clone(),
        via: AnswerVia::Archon,
    };
    assert_eq!(published, vec![&expected], "only the open question closes");
    drop(store);

    // Nothing is left for a later step that a crash could skip.
    let store = Store::open(&dir.path().join("archon.db")).unwrap();
    assert!(store.open_gates().unwrap().is_empty());
    let still_open: Vec<_> = store.questions(true).unwrap();
    assert_eq!(still_open, vec![other], "unrelated questions stay open");
    let all = store.questions(false).unwrap();
    let find = |id: &QuestionId| all.iter().find(|q| &q.id == id).unwrap().clone();
    assert_eq!(find(&open.id).answer.as_deref(), Some(text.as_str()));
    assert_eq!(find(&open.id).answered_at, Some(t(40)));
    assert_eq!(find(&replied.id).answer.as_deref(), Some("yes"), "kept");
    assert_eq!(answers(&store), vec![(open.id, text)]);
    assert_eq!(store.feed_after(0, 10).unwrap(), finished.answers);
}

#[test]
fn startup_closes_questions_left_open_on_withdrawn_gates() {
    let dir = tempfile::tempdir().unwrap();
    let (mut store, run) = on_disk(&dir);
    // What a crash after the old finish transaction left: the gate withdrawn
    // but its question open.
    let (mut stale, question) = asked_gate(&mut store, &run, 1);
    stale.withdrawn = Some("the run failed".into());
    stale.decided_at = Some(t(30));
    store.update_gate(&stale).unwrap();
    let (live, waiting) = asked_gate(&mut store, &run, 2);
    drop(store);

    let mut store = Store::open(&dir.path().join("archon.db")).unwrap();
    let closed = store.close_withdrawn_questions(t(50)).unwrap();
    assert_eq!(closed.len(), 1);
    let text = "withdrawn: the run failed".to_owned();
    assert_eq!(answers(&store), vec![(question.id.clone(), text)]);
    assert_eq!(store.questions(true).unwrap(), vec![waiting]);
    assert_eq!(
        store.open_gates().unwrap(),
        vec![live],
        "an open gate waits"
    );
    assert!(
        store.close_withdrawn_questions(t(51)).unwrap().is_empty(),
        "a second pass finds nothing"
    );
    assert_eq!(store.feed_after(0, 10).unwrap(), closed);
}
