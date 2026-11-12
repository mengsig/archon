//! Structured provenance: the item, run and pull request a lesson came from.
use super::super::codec::SqlContext;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::lesson::{LessonRef, LessonRefKind};
use archon_core::pull_request::PullRequestRef;
use rusqlite::{Connection, OptionalExtension};

/// `item`, its newest run, and the pull request that run delivered to, as far
/// as the store still knows them.
pub(super) fn of_item(
    conn: &Connection,
    item: &QueueItemId,
) -> Result<Vec<LessonRef>, ArchonError> {
    let mut refs = vec![LessonRef {
        kind: LessonRefKind::Item,
        value: item.to_string(),
    }];
    let run: Option<String> = conn
        .query_row(
            "SELECT id FROM runs WHERE item=?1 ORDER BY started_at DESC, id DESC LIMIT 1",
            [item.as_str()],
            |r| r.get(0),
        )
        .optional()
        .ctx("read a lesson item's newest run")?;
    let Some(run) = run else {
        return Ok(refs);
    };
    let pr: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT host, repository, number FROM run_pull_requests WHERE run=?1",
            [run.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ctx("read a lesson run's pull request")?;
    refs.push(LessonRef {
        kind: LessonRefKind::Run,
        value: run,
    });
    if let Some((host, repository, number)) = pr {
        let number = u64::try_from(number)
            .map_err(|_| ArchonError::Store(format!("pull request number {number}")))?;
        let pr = PullRequestRef {
            host,
            repository,
            number,
        };
        refs.push(LessonRef {
            kind: LessonRefKind::Pr,
            value: pr.url(),
        });
    }
    assert!(refs.len() <= 3);
    assert_eq!(refs[0].kind, LessonRefKind::Item);
    Ok(refs)
}
