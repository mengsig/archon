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
             VALUES (?1, 'review', 1, 'failed', '[]', 'a', 'a', '{}')",
            [item],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (id, item, pipeline, terminal, started_at, data)
             VALUES (?1, ?2, 'review', 1, 'a', ?3)",
            [run.id.as_str(), item, &serde_json::to_string(run).unwrap()],
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
         VALUES (?1, ?2, NULL, 'review', NULL, ?3, ?4, 0, NULL, 0.5, 'f', 'u', ?5)",
        rusqlite::params![
            run.id.as_str(),
            item,
            o.result.as_str(),
            old.map(FailureClass::as_str),
            serde_json::to_string(&o).unwrap()
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
        BTreeMap::from([("review", 1), ("timeout", 1)])
    );
    assert_eq!(counts.after, BTreeMap::from([("review", 2)]));
    assert_eq!(counts.legacy, BTreeMap::from([("review", 1)]));
}
