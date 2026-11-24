use super::super::codec::{SqlContext, text_column};
use archon_core::error::ArchonError;
use archon_core::id::LessonId;
use archon_core::lesson::{Lesson, LessonRetirement, LessonSource, LessonState, MAX_LESSON_REFS};
use archon_core::notify::FeedEntry;
use archon_core::outcome::{FailureClass, Outcome, OutcomeResult, UNSCORED_FAILURES};
use rusqlite::{Connection, params};
use std::collections::BTreeSet;

pub(in crate::store) fn score_run(
    conn: &Connection,
    outcome: &Outcome,
    min_uses: u32,
) -> Result<(), ArchonError> {
    assert!(outcome.check().is_ok());
    let mut stmt = conn
        .prepare("SELECT lesson FROM lesson_uses WHERE run=?1")
        .ctx("prepare used lessons")?;
    let ids = stmt
        .query_map([outcome.run.as_str()], |r| r.get::<_, String>(0))
        .ctx("query used lessons")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read used lessons")?;
    // An unscored use keeps `won` NULL, so it counts as neither a use nor a win.
    let won = outcome.is_scored().then(|| outcome.is_win());
    assert!(
        !outcome.is_win() || won == Some(true),
        "a win is always scored"
    );
    // Both holdout arms take the run's score; a held-out lesson's own counts never do.
    conn.execute(
        "UPDATE lesson_trials SET won=?1 WHERE run=?2",
        params![won, outcome.run.as_str()],
    )
    .ctx("score lesson trials")?;
    for id in ids {
        conn.execute(
            "UPDATE lesson_uses SET won=?1 WHERE run=?2 AND lesson=?3",
            params![won, outcome.run.as_str(), id],
        )
        .ctx("score lesson use")?;
        score_lesson(conn, outcome, &LessonId::parse(&id)?, min_uses)?;
    }
    Ok(())
}

/// Re-scores the lesson uses and holdout trials of failed runs whose class
/// V24 recomputed (`scored` per run), then recounts the lessons they used. A
/// failed run never wins, so a use is a loss or, when unscored, `NULL`.
/// Returns how many lessons were recounted: one whose rows cannot be read
/// keeps its stored counts, so that no row stops the migration.
pub(in crate::store) fn rescore_failed_runs(
    conn: &Connection,
    runs: &[(String, bool)],
) -> Result<usize, ArchonError> {
    let mut touched = BTreeSet::new();
    for (run, scored) in runs {
        assert!(!run.is_empty(), "a rescored run is named");
        let won = scored.then_some(false);
        conn.execute(
            "UPDATE lesson_uses SET won=?1 WHERE run=?2",
            params![won, run],
        )
        .ctx("rescore lesson uses")?;
        conn.execute(
            "UPDATE lesson_trials SET won=?1 WHERE run=?2",
            params![won, run],
        )
        .ctx("rescore lesson trials")?;
        touched.extend(used_lessons(conn, run)?);
    }
    let mut recounted = 0;
    for id in &touched {
        let what = format!("lesson {id} keeps its stored counts");
        if or_skipped(conn, &what, || recount_stored(conn, id))?.is_some() {
            recounted += 1;
        }
    }
    assert!(recounted <= touched.len(), "a lesson is recounted once");
    Ok(recounted)
}

/// The lessons `run` used. A use whose lesson is not text names none.
fn used_lessons(conn: &Connection, run: &str) -> Result<Vec<String>, ArchonError> {
    let mut stmt = conn
        .prepare_cached("SELECT lesson FROM lesson_uses WHERE run=?1")
        .ctx("prepare rescored lessons")?;
    let rows = stmt
        .query_map([run], |r| text_column(r, 0))
        .ctx("query rescored lessons")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ctx("read rescored lessons")?;
    let stored = rows.len();
    let named: Vec<String> = rows.into_iter().flatten().collect();
    if named.len() < stored {
        let skipped = stored - named.len();
        tracing::warn!(
            run,
            skipped,
            "lesson uses that name no lesson are not recounted"
        );
    }
    Ok(named)
}

/// `step` in a savepoint: `None`, with its writes undone, when it fails on
/// anything but a full disk, so that one unreadable row never stops V24.
fn or_skipped<T>(
    conn: &Connection,
    what: &str,
    step: impl FnOnce() -> Result<T, ArchonError>,
) -> Result<Option<T>, ArchonError> {
    assert!(!what.is_empty(), "a skipped step says what it leaves");
    conn.execute_batch("SAVEPOINT rescore")
        .ctx("open a rescoring savepoint")?;
    let done = step();
    let end = match &done {
        Ok(_) => "RELEASE rescore",
        Err(_) => "ROLLBACK TO rescore; RELEASE rescore",
    };
    conn.execute_batch(end).ctx("close a rescoring savepoint")?;
    match done {
        Ok(value) => Ok(Some(value)),
        Err(error @ ArchonError::DiskFull(_)) => Err(error),
        Err(error) => {
            tracing::warn!(%error, "{what}: its rows cannot be rescored");
            Ok(None)
        }
    }
}

/// Rebuilds the stored counts during V24, before live learning is configured.
fn recount_stored(conn: &Connection, id: &str) -> Result<(), ArchonError> {
    let mut lesson = super::load(conn, &LessonId::parse(id)?)?;
    if lesson.id.as_str() != id {
        let what = format!("lesson row {id} holds {}", lesson.id);
        return Err(ArchonError::Corrupt(what));
    }
    let before = (lesson.uses, lesson.wins);
    recount(conn, &mut lesson)?;
    assert!(lesson.wins <= lesson.uses);
    if (lesson.uses, lesson.wins) == before {
        return Ok(());
    }
    check_writable(&lesson)?;
    let rows = conn
        .execute(
            "UPDATE lessons SET uses=?2, wins=?3,
                 data=json_set(data, '$.uses', ?2, '$.wins', ?3) WHERE id=?1",
            params![id, lesson.uses, lesson.wins],
        )
        .ctx("write recounted lesson counts")?;
    assert_eq!(rows, 1, "the recounted lesson came from this row");
    Ok(())
}

/// Rejects a damaged lesson before updating its JSON.
fn check_writable(lesson: &Lesson) -> Result<(), ArchonError> {
    let counted = lesson.wins <= lesson.uses;
    let confident = lesson.confidence.is_finite() && (0.0..=1.0).contains(&lesson.confidence);
    if counted && confident && lesson.refs.len() <= MAX_LESSON_REFS {
        return Ok(());
    }
    let what = format!(
        "lesson {} holds counts, a confidence or refs no lesson has",
        lesson.id
    );
    Err(ArchonError::Corrupt(what))
}

fn score_lesson(
    conn: &Connection,
    outcome: &Outcome,
    id: &LessonId,
    min_uses: u32,
) -> Result<(), ArchonError> {
    let mut lesson = super::load(conn, id)?;
    let before = lesson.state;
    recount(conn, &mut lesson)?;
    if outcome.is_win()
        && lesson
            .confirmed_at
            .is_none_or(|at| at < outcome.finished_at)
    {
        lesson.confirmed_at = Some(outcome.finished_at);
    }
    if outcome.is_win()
        && lesson.state == LessonState::Proposed
        && lesson.source == LessonSource::Orchestrator
    {
        lesson.state = LessonState::Active;
    }
    let at = archon_core::now();
    refresh(conn, &mut lesson, at, min_uses)?;
    let (scope, changed) = (lesson.scope.clone(), lesson.state != before);
    save_scored(conn, before, lesson, at)?;
    if changed {
        super::consolidation::consolidate(conn, &scope)?;
    }
    Ok(())
}

/// Sets a lesson's uses and wins from its scored uses since its epoch.
pub(super) fn recount(conn: &Connection, lesson: &mut Lesson) -> Result<(), ArchonError> {
    let id = lesson.id.as_str();
    let epoch = super::super::settings::kv_get(conn, &format!("learning/lesson-epoch/{id}"))?;
    let [budget, timeout, infrastructure] = UNSCORED_FAILURES.map(FailureClass::as_str);
    // Before 1.4.7 an unscored run's use was stored as a loss: the outcome
    // join leaves those out too, so an upgraded store counts them as now.
    let (uses, wins): (u32, u32) = conn
        .query_row(
            "SELECT COUNT(u.won),COALESCE(SUM(u.won),0) FROM lesson_uses u
             LEFT JOIN outcomes o ON o.run=u.run
             WHERE u.lesson=?1 AND (?2 IS NULL OR u.used_at>=?2)
             AND (o.run IS NULL OR (o.result<>?3
                  AND COALESCE(o.failure_class,'') NOT IN (?4,?5,?6)))",
            params![
                id,
                epoch,
                OutcomeResult::Cancelled.as_str(),
                budget,
                timeout,
                infrastructure
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ctx("count lesson uses")?;
    assert!(
        wins <= uses,
        "a lesson cannot win more runs than it was used in"
    );
    lesson.uses = uses;
    lesson.wins = wins;
    Ok(())
}

/// Writes a lesson scoring changed. A state change scoring made by itself is
/// stamped and posted to the feed, so no lesson is activated or retracted silently.
pub(super) fn save_scored(
    conn: &Connection,
    before: LessonState,
    mut lesson: Lesson,
    at: archon_core::Timestamp,
) -> Result<(), ArchonError> {
    let changed = lesson.state != before;
    if changed {
        lesson.updated_at = at;
    }
    super::write(conn, &lesson)?;
    if !changed {
        return Ok(());
    }
    let verb = match lesson.state {
        LessonState::Active => "activated",
        LessonState::Retracted => "retired",
        LessonState::Proposed => unreachable!("scoring never proposes a lesson"),
    };
    assert!(lesson.wins <= lesson.uses);
    let text = format!(
        "Lesson {} {verb} automatically: {} of {} scored runs won. {}",
        lesson.id,
        lesson.wins,
        lesson.uses,
        serde_json::to_string(&lesson.text)?
    );
    super::super::feed::append_in(
        conn,
        FeedEntry::System {
            text,
            command_id: None,
        },
        at,
    )?;
    Ok(())
}

pub(super) fn refresh(
    conn: &Connection,
    lesson: &mut Lesson,
    at: archon_core::Timestamp,
    min_uses: u32,
) -> Result<(), ArchonError> {
    refresh_confidence(lesson, at);
    assert!(min_uses > 0);
    if lesson.pinned
        || lesson.source == LessonSource::Operator
        || lesson.state == LessonState::Retracted
        || lesson.uses < min_uses
    {
        return Ok(());
    }
    let baseline = super::holdout::baseline(conn, lesson, min_uses)?.unwrap_or(0.5);
    assert!((0.0..=1.0).contains(&baseline));
    let win_rate = f64::from(lesson.wins) / f64::from(lesson.uses);
    if win_rate <= baseline {
        lesson.state = LessonState::Retracted;
        lesson.retirement = Some(LessonRetirement { min_uses, baseline });
    }
    Ok(())
}

pub(super) fn refresh_confidence(lesson: &mut Lesson, at: archon_core::Timestamp) {
    assert!(lesson.wins <= lesson.uses);
    let base = if lesson.uses == 0 {
        match lesson.source {
            LessonSource::Operator => 1.0,
            LessonSource::Orchestrator if lesson.state == LessonState::Proposed => 0.15,
            _ => 0.5,
        }
    } else {
        wilson(lesson.wins, lesson.uses)
    };
    let days = (at
        - lesson
            .last_used_at
            .or(lesson.confirmed_at)
            .unwrap_or(lesson.created_at))
    .num_days()
    .max(0) as f64;
    lesson.confidence = base
        * if lesson.pinned {
            1.0
        } else {
            2.0_f64.powf(-days / 30.0)
        };
    assert!((0.0..=1.0).contains(&lesson.confidence));
}

fn wilson(wins: u32, uses: u32) -> f64 {
    assert!(uses > 0);
    assert!(wins <= uses);
    let n = f64::from(uses);
    let p = f64::from(wins) / n;
    let z2 = 1.96_f64.powi(2);
    ((p + z2 / (2.0 * n) - 1.96 * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt()) / (1.0 + z2 / n))
        .max(0.0)
}
