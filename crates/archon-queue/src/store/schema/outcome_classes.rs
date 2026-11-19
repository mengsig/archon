//! V24 recomputes each stored failed outcome's class from its stage records.
//! Its reviewed pull requests are in `outcome_reviews`, the rows it cannot
//! trust in `outcome_kept`.

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
pub(super) fn v23_store() -> Connection {
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

pub(super) fn stage(
    run: &PipelineRun,
    stage_id: &str,
    kind: StageKind,
    state: StageState,
) -> StageRun {
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
        record_repair: None,
        host_context: Default::default(),
        aether_agent: None,
        aether_task: None,
        verdict: None,
        verdict_note: None,
        sizing: Default::default(),
        restatement: None,
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

/// A time as the store writes it into a column.
fn stored_time(at: &archon_core::Timestamp) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Seeds `run` (unless pruned), its stages and the outcome 1.6.3 recorded,
/// each row's columns as the store writes them.
pub(super) fn seed(
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
             VALUES (?1, ?2, ?3, ?5, ?6, ?4)",
            rusqlite::params![
                run.id.as_str(),
                item,
                &run.pipeline,
                &serde_json::to_string(run).unwrap(),
                run.state.is_terminal(),
                stored_time(&run.started_at)
            ],
        )
        .unwrap();
    }
    for s in stages {
        conn.execute(
            "INSERT INTO stage_runs (id, run, stage_id, attempt, fan_index, state, terminal,
                                     started_at, data)
             VALUES (?1, ?2, ?3, ?5, ?6, ?7, ?8, ?9, ?4)",
            rusqlite::params![
                s.id.as_str(),
                run.id.as_str(),
                &s.stage_id,
                &serde_json::to_string(s).unwrap(),
                s.attempt,
                s.fan_index,
                serde_json::to_value(&s.state).unwrap()["state"].as_str(),
                s.state.is_terminal(),
                stored_time(&s.started_at)
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

pub(super) fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// Records that the outcome of `run` observed `attempts` stage runs of `stage_id`.
pub(super) fn observe(conn: &Connection, run: &PipelineRun, stage_id: &str, attempts: u32) {
    conn.execute(
        "INSERT INTO outcome_stages (run, ordinal, stage_id, model, effort, cost_usd, wall_secs,
                                     attempts, passed)
         VALUES (?1, 0, ?2, 'codex/gpt', 'high', 0.5, 20, ?3, 0)",
        rusqlite::params![run.id.as_str(), stage_id, attempts],
    )
    .unwrap();
}

/// The review tally archond kept for `stage`: one reviewer failed its work.
pub(super) fn tally_a_fail(conn: &Connection, stage: &StageRun) {
    let tally = serde_json::json!({
        "fails": [{"agent": "agt_01JB000000000000000000000R", "head": "abc1234", "findings": ["a bug"]}],
        "read": ["agt_01JB000000000000000000000R"],
    });
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2)",
        [
            archon_core::run::review_tally_key(&stage.id),
            tally.to_string(),
        ],
    )
    .unwrap();
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
    let none = super::super::outcomes::reclassify::reclassify(&empty).unwrap();
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
    let counts = super::super::outcomes::reclassify::reclassify(&conn).unwrap();
    assert_eq!(counts.recomputed, 1);
    assert_eq!(
        counts.before,
        BTreeMap::from([("review".into(), 1), ("timeout".into(), 1)])
    );
    assert_eq!(counts.after, BTreeMap::from([("review".into(), 2)]));
    assert_eq!(counts.legacy, BTreeMap::from([("review".into(), 1)]));
}

/// The class columns of a row, read without decoding its record.
pub(super) fn columns(
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

pub(super) fn failed_stage(
    run: &PipelineRun,
    kind: StageKind,
    error: &str,
    infra: bool,
) -> StageRun {
    let error = error.into();
    let mut failed = stage(run, &run_stage(run), kind, StageState::Failed { error });
    failed.infrastructure_failure = infra;
    failed
}

pub(super) fn run_stage(run: &PipelineRun) -> String {
    match &run.state {
        RunState::Failed { stage_id, .. } => stage_id.clone(),
        _ => unreachable!("only a failed run is classed"),
    }
}

pub(super) fn failed_run(stage_id: &str, error: &str) -> PipelineRun {
    let state = RunState::Failed {
        stage_id: stage_id.into(),
        error: error.into(),
    };
    run(&QueueItemId::new(), state)
}

pub(super) fn succeeded_run() -> PipelineRun {
    let summary = "ok".into();
    run(&QueueItemId::new(), RunState::Succeeded { summary })
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
        "cost budget of $2: $2.0100 spent",
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

pub(super) fn lesson(conn: &Connection, uses: u32) -> archon_core::id::LessonId {
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

pub(super) fn use_and_trial(
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
fn v24_keeps_the_old_class_of_a_run_whose_ending_stage_left_no_record() {
    let mut conn = v23_store();
    let lost = failed_run("review", "stage `review` failed: aether budget");
    let earlier = stage(&lost, "plan", StageKind::Work, StageState::Completed);
    seed(&conn, &lost, &[earlier], Some(FailureClass::Budget), false);
    let unstarted = failed_run("budget", "BLOCKED: the item's budget is used up: $1 of $1");
    seed(&conn, &unstarted, &[], Some(FailureClass::Blocked), false);
    let shipped = failed_run(archon_core::run::SHIP_STEP, "the push was refused");
    let built = stage(&shipped, "build", StageKind::Work, StageState::Completed);
    seed(&conn, &shipped, &[built], Some(FailureClass::Other), false);
    let aborted = "stage `ship` was aborted by a steer, and the stage to continue with no longer \
                   exists";
    let skipped = failed_run(archon_core::run::SHIP_STEP, aborted);
    seed(&conn, &skipped, &[], Some(FailureClass::Ship), false);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &skipped),
        (some("ship"), some("legacy_163"), some("ship")),
        "the driver's word on a stage named `ship` is no ship step"
    );

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
    assert_eq!(
        columns(&conn, &unstarted),
        (some("budget"), some("recomputed"), some("blocked")),
        "a run stopped before its first stage misses no stage run"
    );
}

/// A delivery that failed after a work stage named `ship` completed or was
/// skipped stays the ship step's failure, and a completed stage's stated
/// escalation is read.
#[test]
fn v24_reads_the_ship_step_and_a_stated_stop_past_a_settled_stage() {
    let mut conn = v23_store();
    let refused = failed_run(archon_core::run::SHIP_STEP, "the push was refused");
    let wrote = stage(&refused, "ship", StageKind::Work, StageState::Completed);
    seed(&conn, &refused, &[wrote], Some(FailureClass::Ship), false);
    let late = failed_run(archon_core::run::SHIP_STEP, "the fast check failed");
    let skipped = stage(&late, "ship", StageKind::Work, StageState::Cancelled);
    seed(&conn, &late, &[skipped], Some(FailureClass::Ship), false);
    let said = archon_core::run::STATED_FAILURE;
    let stated = failed_run("implement", &format!("stage `implement` failed: {said}"));
    let mut gave_up = stage(&stated, "implement", StageKind::Work, StageState::Completed);
    gave_up.summary = Some("ESCALATE: this needs another pipeline".into());
    seed(&conn, &stated, &[gave_up], Some(FailureClass::Other), false);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &refused),
        (some("ship"), some("recomputed"), some("ship"))
    );
    assert_eq!(
        columns(&conn, &late),
        (some("ship"), some("recomputed"), some("ship")),
        "a stage a steer skipped explains no later delivery"
    );
    assert_eq!(
        columns(&conn, &stated),
        (some("escalated"), some("recomputed"), some("other"))
    );
}

/// The `won` of the lesson use of `run`, and the uses its lesson counts.
pub(super) fn lesson_score(conn: &Connection, run: &PipelineRun) -> (Option<bool>, u32) {
    conn.query_row(
        "SELECT u.won, l.uses FROM lesson_uses u JOIN lessons l ON l.id = u.lesson
         WHERE u.run = ?1",
        [run.id.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn v24_keeps_the_class_and_lesson_scores_of_a_kept_run_missing_stage_rows() {
    let mut conn = v23_store();
    let failed_review = |run: &PipelineRun| {
        let mut verdict = stage(run, "review", StageKind::Review, StageState::Completed);
        verdict.verdict = Some(Verdict::Fail);
        verdict
    };
    // The outcome observed two runs of the review stage; one row is gone,
    // and it may be the one that timed out.
    let torn = failed_run("review", "stage `review` failed: aether budget");
    seed(
        &conn,
        &torn,
        &[failed_review(&torn)],
        Some(FailureClass::Budget),
        false,
    );
    observe(&conn, &torn, "review", 2);
    // Every row of the stage is gone, though a prefix names the stop.
    let emptied = failed_run(
        "implement",
        "BLOCKED: the item's budget is used up: $1 of $1",
    );
    seed(&conn, &emptied, &[], Some(FailureClass::Blocked), false);
    observe(&conn, &emptied, "implement", 1);
    let whole = failed_run("review", "stage `review` failed: aether budget");
    seed(
        &conn,
        &whole,
        &[failed_review(&whole)],
        Some(FailureClass::Budget),
        false,
    );
    observe(&conn, &whole, "review", 1);
    let unscored = lesson(&conn, 0);
    use_and_trial(&conn, &unscored, &torn, None);
    let lost = lesson(&conn, 1);
    use_and_trial(&conn, &lost, &emptied, Some(false));
    let rescored = lesson(&conn, 0);
    use_and_trial(&conn, &rescored, &whole, None);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &torn),
        (some("budget"), some("legacy_163"), some("budget")),
        "a missing stage run could hide what stopped the run"
    );
    assert_eq!(
        lesson_score(&conn, &torn),
        (None, 0),
        "its lesson use is not rescored"
    );
    assert_eq!(
        columns(&conn, &emptied),
        (some("blocked"), some("legacy_163"), some("blocked")),
        "a prefix recomputes no run whose stage runs are gone"
    );
    assert_eq!(lesson_score(&conn, &emptied), (Some(false), 1));
    assert_eq!(
        columns(&conn, &whole),
        (some("review"), some("recomputed"), some("budget")),
        "a run with every observed stage run is recomputed"
    );
    assert_eq!(lesson_score(&conn, &whole), (Some(false), 1));
}

/// A 1.6.3 row of a spawn given up on in a workspace gone missing, flagged
/// nothing and holding no agent, stays the infrastructure failure it was.
#[test]
fn v24_reads_a_spawn_failure_wrapped_by_a_missing_workspace() {
    let mut conn = v23_store();
    let error = "workspace missing: /wt/x; the engine said: \
        create_agent kept failing: aether: internal: storage temporarily unavailable";
    let wrapped = failed_run("implement", error);
    let gave_up = failed_stage(&wrapped, StageKind::Work, error, false);
    seed(
        &conn,
        &wrapped,
        &[gave_up],
        Some(FailureClass::Infrastructure),
        false,
    );
    let scenario = format!(
        "stage `e2e` failed: strict scenario gate failed: scenario import: {}: a step has no id",
        archon_core::run::REVIEW_NOT_ON_RECORD
    );
    let strict = failed_run("e2e", &scenario);
    let mut test = stage(&strict, "e2e", StageKind::Test, StageState::Completed);
    test.verdict = Some(Verdict::Pass);
    seed(&conn, &strict, &[test], Some(FailureClass::Other), false);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &wrapped),
        (
            some("infrastructure"),
            some("recomputed"),
            some("infrastructure")
        )
    );
    assert_eq!(
        columns(&conn, &strict).0,
        some("verification"),
        "a scenario's id is no gate reason"
    );
}

/// The wrapper is read at the end of the path the run's records name, never at
/// a separator inside a configured path; a bare refusal of that path before any
/// agent ran is the machine's too, and a path the run never had is neither.
#[test]
fn v24_reads_the_wrapper_at_the_end_of_the_runs_own_path() {
    let mut conn = v23_store();
    let odd_root = "/tmp/trees; the engine said: create_agent kept failing: archive";
    let gave_up = "create_agent kept failing: aether: internal: storage temporarily unavailable";
    let seeded = |conn: &Connection, workspace: &str, team_root: Option<&str>, error: String| {
        let mut run = failed_run("implement", &error);
        run.workspace = workspace.into();
        run.team_root = team_root.map(Into::into);
        let failed = failed_stage(&run, StageKind::Work, &error, false);
        seed(conn, &run, &[failed], Some(FailureClass::Other), false);
        run
    };
    let tree = format!("{odd_root}/run");
    let bare = seeded(&conn, &tree, None, format!("workspace missing: {tree}"));
    let wrapped = seeded(
        &conn,
        &tree,
        None,
        format!("workspace missing: {tree}; the engine said: {gave_up}"),
    );
    let root = format!("{odd_root}/team");
    let worker = seeded(
        &conn,
        &format!("{root}/main"),
        Some(&root),
        format!("workspace missing: {root}/build-api; the engine said: {gave_up}"),
    );
    let elsewhere = seeded(
        &conn,
        "/wt/x",
        None,
        format!("workspace missing: /wt/y; the engine said: {gave_up}"),
    );
    let bare_worker = seeded(
        &conn,
        &format!("{root}/main"),
        Some(&root),
        format!("workspace missing: {root}/review-lead"),
    );
    let bare_elsewhere = seeded(&conn, "/wt/x", None, "workspace missing: /wt/y".into());

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let class = |run: &PipelineRun| columns(&conn, run).0;
    assert_eq!(class(&bare), some("infrastructure"), "gone before a spawn");
    assert_eq!(class(&wrapped), some("infrastructure"));
    assert_eq!(class(&worker), some("infrastructure"), "a worker's tree");
    assert_eq!(class(&elsewhere), some("other"), "a path the run never had");
    assert_eq!(
        class(&bare_worker),
        some("infrastructure"),
        "a worker's tree"
    );
    assert_eq!(class(&bare_elsewhere), some("other"), "not the run's tree");
}

/// 1.6.3 could not prepare a worker when the run's tree was gone, wrapped in the
/// worker's missing tree or bare when a redispatch kept it: the machine's. A worker never made
/// because the plan left the run's tree dirty is the work's, and stays scored.
#[test]
fn v24_reads_a_worker_unprepared_for_a_run_tree_gone() {
    let mut conn = v23_store();
    let root = "/wt/team";
    let tree = format!("{root}/main");
    let seeded_bare = |conn: &Connection, error: String| {
        let mut run = failed_run("build", &error);
        run.workspace = tree.as_str().into();
        run.team_root = Some(root.into());
        let failed = failed_stage(&run, StageKind::Work, &error, false);
        seed(conn, &run, &[failed], Some(FailureClass::Other), false);
        run
    };
    let seeded = |conn: &Connection, said: String| {
        let error = format!("workspace missing: {root}/build-api; the engine said: {said}");
        seeded_bare(conn, error)
    };
    let prepare = "preparing the role's worktree failed: io: inspecting";
    let gone = seeded(
        &conn,
        format!("{prepare} {tree}: No such file or directory (os error 2)"),
    );
    let kept_worker = seeded_bare(
        &conn,
        format!("{prepare} {tree}: No such file or directory (os error 2)"),
    );
    let other_tree = seeded_bare(
        &conn,
        format!("{prepare} /wt/elsewhere: No such file or directory (os error 2)"),
    );
    let denied = seeded(
        &conn,
        format!("{prepare} {tree}: Permission denied (os error 13)"),
    );
    let dirty = seeded(
        &conn,
        format!(
            "preparing the role's worktree failed: invalid argument: the run worktree {tree} has \
             uncommitted changes to README.md; roles fork from the last commit on the run \
             branch \"archon/x\", so commit them there (or discard them) first"
        ),
    );

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let class = |run: &PipelineRun| columns(&conn, run).0;
    assert_eq!(class(&gone), some("infrastructure"), "the run's tree went");
    assert_eq!(
        class(&kept_worker),
        some("infrastructure"),
        "unwrapped, the worker's tree kept for a redispatch"
    );
    assert_eq!(class(&other_tree), some("other"), "not the run's tree");
    assert_eq!(class(&denied), some("other"), "the tree is there");
    assert_eq!(class(&dirty), some("other"), "the plan left it dirty");
}

/// A 1.6.3 run archond's spawn preflight refused, before any agent, for a
/// stage budget it could not enforce stays the unscored budget stop it was.
#[test]
fn v24_reads_a_spawn_refused_for_an_unenforceable_stage_budget() {
    let mut conn = v23_store();
    let error = "stage implement refused at spawn: model policy: cannot enforce the dollar \
        budget for codex/gpt: Aether cost reporting is unavailable or unconfirmed and \
        limits.count_estimated_cost is disabled; enable subscription estimates or choose a \
        cost-reporting model";
    let refused = failed_run("implement", error);
    let preflight = failed_stage(&refused, StageKind::Work, error, false);
    seed(
        &conn,
        &refused,
        &[preflight],
        Some(FailureClass::Budget),
        false,
    );

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(
        columns(&conn, &refused),
        (some("budget"), some("recomputed"), some("budget"))
    );
}
