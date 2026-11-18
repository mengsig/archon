//! V24 never stops on a lesson it cannot rescore, nor on a record SQLite
//! holds as a blob: the row keeps what 1.6.3 stored and the migration goes on.

use super::outcome_classes::*;
use super::*;
use archon_core::catalog::StageKind;
use archon_core::id::LessonId;
use archon_core::outcome::FailureClass;
use archon_core::run::{PipelineRun, StageState, Verdict};

/// A failed review run 1.6.3 classed `budget`, which V24 recomputes as a
/// scored review failure.
fn budget_run(conn: &Connection) -> PipelineRun {
    let run = failed_run("review", "stage `review` failed: aether budget");
    let mut review = stage(&run, "review", StageKind::Review, StageState::Completed);
    review.verdict = Some(Verdict::Fail);
    seed(conn, &run, &[review], Some(FailureClass::Budget), false);
    observe(conn, &run, "review", 1);
    run
}

/// A [`budget_run`] and the lesson it used, which 1.6.3 left unscored.
fn used_lesson(conn: &Connection) -> (PipelineRun, LessonId) {
    let run = budget_run(conn);
    let unscored = lesson(conn, 0);
    use_and_trial(conn, &unscored, &run, None);
    (run, unscored)
}

fn set_lesson_data(conn: &Connection, lesson: &LessonId, data: &dyn rusqlite::ToSql) {
    let changed = conn
        .execute(
            "UPDATE lessons SET data = ?2 WHERE id = ?1",
            rusqlite::params![lesson.as_str(), data],
        )
        .unwrap();
    assert_eq!(changed, 1, "the lesson row exists");
}

fn lesson_json(conn: &Connection, lesson: &LessonId) -> serde_json::Value {
    let data: String = conn
        .query_row(
            "SELECT data FROM lessons WHERE id = ?1",
            [lesson.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&data).unwrap()
}

/// The stored `uses` and state columns of a lesson.
fn standing(conn: &Connection, lesson: &LessonId) -> (u32, String) {
    conn.query_row(
        "SELECT uses, state FROM lessons WHERE id = ?1",
        [lesson.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn use_won(conn: &Connection, run: &PipelineRun) -> Option<bool> {
    conn.query_row(
        "SELECT won FROM lesson_uses WHERE run = ?1",
        [run.id.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

/// A use by `run` of a lesson the store does not hold under the text `lesson`.
fn dangling_use(conn: &Connection, run: &PipelineRun, lesson: &dyn rusqlite::ToSql) {
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    conn.execute(
        "INSERT INTO lesson_uses (lesson, run, used_at, won) VALUES (?1, ?2, 't', NULL)",
        rusqlite::params![lesson, run.id.as_str()],
    )
    .unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
}

#[test]
fn v24_goes_on_over_a_lesson_it_cannot_recount() {
    let mut conn = v23_store();
    let (blob_run, blob) = used_lesson(&conn);
    set_lesson_data(&conn, &blob, &vec![0xFF_u8]);
    let (torn_run, torn) = used_lesson(&conn);
    set_lesson_data(&conn, &torn, &"not json");
    let (other_run, other) = used_lesson(&conn);
    let mut holds_another = lesson_json(&conn, &other);
    holds_another["id"] = serde_json::json!(LessonId::new());
    set_lesson_data(&conn, &other, &holds_another.to_string());
    let (crowded_run, crowded) = used_lesson(&conn);
    let mut too_many_refs = lesson_json(&conn, &crowded);
    let refs: Vec<_> = (0..archon_core::lesson::MAX_LESSON_REFS + 1)
        .map(|n| serde_json::json!({"kind": "item", "value": format!("qi_{n}")}))
        .collect();
    too_many_refs["refs"] = serde_json::json!(refs);
    set_lesson_data(&conn, &crowded, &too_many_refs.to_string());
    let (epoch_run, epoch) = used_lesson(&conn);
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?1, X'FF')",
        [format!("learning/lesson-epoch/{epoch}")],
    )
    .unwrap();
    let unnamed_run = budget_run(&conn);
    dangling_use(&conn, &unnamed_run, &vec![0xFF_u8]);
    let unknown_run = budget_run(&conn);
    dangling_use(&conn, &unknown_run, &"no lesson id");
    let gone_run = budget_run(&conn);
    dangling_use(&conn, &gone_run, &LessonId::new().as_str());
    let (whole_run, whole) = used_lesson(&conn);

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    let unread = [
        (&blob_run, "a lesson row that is a blob"),
        (&torn_run, "a lesson row that is no JSON"),
        (&other_run, "a lesson row holding another lesson"),
        (&crowded_run, "a lesson with more refs than a lesson holds"),
        (&epoch_run, "a lesson whose epoch is a blob"),
        (&unnamed_run, "a use whose lesson is a blob"),
        (&unknown_run, "a use whose lesson is no lesson id"),
        (&gone_run, "a use of a lesson that is gone"),
    ];
    for (run, why) in unread {
        assert_eq!(
            columns(&conn, run),
            (some("review"), some("recomputed"), some("budget")),
            "{why}: its run is still reclassified"
        );
        assert_eq!(use_won(&conn, run), Some(false), "{why}");
    }
    for kept in [&blob, &torn, &other, &crowded, &epoch] {
        assert_eq!(standing(&conn, kept).0, 0, "its stored counts stay");
    }
    assert_eq!(
        lesson_score(&conn, &whole_run),
        (Some(false), 1),
        "the readable lesson beside them is recounted"
    );
    assert_eq!(standing(&conn, &whole), (1, "active".to_owned()));
}

/// Five failed reviews 1.6.3 left unscored retract the lesson they used, so
/// V24 recomputes the lesson clusters of a store holding a lesson unfit for it.
#[test]
fn v24_goes_on_when_the_lesson_clusters_cannot_be_recomputed() {
    let whys = [
        "a twin holding more wins than uses",
        "a lesson row that is no JSON",
    ];
    for why in whys {
        let mut conn = v23_store();
        let losing = lesson(&conn, 0);
        let runs: Vec<PipelineRun> = (0..5).map(|_| budget_run(&conn)).collect();
        for run in &runs {
            use_and_trial(&conn, &losing, run, None);
        }
        // Two lessons of one text join a cluster, so consolidation writes both.
        let (unfit, _twin) = (lesson(&conn, 0), lesson(&conn, 0));
        if why == whys[0] {
            let mut won_more = lesson_json(&conn, &unfit);
            won_more["wins"] = serde_json::json!(3);
            set_lesson_data(&conn, &unfit, &won_more.to_string());
        } else {
            set_lesson_data(&conn, &unfit, &"not json");
        }

        assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len(), "{why}");

        assert_eq!(
            standing(&conn, &losing),
            (5, "retracted".to_owned()),
            "{why}: the lesson is still rescored"
        );
        for run in &runs {
            assert_eq!(columns(&conn, run).1, some("recomputed"), "{why}");
        }
    }
}

#[test]
fn v24_goes_on_over_records_sqlite_holds_as_blobs() {
    let mut conn = v23_store();
    let json_blob = budget_run(&conn);
    conn.execute(
        "UPDATE outcomes SET data = CAST(data AS BLOB) WHERE run = ?1",
        [json_blob.id.as_str()],
    )
    .unwrap();
    let blob = budget_run(&conn);
    conn.execute(
        "UPDATE outcomes SET data = X'FF' WHERE run = ?1",
        [blob.id.as_str()],
    )
    .unwrap();
    let item_blob = budget_run(&conn);
    conn.execute(
        "UPDATE items SET data = X'FF' WHERE id = ?1",
        [item_blob.item.as_str()],
    )
    .unwrap();
    let item_json_blob = budget_run(&conn);
    conn.execute(
        "UPDATE items SET data = CAST(?2 AS BLOB) WHERE id = ?1",
        [
            item_json_blob.item.as_str(),
            r#"{"onto": {"review_only": true, "pr": "https://github.com/o/r/pull/7"}}"#,
        ],
    )
    .unwrap();

    assert_eq!(migrate(&mut conn).unwrap(), MIGRATIONS.len());

    for run in [&json_blob, &blob, &item_blob, &item_json_blob] {
        assert_eq!(
            columns(&conn, run),
            (some("review"), some("recomputed"), some("budget"))
        );
    }
    let kept: Vec<u8> = conn
        .query_row(
            "SELECT data FROM outcomes WHERE run = ?1",
            [blob.id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, vec![0xFF], "an unreadable record is left as it was");
}
