use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::id::LessonId;
use archon_core::lesson::{Lesson, LessonState};
use archon_core::notify::FeedEntry;
use archon_core::outcome::{FailureClass, Outcome, OutcomeResult, UNSCORED_FAILURES};
use rusqlite::{Connection, params};

pub(in crate::store) fn score_run(conn: &Connection, outcome: &Outcome) -> Result<(), ArchonError> {
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
    for id in ids {
        conn.execute(
            "UPDATE lesson_uses SET won=?1 WHERE run=?2 AND lesson=?3",
            params![won, outcome.run.as_str(), id],
        )
        .ctx("score lesson use")?;
        score_lesson(conn, outcome, &LessonId::parse(&id)?)?;
    }
    Ok(())
}

fn score_lesson(conn: &Connection, outcome: &Outcome, id: &LessonId) -> Result<(), ArchonError> {
    let mut lesson = super::load(conn, id)?;
    let before = lesson.state;
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
                id.as_str(),
                epoch,
                OutcomeResult::Cancelled.as_str(),
                budget,
                timeout,
                infrastructure
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ctx("count lesson uses")?;
    assert!(wins <= uses);
    lesson.uses = uses;
    lesson.wins = wins;
    if outcome.is_win()
        && lesson
            .confirmed_at
            .is_none_or(|at| at < outcome.finished_at)
    {
        lesson.confirmed_at = Some(outcome.finished_at);
    }
    if outcome.is_win()
        && lesson.state == LessonState::Proposed
        && lesson.source == archon_core::lesson::LessonSource::Orchestrator
    {
        lesson.state = LessonState::Active;
    }
    let at = archon_core::now();
    refresh(&mut lesson, at);
    save_scored(conn, before, lesson, at)
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
        LessonState::Retracted => "retracted",
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
    super::super::feed::append_in(conn, FeedEntry::System { text }, at)?;
    Ok(())
}

pub(super) fn refresh(lesson: &mut Lesson, at: archon_core::Timestamp) {
    assert!(lesson.wins <= lesson.uses);
    let base = if lesson.uses == 0 {
        match lesson.source {
            archon_core::lesson::LessonSource::Operator => 1.0,
            archon_core::lesson::LessonSource::Orchestrator
                if lesson.state == LessonState::Proposed =>
            {
                0.15
            }
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
    if !lesson.pinned && lesson.uses >= 5 && wilson(lesson.wins, lesson.uses) < 0.3 {
        lesson.state = LessonState::Retracted;
    }
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
