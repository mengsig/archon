use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::id::LessonId;
use archon_core::lesson::{Lesson, LessonState};
use archon_core::outcome::Outcome;
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
    for id in ids {
        conn.execute(
            "UPDATE lesson_uses SET won=?1 WHERE run=?2 AND lesson=?3",
            params![outcome.is_win(), outcome.run.as_str(), id],
        )
        .ctx("score lesson use")?;
        let mut lesson = super::load(conn, &LessonId::parse(&id)?)?;
        let epoch = super::super::settings::kv_get(conn, &format!("learning/lesson-epoch/{id}"))?;
        let (uses, wins): (u32, u32) = conn
            .query_row(
                "SELECT COUNT(won),COALESCE(SUM(won),0) FROM lesson_uses WHERE lesson=?1 AND (?2 IS NULL OR used_at>=?2)",
                params![id,epoch],
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
        refresh(&mut lesson, archon_core::now());
        super::write(conn, &lesson)?;
    }
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
