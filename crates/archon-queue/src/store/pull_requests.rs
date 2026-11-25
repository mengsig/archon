//! The pull request each run delivered to, by host, repository and number.
//! Several runs may deliver to one pull request.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::pull_request::PullRequestRef;
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, to_i64, to_u64, ts, write_tx};
use super::{github_links, runs};

/// The pull request a run delivered to, with GitHub's node id once known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPullRequest {
    pub pr: PullRequestRef,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PullRequestTimes {
    pub created_at: Option<Timestamp>,
    pub checks_started_at: Option<Timestamp>,
    pub checks_finished_at: Option<Timestamp>,
    pub merged_at: Option<Timestamp>,
}

impl Store {
    /// Record that `run` delivered to `pr`, replacing the one it delivered to
    /// before. A pull request's node id, once known, is every such run's.
    pub fn record_run_pull_request(
        &mut self,
        run: &PipelineRunId,
        pr: &PullRequestRef,
        node_id: Option<&str>,
    ) -> Result<(), ArchonError> {
        self.record_run_pull_request_with_times(run, pr, node_id, PullRequestTimes::default())
    }

    /// Record a delivery and the event times obtained with its identity.
    pub fn record_run_pull_request_with_times(
        &mut self,
        run: &PipelineRunId,
        pr: &PullRequestRef,
        node_id: Option<&str>,
        times: PullRequestTimes,
    ) -> Result<(), ArchonError> {
        assert_eq!(
            PullRequestRef::from_url(&pr.url()).as_ref(),
            Some(pr),
            "a pull request read from its URL"
        );
        if node_id.is_some_and(|id| id.trim().is_empty()) {
            return Err(ArchonError::Invalid(
                "a pull request node id is empty".into(),
            ));
        }
        let number = to_i64(pr.number, "pull request number")?;
        let tx = write_tx(&mut self.conn)?;
        let item = runs::load(&tx, run)?.item;
        let before = recorded(&tx, run)?.map(|recorded| recorded.pr);
        let changed = tx
            .execute(
                "INSERT INTO run_pull_requests (run, host, repository, number, node_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (run) DO UPDATE SET
                     node_id = CASE
                         WHEN (host, repository, number)
                             = (excluded.host, excluded.repository, excluded.number)
                         THEN COALESCE(excluded.node_id, node_id)
                         ELSE excluded.node_id END,
                     created_at = CASE WHEN (host, repository, number) = (excluded.host, excluded.repository, excluded.number) THEN created_at ELSE NULL END,
                     checks_started_at = CASE WHEN (host, repository, number) = (excluded.host, excluded.repository, excluded.number) THEN checks_started_at ELSE NULL END,
                     checks_finished_at = CASE WHEN (host, repository, number) = (excluded.host, excluded.repository, excluded.number) THEN checks_finished_at ELSE NULL END,
                     merged_at = CASE WHEN (host, repository, number) = (excluded.host, excluded.repository, excluded.number) THEN merged_at ELSE NULL END,
                     host = excluded.host, repository = excluded.repository,
                     number = excluded.number",
                params![run.as_str(), pr.host, pr.repository, number, node_id],
            )
            .ctx("record a run's pull request")?;
        assert_eq!(changed, 1, "one run, one row");
        share_known_fields(&tx, pr, number)?;
        record_times(&tx, pr, number, times)?;
        github_links::record_delivery(&tx, &item, before.as_ref(), pr)?;
        tx.commit().ctx("commit a run's pull request")
    }

    /// Merge observed event times into every delivery to this pull request.
    /// Check timing spans the earliest start and latest complete observation
    /// across watched heads and reruns.
    pub fn observe_pull_request_times(
        &mut self,
        pr: &PullRequestRef,
        times: PullRequestTimes,
    ) -> Result<(), ArchonError> {
        assert_eq!(PullRequestRef::from_url(&pr.url()).as_ref(), Some(pr));
        assert!(pr.number > 0, "a pull request has a number");
        let number = to_i64(pr.number, "pull request number")?;
        record_times(&self.conn, pr, number, times)
    }

    /// The pull request `run` delivered to, if it delivered to one.
    pub fn run_pull_request(
        &self,
        run: &PipelineRunId,
    ) -> Result<Option<RunPullRequest>, ArchonError> {
        recorded(&self.conn, run)
    }
}

fn record_times(
    conn: &Connection,
    pr: &PullRequestRef,
    number: i64,
    times: PullRequestTimes,
) -> Result<(), ArchonError> {
    assert_eq!(u64::try_from(number).ok(), Some(pr.number));
    let PullRequestTimes {
        created_at,
        checks_started_at,
        checks_finished_at,
        merged_at,
    } = times;
    let known = |at: Option<Timestamp>| at.filter(|at| at.timestamp() > 0);
    let (created_at, checks_started_at, checks_finished_at, merged_at) = (
        known(created_at),
        known(checks_started_at),
        known(checks_finished_at),
        known(merged_at),
    );
    if created_at.is_none()
        && checks_started_at.is_none()
        && checks_finished_at.is_none()
        && merged_at.is_none()
    {
        return Ok(());
    }
    conn.execute(
        "UPDATE run_pull_requests SET
            created_at = COALESCE(MIN(created_at, ?4), created_at, ?4),
            checks_started_at = COALESCE(MIN(checks_started_at, ?5), checks_started_at, ?5),
            checks_finished_at = COALESCE(MAX(checks_finished_at, ?6), checks_finished_at, ?6),
            merged_at = COALESCE(MIN(merged_at, ?7), merged_at, ?7)
         WHERE host = ?1 AND repository = ?2 AND number = ?3 AND (
            (?4 IS NOT NULL AND (created_at IS NULL OR ?4 < created_at)) OR
            (?5 IS NOT NULL AND (checks_started_at IS NULL OR ?5 < checks_started_at)) OR
            (?6 IS NOT NULL AND (checks_finished_at IS NULL OR ?6 > checks_finished_at)) OR
            (?7 IS NOT NULL AND (merged_at IS NULL OR ?7 < merged_at)))",
        params![
            pr.host,
            pr.repository,
            number,
            created_at.as_ref().map(ts),
            checks_started_at.as_ref().map(ts),
            checks_finished_at.as_ref().map(ts),
            merged_at.as_ref().map(ts)
        ],
    )
    .ctx("record pull request timestamps")?;
    Ok(())
}

fn share_known_fields(
    conn: &Connection,
    pr: &PullRequestRef,
    number: i64,
) -> Result<(), ArchonError> {
    assert!(number > 0);
    assert_eq!(u64::try_from(number).ok(), Some(pr.number));
    conn.execute(
        "UPDATE run_pull_requests SET node_id = (
                 SELECT node_id FROM run_pull_requests
                 WHERE host = ?1 AND repository = ?2 AND number = ?3 AND node_id IS NOT NULL
                 LIMIT 1)
             WHERE host = ?1 AND repository = ?2 AND number = ?3 AND node_id IS NULL",
        params![pr.host, pr.repository, number],
    )
    .ctx("share a pull request's node id")?;
    conn.execute(
            "UPDATE run_pull_requests SET
                 created_at = COALESCE(created_at, (SELECT MIN(created_at) FROM run_pull_requests WHERE host = ?1 AND repository = ?2 AND number = ?3)),
                 checks_started_at = COALESCE(checks_started_at, (SELECT MIN(checks_started_at) FROM run_pull_requests WHERE host = ?1 AND repository = ?2 AND number = ?3)),
                 checks_finished_at = COALESCE(checks_finished_at, (SELECT MAX(checks_finished_at) FROM run_pull_requests WHERE host = ?1 AND repository = ?2 AND number = ?3)),
                 merged_at = COALESCE(merged_at, (SELECT MIN(merged_at) FROM run_pull_requests WHERE host = ?1 AND repository = ?2 AND number = ?3))
             WHERE host = ?1 AND repository = ?2 AND number = ?3",
            params![pr.host, pr.repository, number],
    ).ctx("share a pull request's timestamps")?;
    Ok(())
}

/// What `run_pull_requests` holds for `run`, checked as its URL reads.
fn recorded(conn: &Connection, run: &PipelineRunId) -> Result<Option<RunPullRequest>, ArchonError> {
    let row: Option<(String, String, i64, Option<String>)> = conn
        .query_row(
            "SELECT host, repository, number, node_id FROM run_pull_requests WHERE run = ?1",
            [run.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .ctx("read a run's pull request")?;
    let Some((host, repository, number, node_id)) = row else {
        return Ok(None);
    };
    let pr = PullRequestRef {
        host,
        repository,
        number: to_u64(number, "pull request number")?,
    };
    if PullRequestRef::from_url(&pr.url()).as_ref() != Some(&pr) {
        return Err(ArchonError::Store(format!(
            "run {run} records a malformed pull request {pr}"
        )));
    }
    Ok(Some(RunPullRequest { pr, node_id }))
}
