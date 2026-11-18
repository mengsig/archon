//! V24 recomputes each stored failed outcome's class from its stage records.

use super::*;
use archon_core::catalog::StageKind;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::model::{Effort, ModelChoice, ModelRef};
use archon_core::outcome::{ClassSource, FailureClass, Outcome, OutcomeResult};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState, Verdict};
use chrono::TimeZone;
use std::collections::BTreeMap;

fn at(secs: i64) -> archon_core::Timestamp {
    chrono::Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

/// A store as Archon 1.6.3 left it: every migration up to V23.
fn v23_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    configure(&conn, false).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
    )
    .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(RECLASSIFY_VERSION - 1).enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1, 'then')",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn
}

fn run(item: &QueueItemId, state: RunState) -> PipelineRun {
    PipelineRun {
        id: PipelineRunId::new(),
        item: item.clone(),
        pipeline: "review".into(),
        shipped: false,
        aether_run: None,
        workspace: "/wt/x".into(),
        team_root: None,
        branch: "archon/x".into(),
        state,
        cost_usd: 0.0,
        started_at: at(10),
        finished_at: Some(at(50)),
    }
}

fn stage(run: &PipelineRun, stage_id: &str, kind: StageKind, state: StageState) -> StageRun {
    StageRun {
        only_reviews: None,
        id: StageRunId::new(),
        run: run.id.clone(),
        stage_id: stage_id.into(),
        kind: Some(kind),
        attempt: 1,
        fan_index: 0,
        choice: ModelChoice::pinned(ModelRef::new("codex", "gpt"), Effort::High),
        model_segments: Vec::new(),
        infrastructure_failure: false,
        host_context: Default::default(),
        aether_agent: None,
        aether_task: None,
        verdict: None,
        summary: None,
        structured: None,
        state,
        cost_usd: 0.0,
        started_at: at(20),
        finished_at: Some(at(40)),
    }
}

fn outcome(run: &PipelineRun, failure: Option<FailureClass>) -> Outcome {
    let result = match failure {
        Some(_) => OutcomeResult::Failed,
        None => OutcomeResult::Succeeded,
    };
    serde_json::from_value(serde_json::json!({
        "run": run.id, "item": run.item, "repo": null, "pipeline": run.pipeline,
        "diff": null, "stages": [], "review_rounds": 1, "result": result,
        "failure": failure, "merged": null, "cost_usd": 0.5, "wall_secs": 40,
        "finished_at": at(50),
    }))
    .unwrap()
}

/// Seeds `run` (unless pruned), its stages and the outcome 1.6.3 recorded.
fn seed(
    conn: &Connection,
    run: &PipelineRun,
    stages: &[StageRun],
    old: Option<FailureClass>,
    pruned: bool,
) {
    let item = run.item.as_str();
    if !pruned {
        conn.execute(
            "INSERT INTO items (id, pipeline, priority, state, tags, created_at, updated_at, data)
             VALUES (?1, ?2, 1, 'failed', '[]', 'a', 'a', '{}')",
            [item, &run.pipeline],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
             VALUES (?1, ?2, ?3, 1, 'a', ?4)",
            [
                run.id.as_str(),
                item,
                &run.pipeline,
                &serde_json::to_string(run).unwrap(),
            ],
        )
        .unwrap();
    }
    for s in stages {
        conn.execute(
            "INSERT INTO stage_runs (id, run, stage_id, attempt, fan_index, state, terminal,
                                     started_at, data)
             VALUES (?1, ?2, ?3, 1, 0, 'done', 1, 'b', ?4)",
            [
                s.id.as_str(),
                run.id.as_str(),
                &s.stage_id,
                &serde_json::to_string(s).unwrap(),
            ],
        )
        .unwrap();
    }
    let o = outcome(run, old);
    conn.execute(
        "INSERT INTO outcomes (run, item, repo, pipeline, size, result, failure_class, corrected,
                              merged, cost_usd, finished_at, updated_at, data)
         VALUES (?1, ?2, NULL, ?6, NULL, ?3, ?4, 0, NULL, 0.5, 'f', 'u', ?5)",
        rusqlite::params![
            run.id.as_str(),
            item,
            o.result.as_str(),
            old.map(FailureClass::as_str),
            serde_json::to_string(&o).unwrap(),
            &run.pipeline
        ],
    )
    .unwrap();
}

/// (failure_class, class_source, failure_class_163, the record's class and source).
type Row = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<FailureClass>,
    Option<ClassSource>,
);

fn stored(conn: &Connection, run: &PipelineRun) -> Row {
    let (class, source, old, data): (Option<String>, Option<String>, Option<String>, String) = conn
        .query_row(
            "SELECT failure_class, class_source, failure_class_163, data FROM outcomes WHERE run = ?1",
            [run.id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let record: Outcome = serde_json::from_str(&data).unwrap();
    (class, source, old, record.failure, record.failure_source)
}

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

#[test]
fn v24_recomputes_classes_from_stage_records_and_keeps_pruned_ones() {
    let mut conn = v23_store();
    let item = || QueueItemId::new();
    let failed = |stage_id: &str, error: &str| RunState::Failed {
        stage_id: stage_id.into(),
        error: error.into(),
    };
    // 1.6.3 read "budget" in a review FAIL's error.
    let review = run(
        &item(),
        failed("review", "stage `review` failed: aether timeout budget"),
    );
    let mut verdict = stage(&review, "review", StageKind::Review, StageState::Completed);
    verdict.verdict = Some(Verdict::Fail);
    seed(
        &conn,
        &review,
        &[verdict],
        Some(FailureClass::Budget),
        false,
    );
    let late = run(&item(), failed("implement", "the stage did not finish"));
    let timed_out = stage(&late, "implement", StageKind::Work, StageState::TimedOut);
    seed(&conn, &late, &[timed_out], Some(FailureClass::Other), false);
    let capped = run(
        &item(),
        failed("budget", "BLOCKED: the item's budget is used up"),
    );
    seed(&conn, &capped, &[], Some(FailureClass::Blocked), false);
    let pruned = run(&item(), failed("review", "Aether went away"));
    seed(
        &conn,
        &pruned,
        &[],
        Some(FailureClass::Infrastructure),
        true,
    );
    let passed = run(
        &item(),
        RunState::Succeeded {
            summary: "ok".into(),
        },
    );
    seed(&conn, &passed, &[], None, false);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let recomputed = |class: &str, old: &str, record: FailureClass| {
        (
            some(class),
            some("recomputed"),
            some(old),
            Some(record),
            Some(ClassSource::Recomputed),
        )
    };
    assert_eq!(
        stored(&conn, &review),
        recomputed("review", "budget", FailureClass::Review)
    );
    assert_eq!(
        stored(&conn, &late),
        recomputed("timeout", "other", FailureClass::Timeout)
    );
    assert_eq!(
        stored(&conn, &capped),
        recomputed("budget", "blocked", FailureClass::Budget)
    );
    assert_eq!(
        stored(&conn, &pruned),
        (
            some("infrastructure"),
            some("legacy_163"),
            some("infrastructure"),
            Some(FailureClass::Infrastructure),
            Some(ClassSource::Legacy163)
        ),
        "a pruned run keeps the class 1.6.3 gave it"
    );
    assert_eq!(stored(&conn, &passed), (None, None, None, None, None));
    let unchanged: String = conn
        .query_row(
            "SELECT updated_at FROM outcomes WHERE run = ?1",
            [review.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, "u", "reclassifying is no correction");
}

#[test]
fn reclassify_counts_add_up_and_v24_runs_on_an_empty_store() {
    let mut empty = v23_store();
    assert_eq!(migrate(&mut empty).unwrap(), MIGRATIONS.len());
    let none = super::super::outcomes::class::reclassify(&empty).unwrap();
    assert_eq!(none.recomputed, 0);
    assert!(none.before.is_empty() && none.legacy.is_empty());

    let conn = v23_store();
    let review = run(
        &QueueItemId::new(),
        RunState::Failed {
            stage_id: "review".into(),
            error: "timed out talking to aether".into(),
        },
    );
    let mut verdict = stage(&review, "review", StageKind::Review, StageState::Completed);
    verdict.verdict = Some(Verdict::Fail);
    seed(
        &conn,
        &review,
        &[verdict],
        Some(FailureClass::Timeout),
        false,
    );
    let pruned = run(
        &QueueItemId::new(),
        RunState::Failed {
            stage_id: "review".into(),
            error: "x".into(),
        },
    );
    seed(&conn, &pruned, &[], Some(FailureClass::Review), true);
    conn.execute_batch(V24).unwrap();
    let counts = super::super::outcomes::class::reclassify(&conn).unwrap();
    assert_eq!(counts.recomputed, 1);
    assert_eq!(
        counts.before,
        BTreeMap::from([("review".into(), 1), ("timeout".into(), 1)])
    );
    assert_eq!(counts.after, BTreeMap::from([("review".into(), 2)]));
    assert_eq!(counts.legacy, BTreeMap::from([("review".into(), 1)]));
}

/// The class columns of a row, read without decoding its record.
fn columns(
    conn: &Connection,
    run: &PipelineRun,
) -> (Option<String>, Option<String>, Option<String>) {
    conn.query_row(
        "SELECT failure_class, class_source, failure_class_163 FROM outcomes WHERE run = ?1",
        [run.id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn failed_stage(run: &PipelineRun, kind: StageKind, error: &str, infra: bool) -> StageRun {
    let error = error.into();
    let mut failed = stage(run, &run_stage(run), kind, StageState::Failed { error });
    failed.infrastructure_failure = infra;
    failed
}

fn run_stage(run: &PipelineRun) -> String {
    match &run.state {
        RunState::Failed { stage_id, .. } => stage_id.clone(),
        _ => unreachable!("only a failed run is classed"),
    }
}

fn failed_run(stage_id: &str, error: &str) -> PipelineRun {
    let state = RunState::Failed {
        stage_id: stage_id.into(),
        error: error.into(),
    };
    run(&QueueItemId::new(), state)
}

#[test]
fn v24_reads_daemon_stops_and_nested_reviews() {
    let mut conn = v23_store();
    let nested = failed_run("implement", "stage `implement` failed its gate");
    let mut work = stage(&nested, "implement", StageKind::Work, StageState::Completed);
    work.verdict = Some(Verdict::Pass);
    work.structured = Some(serde_json::json!({"review": {"verdict": "FAIL"}}));
    seed(
        &conn,
        &nested,
        &[work.clone()],
        Some(FailureClass::Other),
        false,
    );
    let refused = failed_run("implement", "stage `implement` failed its gate");
    work.run = refused.id.clone();
    work.id = StageRunId::new();
    work.structured =
        Some(serde_json::json!({"review": {"verdict": "pass", "unverified": "no reviewer ran"}}));
    seed(&conn, &refused, &[work], Some(FailureClass::Other), false);
    let spent = failed_run(
        "implement",
        "BLOCKED: the item's budget is used up: $1.00 spent of its $1.00 cap ($0.00 over)",
    );
    seed(&conn, &spent, &[], Some(FailureClass::Blocked), false);
    let capped = failed_run("implement", "agent failed");
    let stop = failed_stage(
        &capped,
        StageKind::Work,
        "cost budget of $2.00: $2.01 spent",
        true,
    );
    seed(
        &conn,
        &capped,
        &[stop],
        Some(FailureClass::Infrastructure),
        false,
    );
    let quoted = "cost budget of $5 is too small for this job";
    let quoting = failed_run("implement", quoted);
    let words = failed_stage(&quoting, StageKind::Work, quoted, false);
    seed(&conn, &quoting, &[words], Some(FailureClass::Budget), false);
    let disk = failed_run(
        "review",
        "create_agent failed on a full disk 5 times (ENOSPC)",
    );
    let full = failed_stage(
        &disk,
        StageKind::Review,
        "create_agent failed on a full disk 5 times (ENOSPC)",
        false,
    );
    seed(
        &conn,
        &disk,
        &[full],
        Some(FailureClass::Infrastructure),
        false,
    );

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let class = |run: &PipelineRun| columns(&conn, run).0;
    assert_eq!(
        class(&nested),
        some("review"),
        "the gate read a failed nested review"
    );
    assert_eq!(
        class(&refused),
        some("review"),
        "Archon refused the claimed review"
    );
    assert_eq!(
        class(&spent),
        some("budget"),
        "the cap ran out before dispatch"
    );
    assert_eq!(
        class(&capped),
        some("budget"),
        "Aether's cost stop outranks its failure flag"
    );
    assert_eq!(
        class(&quoting),
        some("other"),
        "a task quoting a budget is no budget stop"
    );
    assert_eq!(
        class(&disk),
        some("infrastructure"),
        "Aether's disk stayed full"
    );
}

#[test]
fn v24_keeps_the_old_class_of_rows_it_cannot_read_and_opens() {
    let mut conn = v23_store();
    let failed_review = |run: &PipelineRun| {
        let mut verdict = stage(run, "review", StageKind::Review, StageState::Completed);
        verdict.verdict = Some(Verdict::Fail);
        verdict
    };
    let torn = failed_run("review", "x");
    seed(
        &conn,
        &torn,
        &[failed_review(&torn)],
        Some(FailureClass::Other),
        false,
    );
    conn.execute(
        "UPDATE stage_runs SET data = '{\"torn\": true}' WHERE run = ?1",
        [torn.id.as_str()],
    )
    .unwrap();
    let odd = failed_run("review", "x");
    seed(
        &conn,
        &odd,
        &[failed_review(&odd)],
        Some(FailureClass::Budget),
        false,
    );
    conn.execute(
        "UPDATE outcomes SET data = json_set(data, '$.finished_at', 'not a time') WHERE run = ?1",
        [odd.id.as_str()],
    )
    .unwrap();
    let garbage = failed_run("review", "x");
    seed(
        &conn,
        &garbage,
        &[failed_review(&garbage)],
        Some(FailureClass::Budget),
        false,
    );
    conn.execute(
        "UPDATE outcomes SET data = 'not json' WHERE run = ?1",
        [garbage.id.as_str()],
    )
    .unwrap();

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &torn),
        (some("other"), some("legacy_163"), some("other")),
        "a lost ending slot could hide what stopped the run"
    );
    assert_eq!(
        columns(&conn, &odd),
        (some("review"), some("recomputed"), some("budget"))
    );
    let odd_record: String = conn
        .query_row(
            "SELECT json_extract(data, '$.failure') FROM outcomes WHERE run = ?1",
            [odd.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(odd_record, "review");
    assert_eq!(columns(&conn, &garbage).1, some("recomputed"));
    let kept: String = conn
        .query_row(
            "SELECT data FROM outcomes WHERE run = ?1",
            [garbage.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, "not json", "an unreadable record is left as it was");
}

#[test]
fn v24_freezes_the_reviewed_pull_request_into_its_outcome() {
    let mut conn = v23_store();
    let pr = "https://github.com/o/r/pull/7";
    let reviewed = failed_run("review", "x");
    let review = stage(
        &reviewed,
        "review",
        StageKind::Review,
        StageState::Completed,
    );
    seed(
        &conn,
        &reviewed,
        &[review],
        Some(FailureClass::Review),
        false,
    );
    // The bundled review pipeline's lead is a work stage; 1.6.3 kept no duty.
    let failed_lead = failed_run("review", "the review found blocking issues");
    let lead = failed_stage(
        &failed_lead,
        StageKind::Work,
        "the review found blocking issues",
        false,
    );
    assert!(lead.only_reviews.is_none(), "a 1.6.3 record");
    seed(
        &conn,
        &failed_lead,
        &[lead],
        Some(FailureClass::Other),
        false,
    );
    let read = failed_run("read", "x");
    let analysis = stage(&read, "read", StageKind::Work, StageState::Completed);
    seed(&conn, &read, &[analysis], Some(FailureClass::Other), false);
    let target = serde_json::json!({"onto": {"review_only": true, "pr": pr}}).to_string();
    for run in [&reviewed, &failed_lead, &read] {
        conn.execute(
            "UPDATE items SET data = ?2 WHERE id = ?1",
            [run.item.as_str(), &target],
        )
        .unwrap();
    }

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let frozen = |run: &PipelineRun| -> Option<String> {
        let data: String = conn
            .query_row(
                "SELECT data FROM outcomes WHERE run = ?1",
                [run.id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str::<Outcome>(&data).unwrap().review_pr
    };
    assert_eq!(
        frozen(&reviewed),
        some(pr),
        "the record itself names the pull request"
    );
    assert_eq!(
        frozen(&failed_lead),
        some(pr),
        "a failed round of the work-kind lead"
    );
    assert_eq!(columns(&conn, &failed_lead).0, some("review"));
    assert_eq!(
        frozen(&read),
        None,
        "a pipeline that reviewed nothing reviewed no pull request"
    );
}

#[test]
fn v24_keeps_the_old_class_of_runs_whose_rows_hold_another_run() {
    let mut conn = v23_store();
    let failed_review = |run: &PipelineRun| {
        let mut verdict = stage(run, "review", StageKind::Review, StageState::Completed);
        verdict.verdict = Some(Verdict::Fail);
        verdict
    };
    let stray_stage = failed_run("review", "x");
    let mut stray = failed_review(&stray_stage);
    stray.run = PipelineRunId::new();
    seed(
        &conn,
        &stray_stage,
        &[stray],
        Some(FailureClass::Other),
        false,
    );
    let stray_run = failed_run("review", "x");
    seed(
        &conn,
        &stray_run,
        &[failed_review(&stray_run)],
        Some(FailureClass::Other),
        false,
    );
    let mut other = stray_run.clone();
    other.id = PipelineRunId::new();
    conn.execute(
        "UPDATE runs SET data = ?2 WHERE id = ?1",
        [
            stray_run.id.as_str(),
            &serde_json::to_string(&other).unwrap(),
        ],
    )
    .unwrap();

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let kept = (some("other"), some("legacy_163"), some("other"));
    assert_eq!(
        columns(&conn, &stray_stage),
        kept,
        "a stage row of another run"
    );
    assert_eq!(
        columns(&conn, &stray_run),
        kept,
        "a run row holding another run"
    );
}

fn lesson(conn: &Connection, uses: u32) -> archon_core::id::LessonId {
    let lesson: archon_core::lesson::Lesson = serde_json::from_value(serde_json::json!({
        "id": archon_core::id::LessonId::new(), "scope": {"repo": null, "pipeline": null},
        "kind": "pitfall", "text": "mind the gap", "state": "active", "uses": uses, "wins": 0,
        "confidence": 0.5, "source": "operator", "created_at": at(0), "updated_at": at(0),
    }))
    .unwrap();
    conn.execute(
        "INSERT INTO lessons (id, kind, state, pinned, uses, wins, confidence, source, created_at,
                              updated_at, data)
         VALUES (?1, 'pitfall', 'active', 0, ?2, 0, 0.5, 'operator', 't', 't', ?3)",
        rusqlite::params![
            lesson.id.as_str(),
            uses,
            serde_json::to_string(&lesson).unwrap()
        ],
    )
    .unwrap();
    lesson.id
}

fn use_and_trial(
    conn: &Connection,
    lesson: &archon_core::id::LessonId,
    run: &PipelineRun,
    won: Option<bool>,
) {
    conn.execute(
        "INSERT INTO lesson_uses (lesson, run, used_at, won) VALUES (?1, ?2, 't', ?3)",
        rusqlite::params![lesson.as_str(), run.id.as_str(), won],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO lesson_trials (lesson, run, item, held, at, won) VALUES (?1, ?2, ?3, 1, 't', ?4)",
        rusqlite::params![lesson.as_str(), run.id.as_str(), run.item.as_str(), won],
    )
    .unwrap();
}

#[test]
fn v24_rescores_the_lessons_of_runs_whose_scoring_changed() {
    let mut conn = v23_store();
    let capped = failed_run(
        "budget",
        "BLOCKED: the item's budget is used up: $1.00 spent of its $1.00 cap ($0.00 over)",
    );
    seed(&conn, &capped, &[], Some(FailureClass::Blocked), false);
    let reviewed = failed_run("review", "stage `review` failed: aether budget");
    let mut verdict = stage(
        &reviewed,
        "review",
        StageKind::Review,
        StageState::Completed,
    );
    verdict.verdict = Some(Verdict::Fail);
    seed(
        &conn,
        &reviewed,
        &[verdict],
        Some(FailureClass::Budget),
        false,
    );
    let was_loss = lesson(&conn, 1);
    use_and_trial(&conn, &was_loss, &capped, Some(false));
    let was_unscored = lesson(&conn, 0);
    use_and_trial(&conn, &was_unscored, &reviewed, None);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let won = |table: &str, run: &PipelineRun| -> Option<bool> {
        conn.query_row(
            &format!("SELECT won FROM {table} WHERE run = ?1"),
            [run.id.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    let uses = |id: &archon_core::id::LessonId| -> u32 {
        conn.query_row(
            "SELECT uses FROM lessons WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        won("lesson_uses", &capped),
        None,
        "a budget stop is no loss"
    );
    assert_eq!(won("lesson_trials", &capped), None, "nor in the holdout");
    assert_eq!(
        won("lesson_uses", &reviewed),
        Some(false),
        "a failed review is a loss"
    );
    assert_eq!(won("lesson_trials", &reviewed), Some(false));
    assert_eq!(
        (uses(&was_loss), uses(&was_unscored)),
        (0, 1),
        "the lessons are recounted"
    );
}

#[test]
fn v24_skips_a_pull_request_target_it_cannot_read() {
    let mut conn = v23_store();
    let reviewed = failed_run("review", "x");
    let review = stage(
        &reviewed,
        "review",
        StageKind::Review,
        StageState::Completed,
    );
    seed(
        &conn,
        &reviewed,
        &[review],
        Some(FailureClass::Review),
        false,
    );
    let target = serde_json::json!({"onto": {"review_only": true, "pr": 42}}).to_string();
    conn.execute(
        "UPDATE items SET data = ?2 WHERE id = ?1",
        [reviewed.item.as_str(), &target],
    )
    .unwrap();

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let data: String = conn
        .query_row(
            "SELECT data FROM outcomes WHERE run = ?1",
            [reviewed.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    let outcome: Outcome = serde_json::from_str(&data).expect("the outcome stays readable");
    assert_eq!(outcome.review_pr, None);
}

#[test]
fn v24_keeps_the_old_class_of_a_run_whose_ending_stage_left_no_record() {
    let mut conn = v23_store();
    let lost = failed_run("review", "stage `review` failed: aether budget");
    let earlier = stage(&lost, "plan", StageKind::Work, StageState::Completed);
    seed(&conn, &lost, &[earlier], Some(FailureClass::Budget), false);
    let shipped = failed_run(archon_core::run::SHIP_STEP, "the push was refused");
    seed(&conn, &shipped, &[], Some(FailureClass::Other), false);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &lost),
        (some("budget"), some("legacy_163"), some("budget")),
        "a lost record of the stage that ended it could hide its stop"
    );
    assert_eq!(
        columns(&conn, &shipped),
        (some("ship"), some("recomputed"), some("other")),
        "the ship step runs no stage"
    );
}

#[test]
fn v24_reads_a_163_lead_only_in_the_bundled_review_pipeline() {
    let mut conn = v23_store();
    let pr = "https://github.com/o/r/pull/7";
    let mut other = failed_run("review-input", "the input was malformed");
    other.pipeline = "read".into();
    let input = failed_stage(&other, StageKind::Work, "the input was malformed", false);
    seed(&conn, &other, &[input], Some(FailureClass::Other), false);
    let mut named = failed_run("review", "the input was malformed");
    named.pipeline = "read".into();
    let work = failed_stage(&named, StageKind::Work, "the input was malformed", false);
    seed(&conn, &named, &[work], Some(FailureClass::Other), false);
    let target = serde_json::json!({"onto": {"review_only": true, "pr": pr}}).to_string();
    let unasked = failed_run("review", "the review found blocking issues");
    let lead = failed_stage(&unasked, StageKind::Work, "blocking issues", false);
    seed(&conn, &unasked, &[lead], Some(FailureClass::Other), false);
    let none = serde_json::json!({
        "onto": {"review_only": true, "pr": pr}, "autonomy": {"review": "none"}
    });
    for (run, data) in [
        (&other, &target),
        (&named, &target),
        (&unasked, &none.to_string()),
    ] {
        conn.execute(
            "UPDATE items SET data = ?2 WHERE id = ?1",
            [run.item.as_str(), data],
        )
        .unwrap();
    }

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let review_pr = |run: &PipelineRun| -> Option<String> {
        conn.query_row(
            "SELECT json_extract(data, '$.review_pr') FROM outcomes WHERE run = ?1",
            [run.id.as_str()],
            |r| r.get(0),
        )
        .unwrap()
    };
    for run in [&other, &named] {
        assert_eq!(columns(&conn, run).0, some("other"), "{}", run_stage(run));
        assert_eq!(review_pr(run), None, "{} reviewed nothing", run_stage(run));
    }
    assert_eq!(columns(&conn, &unasked).0, some("review"));
    assert_eq!(review_pr(&unasked), None, "level none records no review");
}
