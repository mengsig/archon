//! Holdout trials: which lessons each run was given or denied, and the lift
//! measured between the two arms once each has enough scored runs.
use super::super::codec::{SqlContext, ts};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::lesson::{HoldoutLift, Lesson, held_out};
use rusqlite::{Connection, params};

/// Splits `selected` into the lessons `item` is given and those held out from
/// it. Pinned lessons are never held out.
pub(super) fn split(
    selected: Vec<Lesson>,
    item: &QueueItemId,
    rate: f64,
) -> (Vec<Lesson>, Vec<Lesson>) {
    assert!((0.0..=1.0).contains(&rate), "a holdout rate is a fraction");
    let count = selected.len();
    let (held, given): (Vec<_>, Vec<_>) = selected
        .into_iter()
        .partition(|l| !l.pinned && held_out(&l.id, item, rate));
    assert_eq!(given.len() + held.len(), count);
    (given, held)
}

/// Records one trial per holdout-eligible lesson of the run, in both arms.
/// Pinned lessons are never held out, so they are no evidence either way.
pub(super) fn record(
    conn: &Connection,
    run: &PipelineRunId,
    item: &QueueItemId,
    given: &[Lesson],
    held: &[Lesson],
) -> Result<(), ArchonError> {
    assert!(
        given.iter().all(|g| held.iter().all(|h| h.id != g.id)),
        "a lesson is given or held out, never both"
    );
    assert!(
        held.iter().all(|l| !l.pinned),
        "a pinned lesson is never held out"
    );
    let at = ts(&archon_core::now());
    let arms = given
        .iter()
        .filter(|l| !l.pinned)
        .map(|l| (l, false))
        .chain(held.iter().map(|l| (l, true)));
    for (lesson, is_held) in arms {
        conn.execute(
            "INSERT OR IGNORE INTO lesson_trials (lesson,run,item,held,at) VALUES (?1,?2,?3,?4,?5)",
            params![lesson.id.as_str(), run.as_str(), item.as_str(), is_held, at],
        )
        .ctx("record lesson trial")?;
    }
    Ok(())
}

/// Both arms' scored runs, reported only when each has the minimum sample. A
/// run counts once per arm however many of its lessons were in that arm.
pub(in crate::store) fn lift(
    conn: &Connection,
    repo: Option<&RepoId>,
) -> Result<Option<HoldoutLift>, ArchonError> {
    let mut stmt = conn
        .prepare(
            "SELECT held, COUNT(*), COALESCE(SUM(won), 0) FROM (
                 SELECT t.run, t.held, MAX(t.won) AS won FROM lesson_trials t
                 LEFT JOIN outcomes o ON o.run = t.run
                 WHERE t.won IS NOT NULL AND (?1 IS NULL OR o.repo = ?1)
                 GROUP BY t.run, t.held)
             GROUP BY held",
        )
        .ctx("prepare holdout lift")?;
    let rows = stmt
        .query_map([repo.map(RepoId::as_str)], |r| {
            Ok((
                r.get::<_, bool>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .ctx("query holdout lift")?;
    let (mut given, mut held) = ((0, 0), (0, 0));
    for row in rows {
        let (is_held, uses, wins) = row.ctx("read holdout arm")?;
        let count =
            |n: i64| u64::try_from(n).map_err(|_| ArchonError::Store(format!("trial count {n}")));
        let (uses, wins) = (count(uses)?, count(wins)?);
        assert!(wins <= uses, "only scored trials count as wins");
        if is_held {
            held = (uses, wins);
        } else {
            given = (uses, wins);
        }
    }
    Ok(HoldoutLift::measured(given.0, given.1, held.0, held.1))
}
