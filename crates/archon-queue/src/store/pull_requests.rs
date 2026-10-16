//! The pull request each run delivered to, by host, repository and number.
//! Several runs may deliver to one pull request.

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::pull_request::PullRequestRef;
use rusqlite::{OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, to_i64, to_u64, write_tx};
use super::runs;

/// The pull request a run delivered to, with GitHub's node id once known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPullRequest {
    pub pr: PullRequestRef,
    pub node_id: Option<String>,
}

impl Store {
    /// Record that `run` delivered to `pr`, replacing the one it delivered to
    /// before. A node id already known for the same pull request is kept.
    pub fn record_run_pull_request(
        &mut self,
        run: &PipelineRunId,
        pr: &PullRequestRef,
        node_id: Option<&str>,
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
        runs::load(&tx, run)?;
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
                     host = excluded.host, repository = excluded.repository,
                     number = excluded.number",
                params![run.as_str(), pr.host, pr.repository, number, node_id],
            )
            .ctx("record a run's pull request")?;
        assert_eq!(changed, 1, "one run, one row");
        tx.commit().ctx("commit a run's pull request")
    }

    /// The pull request `run` delivered to, if it delivered to one.
    pub fn run_pull_request(
        &self,
        run: &PipelineRunId,
    ) -> Result<Option<RunPullRequest>, ArchonError> {
        let row: Option<(String, String, i64, Option<String>)> = self
            .conn
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
}
