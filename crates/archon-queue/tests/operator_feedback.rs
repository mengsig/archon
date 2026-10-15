//! Replays of operator corrections against fixed, isolated histories at a
//! fixed clock: the masks they put on routing samples and the window a
//! cohort with typed feedback reads.

mod common;

use std::path::Path;

use archon_core::Timestamp;
use archon_core::catalog::StageKind;
use archon_core::config::LearningConfig;
use archon_core::feedback::{CorrectionRequest, Credit};
use archon_core::model::{Effort, ModelChoice};
use archon_core::outcome::{CorrectionCause, RouteContext};
use archon_core::queue::QueueState;
use archon_core::repo::Repo;
use archon_core::run::{PipelineRun, StageRun, StageState};
use archon_queue::Store;
use archon_queue::store::{CorrectionFacts, RouteQuery, preferred_route};
use common::*;

fn model(name: &str) -> ModelChoice {
    ModelChoice {
        model: format!("codex/{name}").parse().unwrap(),
        effort: Effort::Low,
    }
}

fn context(review: &str) -> RouteContext {
    RouteContext {
        revision: "rev-1".into(),
        review: review.into(),
        task_class: "general".into(),
    }
}

fn days(n: i64) -> chrono::Duration {
    chrono::Duration::days(n)
}

/// One finished run of `repo` whose implement stage `choice` completed.
fn finished_run(
    store: &mut Store,
    repo: &Repo,
    choice: &ModelChoice,
    cost: f64,
    context: &RouteContext,
) -> PipelineRun {
    shaped_run(store, repo, (choice, cost), context, |stage| vec![stage])
}

/// A finished run whose implement stage ran as `attempts` of one
/// completed `choice` stage run say.
fn shaped_run(
    store: &mut Store,
    repo: &Repo,
    (choice, cost): (&ModelChoice, f64),
    context: &RouteContext,
    attempts: impl Fn(StageRun) -> Vec<StageRun>,
) -> PipelineRun {
    let item = store
        .insert_item(&item(Some(repo), "sample"), None)
        .unwrap();
    let run = start(store, &item.id);
    let stage = StageRun {
        choice: choice.clone(),
        state: StageState::Completed,
        cost_usd: cost,
        finished_at: Some(t(30)),
        ..stage(&run, 0)
    };
    for attempt in attempts(stage) {
        store.upsert_stage_run(&attempt).unwrap();
    }
    let next = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "done".into(),
    };
    settle_run(store, &run, next, t(50));
    store
        .capture_outcome(&store.run(&run.id).unwrap(), None, true)
        .unwrap();
    let mut outcome = store.outcome(&run.id).unwrap().unwrap();
    outcome.context = Some(context.clone());
    store.record_outcome(&outcome).unwrap();
    assert!(store.outcome(&run.id).unwrap().unwrap().is_win());
    store.run(&run.id).unwrap()
}

/// Five successful runs each of a cheap and a dearer choice in `repo`.
fn two_choices(store: &mut Store, repo: &Repo, context: &RouteContext) -> Vec<PipelineRun> {
    let mut cheap = Vec::new();
    for _ in 0..5 {
        cheap.push(finished_run(store, repo, &model("cheap"), 0.1, context));
        finished_run(store, repo, &model("dear"), 0.5, context);
    }
    cheap
}

fn request(run: &PipelineRun, cause: CorrectionCause, reason: &str) -> CorrectionRequest {
    CorrectionRequest {
        item: run.item.clone(),
        run: run.id.clone(),
        stage: "implement".into(),
        cause,
        reason: reason.into(),
        forget_memory: None,
    }
}

fn facts(observed_at: Timestamp) -> CorrectionFacts {
    CorrectionFacts {
        memory_feedback: None,
        delivery_commit: Some("abc123".into()),
        aether_instance: None,
        observed_at,
    }
}

/// The selected choice's name and each choice's (runs, passed) `as_of`.
fn standing(
    store: &Store,
    repo: Option<&Repo>,
    context: &RouteContext,
    as_of: Timestamp,
) -> (Option<String>, Vec<(String, u32, u32)>) {
    let query = RouteQuery {
        repo: repo.map(|r| &r.id),
        pipeline: "direct",
        stage: "implement",
        size: None,
        context: Some(context),
    };
    let evidence = store.cohort_evidence(&query, as_of).unwrap();
    let selected = preferred_route(&evidence.routes).map(|e| e.route.choice.model.model.clone());
    let mut rows: Vec<_> = evidence
        .routes
        .iter()
        .map(|e| {
            (
                e.route.choice.model.model.clone(),
                e.route.runs,
                e.route.passed,
            )
        })
        .collect();
    rows.sort();
    (selected, rows)
}

fn counts(cheap: u32) -> Vec<(String, u32, u32)> {
    vec![("cheap".into(), 5, cheap), ("dear".into(), 5, 5)]
}

/// A store at `path` whose one repository and review cohort holds five
/// runs each of a cheap and a dear choice; the cheap runs.
fn cohort(path: &Path) -> (Store, Repo, RouteContext, Vec<PipelineRun>) {
    let mut store = Store::open(path).unwrap();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let ctx = context("standard");
    let cheap = two_choices(&mut store, &repo, &ctx);
    (store, repo, ctx, cheap)
}

#[test]
fn one_quality_correction_moves_the_default_once() {
    let dir = tempfile::tempdir().unwrap();
    let (store, repo, ctx, cheap) = cohort(&dir.path().join("archon.db"));
    let as_of = t(200);
    assert_eq!(
        standing(&store, Some(&repo), &ctx, as_of),
        (Some("cheap".into()), counts(5))
    );

    let wrong = request(
        &cheap[2],
        CorrectionCause::Quality,
        "Ignored the identity rule",
    );
    let (first, replayed) = store
        .record_operator_correction(&wrong, &facts(t(100)))
        .unwrap();
    assert!(!replayed);
    assert!(
        matches!(first.attribution.credit, Credit::Model { ref choice, .. } if *choice == model("cheap"))
    );
    assert_eq!(first.attribution.delivery_commit.as_deref(), Some("abc123"));
    assert_eq!(
        standing(&store, Some(&repo), &ctx, as_of),
        (Some("dear".into()), counts(4))
    );
    assert!(
        standing(&store, Some(&repo), &ctx, t(99)).1 == counts(5),
        "not before it was made"
    );

    let (again, replayed) = store
        .record_operator_correction(&wrong, &facts(t(150)))
        .unwrap();
    assert!(replayed && again.id == first.id && again.observed_at == first.observed_at);
    let other = request(
        &cheap[2],
        CorrectionCause::Quality,
        "Also skipped the tests",
    );
    let (second, _) = store
        .record_operator_correction(&other, &facts(t(110)))
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(
        standing(&store, Some(&repo), &ctx, as_of).1,
        counts(4),
        "masked once"
    );
}

#[test]
fn undoing_every_correction_of_a_sample_restores_the_default_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let (store, repo, ctx, cheap) = cohort(&path);
    let work = facts(t(100));
    let mut ids = Vec::new();
    for reason in ["Ignored the identity rule", "Also skipped the tests"] {
        let wrong = request(&cheap[2], CorrectionCause::Quality, reason);
        ids.push(
            store
                .record_operator_correction(&wrong, &work)
                .unwrap()
                .0
                .id,
        );
    }
    drop(store);

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.item_feedback(&cheap[2].item).unwrap().len(),
        2,
        "kept across reopen"
    );
    let as_of = t(200);
    store.revoke_feedback(&ids[0], t(160)).unwrap();
    assert_eq!(standing(&store, Some(&repo), &ctx, as_of).1, counts(4));
    let revoked = store.revoke_feedback(&ids[1], t(170)).unwrap();
    assert_eq!(
        store.revoke_feedback(&ids[1], t(180)).unwrap(),
        revoked,
        "undo is idempotent"
    );
    assert_eq!(
        standing(&store, Some(&repo), &ctx, as_of),
        (Some("cheap".into()), counts(5))
    );
    assert_eq!(
        standing(&store, Some(&repo), &ctx, t(165)).1,
        counts(4),
        "replayed as of then"
    );
}

#[test]
fn informational_ambiguous_and_foreign_corrections_penalize_no_model() {
    let mut store = store();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let standard = context("standard");
    let cheap = two_choices(&mut store, &alpha, &standard);
    let work = facts(t(100));
    for cause in [
        CorrectionCause::Scope,
        CorrectionCause::Budget,
        CorrectionCause::Infrastructure,
    ] {
        let (event, _) = store
            .record_operator_correction(&request(&cheap[0], cause, "noted"), &work)
            .unwrap();
        assert!(event.penalized().is_none(), "{cause:?} is informational");
    }
    for kind in [Some(StageKind::Review), None] {
        let ran_as = |stage| vec![StageRun { kind, ..stage }];
        let elsewhere = context("strict");
        let run = shaped_run(
            &mut store,
            &alpha,
            (&model("cheap"), 0.1),
            &elsewhere,
            ran_as,
        );
        let quality = request(&run, CorrectionCause::Quality, "ran as another kind");
        let (event, _) = store
            .record_operator_correction(&quality, &facts(t(100)))
            .unwrap();
        assert!(
            matches!(event.attribution.credit, Credit::Unavailable { .. }),
            "{kind:?}"
        );
    }
    let mut foreign = request(&cheap[0], CorrectionCause::Quality, "wrong item");
    foreign.item = archon_core::id::QueueItemId::new();
    assert!(store.record_operator_correction(&foreign, &work).is_err());
    let mut absent = request(&cheap[0], CorrectionCause::Quality, "wrong stage");
    absent.stage = "review".into();
    assert!(store.record_operator_correction(&absent, &work).is_err());
    assert_eq!(
        store.item_feedback(&cheap[0].item).unwrap().len(),
        3,
        "refusals record nothing"
    );
    assert_eq!(
        standing(&store, Some(&alpha), &standard, t(200)),
        (Some("cheap".into()), counts(5)),
        "no model was penalized"
    );
}

#[test]
fn a_correction_masks_only_its_own_repository_and_review_cohort() {
    let mut store = store();
    let (alpha, beta) = (repo("alpha"), repo("beta"));
    store.insert_repo(&alpha).unwrap();
    store.insert_repo(&beta).unwrap();
    let (strict, standard) = (context("strict"), context("standard"));
    let cheap = two_choices(&mut store, &alpha, &standard);
    two_choices(&mut store, &alpha, &strict);
    two_choices(&mut store, &beta, &standard);
    let quality = request(&cheap[3], CorrectionCause::Quality, "real defect");
    let work = facts(t(100));
    store.record_operator_correction(&quality, &work).unwrap();
    assert_eq!(
        standing(&store, Some(&alpha), &standard, t(200)).1,
        counts(4)
    );
    assert_eq!(
        standing(&store, Some(&alpha), &strict, t(200)).1,
        counts(5),
        "other review cohort"
    );
    assert_eq!(
        standing(&store, Some(&beta), &standard, t(200)).1,
        counts(5),
        "other repository"
    );
    let pooled = standing(&store, None, &standard, t(200)).1;
    assert_eq!(
        pooled,
        vec![("cheap".into(), 10, 10), ("dear".into(), 10, 10)],
        "never pooled"
    );
}

#[test]
fn ambiguous_stage_histories_are_recorded_without_credit() {
    let mut store = store();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    let ctx = context("standard");
    let cheap = (&model("cheap"), 0.1);
    let fanned = shaped_run(&mut store, &repo, cheap, &ctx, |stage| {
        let id = archon_core::id::StageRunId::new();
        vec![
            stage.clone(),
            StageRun {
                id,
                fan_index: 1,
                ..stage
            },
        ]
    });
    let mixed = shaped_run(&mut store, &repo, cheap, &ctx, |stage| {
        let (id, choice) = (archon_core::id::StageRunId::new(), model("dear"));
        vec![
            stage.clone(),
            StageRun {
                id,
                attempt: 2,
                choice,
                ..stage
            },
        ]
    });
    let before = standing(&store, Some(&repo), &ctx, t(200));
    let work = facts(t(100));
    for (run, expected) in [(&fanned, "several agents"), (&mixed, "more than one model")] {
        let quality = request(run, CorrectionCause::Quality, expected);
        let (event, _) = store.record_operator_correction(&quality, &work).unwrap();
        let Credit::Unavailable { why } = &event.attribution.credit else {
            panic!("{expected}: the producing choice is ambiguous");
        };
        assert!(why.contains(expected), "{why}");
        assert!(!event.penalizing(t(200)));
    }
    assert_eq!(
        standing(&store, Some(&repo), &ctx, t(200)),
        before,
        "no sample changed"
    );
}

#[test]
fn a_cohort_with_typed_feedback_reads_ninety_days() {
    let mut store = store();
    let (alpha, beta) = (repo("alpha"), repo("beta"));
    store.insert_repo(&alpha).unwrap();
    store.insert_repo(&beta).unwrap();
    let ctx = context("standard");
    let cheap = two_choices(&mut store, &alpha, &ctx);
    two_choices(&mut store, &beta, &ctx);
    let quality = request(&cheap[0], CorrectionCause::Quality, "stale rule");
    let (event, _) = store
        .record_operator_correction(&quality, &facts(t(100)))
        .unwrap();
    let aged = t(50) + days(91);
    let query = |repo: &Repo, as_of| {
        let query = RouteQuery {
            repo: Some(&repo.id),
            pipeline: "direct",
            stage: "implement",
            size: None,
            context: Some(&ctx),
        };
        store.cohort_evidence(&query, as_of).unwrap()
    };
    let fresh = query(&alpha, t(200));
    assert!(fresh.feedback_history && fresh.penalties == vec![event.id.clone()]);
    assert!(fresh.corrected);
    let unread = query(&alpha, t(75) + days(90));
    assert!(unread.routes.is_empty() && unread.penalties.is_empty());
    assert!(
        unread.corrected,
        "its sample aged out first, yet the correction still stops exploration"
    );
    let old = query(&alpha, aged);
    assert!(old.routes.is_empty(), "every sample is older than 90 days");
    assert!(old.feedback_history && old.penalties.is_empty() && !old.corrected);
    assert!(
        !event.penalizing(t(100) + days(90)),
        "the penalty expired too"
    );
    let untouched = query(&beta, aged);
    assert!(
        !untouched.feedback_history && untouched.routes.len() == 2,
        "no window without feedback"
    );
    assert_eq!(
        store.item_feedback(&cheap[0].item).unwrap(),
        vec![event],
        "history stays"
    );
}

#[test]
fn disabled_learning_records_no_correction() {
    let dir = tempfile::tempdir().unwrap();
    let (store, repo, ctx, cheap) = cohort(&dir.path().join("archon.db"));
    store
        .configure_learning(&LearningConfig {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
    let refused = request(&cheap[1], CorrectionCause::Quality, "while disabled");
    assert!(
        store
            .record_operator_correction(&refused, &facts(t(300)))
            .is_err()
    );
    assert!(store.item_feedback(&cheap[1].item).unwrap().is_empty());
    assert_eq!(standing(&store, Some(&repo), &ctx, t(300)).1, counts(5));
}

#[test]
fn routing_reads_a_corrections_columns_never_its_record() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let (store, repo, ctx, cheap) = cohort(&path);
    let quality = request(&cheap[1], CorrectionCause::Quality, "stale rule");
    let (event, _) = store
        .record_operator_correction(&quality, &facts(t(100)))
        .unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    let blanked = raw
        .execute(
            "UPDATE operator_feedback SET data = '{}' WHERE id = ?1",
            [event.id.as_str()],
        )
        .unwrap();
    assert_eq!(blanked, 1);
    let query = RouteQuery {
        repo: Some(&repo.id),
        pipeline: "direct",
        stage: "implement",
        size: None,
        context: Some(&ctx),
    };
    let evidence = store.cohort_evidence(&query, t(200)).unwrap();
    assert!(evidence.feedback_history && evidence.penalties == vec![event.id]);
    assert_eq!(standing(&store, Some(&repo), &ctx, t(200)).1, counts(4));
}

#[test]
fn memory_feedback_intent_and_receipts_survive_reopening_without_changing_attribution() {
    use archon_core::aether_ids::{AetherMemoryFeedbackId, AetherMemoryId, AetherTaskId};
    use archon_core::feedback::{MemoryFeedback, MemoryFeedbackEntry, MemoryFeedbackTask};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let (store, _, _, cheap) = cohort(&path);
    let mut facts = facts(t(100));
    facts.memory_feedback = Some(MemoryFeedback {
        instance_id: "instance".into(),
        skipped: None,
        tasks: vec![MemoryFeedbackTask {
            task: AetherTaskId::parse("tsk_1").unwrap(),
            discovered: false,
            partial: false,
            skipped: None,
            error: None,
            records: Vec::new(),
        }],
    });
    let (first, _) = store
        .record_operator_correction(
            &request(&cheap[0], CorrectionCause::Quality, "wrong"),
            &facts,
        )
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.operator_feedback(&first.id).unwrap(), first);
    let mut ledger = first.memory_feedback.clone().unwrap();
    ledger.tasks[0].discovered = true;
    ledger.tasks[0].records.push(MemoryFeedbackEntry {
        memory: AetherMemoryId::parse("mem_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
        feedback: Some(AetherMemoryFeedbackId::parse("mfb_1").unwrap()),
        attempted: true,
        submission_horizon: Some("epoch:0".into()),
        unconfirmed: false,
        failed: None,
        retracted: false,
        dropped: false,
        error: None,
    });
    store.revoke_feedback(&first.id, t(150)).unwrap();
    let updated = store.update_memory_feedback(&first.id, &ledger).unwrap();
    assert_eq!(updated.attribution, first.attribution);
    assert_eq!(updated.revoked_at, Some(t(150)));
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.operator_feedback(&first.id).unwrap().memory_feedback,
        Some(ledger.clone())
    );
    ledger.instance_id = "other".into();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || store.update_memory_feedback(&first.id, &ledger)
        ))
        .is_err()
    );
}
