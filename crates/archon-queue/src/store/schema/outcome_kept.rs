//! V24 keeps the class 1.6.3 stored, and the lesson scores under it, for a run
//! whose records SQLite or recovery left unfit to class it; it never stops.

use super::outcome_classes::*;
use super::*;
use archon_core::catalog::StageKind;
use archon_core::outcome::FailureClass;
use archon_core::run::{PipelineRun, StageRun, StageState, Verdict};

/// A failed review run 1.6.3 classed `budget`, with the review stage run
/// that failed it and a lesson use 1.6.3 left unscored.
fn budget_run(conn: &Connection) -> (PipelineRun, StageRun) {
    let run = failed_run("review", "stage `review` failed: aether budget");
    let mut review = stage(&run, "review", StageKind::Review, StageState::Completed);
    review.verdict = Some(Verdict::Fail);
    seed(
        conn,
        &run,
        &[review.clone()],
        Some(FailureClass::Budget),
        false,
    );
    observe(conn, &run, "review", 1);
    let unscored = lesson(conn, 0);
    use_and_trial(conn, &unscored, &run, None);
    (run, review)
}

fn assert_kept(conn: &Connection, run: &PipelineRun, why: &str) {
    assert_eq!(
        columns(conn, run),
        (some("budget"), some("legacy_163"), some("budget")),
        "{why}"
    );
    assert_eq!(lesson_score(conn, run), (None, 0), "{why}");
}

fn set_stage_data(conn: &Connection, stage: &StageRun, data: &dyn rusqlite::ToSql) {
    let changed = conn
        .execute(
            "UPDATE stage_runs SET data = ?2 WHERE id = ?1",
            rusqlite::params![stage.id.as_str(), data],
        )
        .unwrap();
    assert_eq!(changed, 1, "the stage row exists");
}

#[test]
fn v24_keeps_a_run_whose_rows_sqlite_holds_as_another_type() {
    let mut conn = v23_store();
    let (blob, stage) = budget_run(&conn);
    set_stage_data(&conn, &stage, &vec![0xFF_u8]);
    let (not_utf8, stage) = budget_run(&conn);
    conn.execute(
        "UPDATE stage_runs SET data = CAST(X'FF' AS TEXT) WHERE id = ?1",
        [stage.id.as_str()],
    )
    .unwrap();
    let (run_blob, _) = budget_run(&conn);
    conn.execute(
        "UPDATE runs SET data = X'FF' WHERE id = ?1",
        [run_blob.id.as_str()],
    )
    .unwrap();
    let (tally_blob, stage) = budget_run(&conn);
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, X'FF')",
        [archon_core::run::review_tally_key(&stage.id)],
    )
    .unwrap();
    let (uncounted, _) = budget_run(&conn);
    conn.execute(
        "UPDATE outcome_stages SET attempts = 'many' WHERE run = ?1",
        [uncounted.id.as_str()],
    )
    .unwrap();
    let (whole, _) = budget_run(&conn);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_kept(&conn, &blob, "a stage row that is a blob");
    assert_kept(&conn, &not_utf8, "a stage row that is no UTF-8");
    assert_kept(&conn, &run_blob, "a run row that is a blob");
    assert_kept(&conn, &tally_blob, "a review tally that is a blob");
    assert_kept(&conn, &uncounted, "an observed stage with no count");
    assert_eq!(
        columns(&conn, &whole),
        (some("review"), some("recomputed"), some("budget")),
        "the readable run beside them is recomputed"
    );
    assert_eq!(lesson_score(&conn, &whole), (Some(false), 1));
}

#[test]
fn v24_keeps_a_run_whose_stage_row_decodes_into_no_stage_run() {
    let mut conn = v23_store();
    let (unattempted, mut stage) = budget_run(&conn);
    stage.attempt = 0;
    set_stage_data(&conn, &stage, &serde_json::to_string(&stage).unwrap());
    let (unnamed, mut stage) = budget_run(&conn);
    stage.stage_id = String::new();
    set_stage_data(&conn, &stage, &serde_json::to_string(&stage).unwrap());

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_kept(&conn, &unattempted, "a stage run of attempt 0");
    assert_kept(&conn, &unnamed, "a stage run of no stage");
}

#[test]
fn v24_keeps_a_run_holding_a_non_positive_attempt() {
    let mut conn = v23_store();
    let (unobserved, _) = budget_run(&conn);
    conn.execute(
        "UPDATE outcome_stages SET attempts = 0 WHERE run = ?1",
        [unobserved.id.as_str()],
    )
    .unwrap();
    let (negative, stage) = budget_run(&conn);
    conn.execute(
        "UPDATE stage_runs SET data = json_set(data, '$.attempt', -1) WHERE id = ?1",
        [stage.id.as_str()],
    )
    .unwrap();
    let (whole, _) = budget_run(&conn);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_kept(&conn, &unobserved, "an observed stage of no attempts");
    assert_kept(&conn, &negative, "a stage run of a negative attempt");
    assert_eq!(columns(&conn, &whole).1, some("recomputed"));
}

/// A row that decodes, but that the store would never write, explains no
/// class: a settled stage that never finished, or one its columns count otherwise.
#[test]
fn v24_keeps_a_run_whose_stage_row_the_store_would_not_write() {
    let mut conn = v23_store();
    let (unfinished, stage) = budget_run(&conn);
    conn.execute(
        "UPDATE stage_runs SET data = json_remove(data, '$.finished_at') WHERE id = ?1",
        [stage.id.as_str()],
    )
    .unwrap();
    let (uncounted, stage) = budget_run(&conn);
    conn.execute(
        "UPDATE stage_runs SET attempt = 0 WHERE id = ?1",
        [stage.id.as_str()],
    )
    .unwrap();
    let (miscounted, stage) = budget_run(&conn);
    conn.execute(
        "UPDATE stage_runs SET attempt = 'one' WHERE id = ?1",
        [stage.id.as_str()],
    )
    .unwrap();
    let (unnamed, stage) = budget_run(&conn);
    conn.execute(
        "UPDATE stage_runs SET stage_id = '', data = json_set(data, '$.stage_id', '')
         WHERE id = ?1",
        [stage.id.as_str()],
    )
    .unwrap();
    let (whole, _) = budget_run(&conn);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_kept(&conn, &unnamed, "a stage row with no stage id");
    assert_kept(&conn, &unfinished, "a settled stage with no finish time");
    assert_kept(&conn, &uncounted, "a stage row counted as attempt 0");
    assert_kept(&conn, &miscounted, "a stage row whose attempt is text");
    assert_eq!(columns(&conn, &whole).1, some("recomputed"));
}

/// Each column the store writes beside a record repeats it. A row holding
/// another value there, or one SQLite cannot read, is not the row it wrote.
#[test]
fn v24_keeps_a_run_whose_columns_are_not_its_records() {
    let edits = [
        ("stage_runs", "fan_index = X'FF'"),
        ("stage_runs", "fan_index = 7"),
        ("stage_runs", "stage_id = 'other'"),
        ("stage_runs", "state = X'FF'"),
        ("stage_runs", "state = 'failed'"),
        ("stage_runs", "terminal = 0"),
        ("stage_runs", "started_at = X'FF'"),
        (
            "stage_runs",
            "started_at = '2020-01-01T00:00:00.000000000Z'",
        ),
        ("stage_runs", "aether_agent = X'FF'"),
        (
            "stage_runs",
            "aether_agent = 'agt_01JB000000000000000000000R'",
        ),
        ("stage_runs", "data = json_set(data, '$.fan_index', 3)"),
        (
            "stage_runs",
            "data = json_set(data, '$.started_at', '+12345-06-01T00:00:00Z')",
        ),
        (
            "runs",
            "data = json_set(data, '$.started_at', '-0044-03-15T00:00:00Z')",
        ),
        ("runs", "pipeline = X'FF'"),
        ("runs", "pipeline = 'other'"),
        ("runs", "terminal = 0"),
        ("runs", "started_at = X'FF'"),
        ("runs", "started_at = '2020-01-01T00:00:00.000000000Z'"),
    ];
    let mut conn = v23_store();
    let mut edited = Vec::new();
    for (table, column) in edits {
        let (run, _) = budget_run(&conn);
        let key = if table == "runs" { "id" } else { "run" };
        let sql = format!("UPDATE {table} SET {column} WHERE {key} = ?1");
        assert_eq!(conn.execute(&sql, [run.id.as_str()]).unwrap(), 1, "{sql}");
        edited.push((run, format!("{table}: {column}")));
    }
    let (whole, _) = budget_run(&conn);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_eq!(edited.len(), edits.len());
    for (run, edit) in &edited {
        assert_kept(&conn, run, edit);
    }
    assert_eq!(columns(&conn, &whole).1, some("recomputed"));
}

/// 1.6.3's schema lets an outcome's key be empty: no run can be read for it.
#[test]
fn v24_keeps_a_failed_outcome_under_an_empty_run_key() {
    let mut conn = v23_store();
    let (whole, _) = budget_run(&conn);
    conn.execute(
        "INSERT INTO outcomes (run, item, repo, pipeline, size, result, failure_class, corrected,
                               merged, cost_usd, finished_at, updated_at, data)
         SELECT '', item, repo, pipeline, size, result, failure_class, corrected, merged,
                cost_usd, finished_at, updated_at, data FROM outcomes WHERE run = ?1",
        [whole.id.as_str()],
    )
    .unwrap();

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let unkeyed: (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT failure_class, class_source, failure_class_163 FROM outcomes WHERE run = ''",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        unkeyed,
        (some("budget"), some("legacy_163"), some("budget"))
    );
    assert_eq!(columns(&conn, &whole).1, some("recomputed"));
}

#[test]
fn v24_leaves_a_failed_outcome_whose_class_is_no_text() {
    let mut conn = v23_store();
    let (odd, _) = budget_run(&conn);
    conn.execute(
        "UPDATE outcomes SET failure_class = X'FF' WHERE run = ?1",
        [odd.id.as_str()],
    )
    .unwrap();
    let (whole, _) = budget_run(&conn);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let (class, source): (Vec<u8>, Option<String>) = conn
        .query_row(
            "SELECT failure_class, class_source FROM outcomes WHERE run = ?1",
            [odd.id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((class, source), (vec![0xFF], None), "the row is untouched");
    assert_eq!(lesson_score(&conn, &odd), (None, 0));
    assert_eq!(columns(&conn, &whole).1, some("recomputed"));
}

/// Recovery removes stage rows it cannot decode before it fails the run, so
/// the rows that remain may not hold what stopped it.
#[test]
fn v24_keeps_a_run_recovery_failed_after_dropping_stage_rows() {
    let mut conn = v23_store();
    let error = format!(
        "{}corrupt: decode stage run: unknown variant `budget`. `archon queue retry qi_x` \
         runs it again once fixed; `archon queue cancel qi_x` gives it up",
        archon_core::run::UNRESUMABLE_RUN
    );
    let dropped = failed_run("review", &error);
    let survivor = stage(&dropped, "review", StageKind::Work, StageState::Cancelled);
    seed(
        &conn,
        &dropped,
        &[survivor],
        Some(FailureClass::Budget),
        false,
    );
    observe(&conn, &dropped, "review", 1);
    let unscored = lesson(&conn, 0);
    use_and_trial(&conn, &unscored, &dropped, None);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    assert_kept(&conn, &dropped, "its dropped rows left no trace to count");
}
