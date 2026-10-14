//! What became of the pull requests of finished runs: merged, closed, or sent
//! back by a reviewer. Learning counts it for or against the run (§19).

use archon_core::error::ArchonError;
use archon_core::outcome::{Correction, Outcome};
use archon_core::repo::{Repo, ShipMode};

use super::dependencies::{is_plain_branch, is_plain_url};
use super::gh::{self, Pass, PullRequest};
use crate::context::DaemonContext;

/// How long after its run finished a pull request is still asked about.
const WATCHED_DAYS: i64 = 14;

/// What one pass learned from GitHub.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Observed {
    pub merged: usize,
    pub closed: usize,
    pub changes_requested: usize,
    /// Pull requests GitHub gave no answer about; the next pass asks again.
    pub unanswered: usize,
}

struct ShippedPr {
    repo: Repo,
    branch: String,
    url: String,
}

/// Ask GitHub about the watched pull requests, with one listing of each
/// repository's open ones, and record what changed. One that cannot be read
/// is logged and asked about again in the next pass.
pub async fn observe(ctx: &DaemonContext) -> Result<Observed, ArchonError> {
    let mut seen = Observed::default();
    if !ctx.config.learning.enabled {
        return Ok(seen);
    }
    let since = archon_core::now() - chrono::Duration::days(WATCHED_DAYS);
    let waiting = ctx.store.lock().await.outcomes_awaiting_merge(&since)?;
    let mut pass = Pass::default();
    let binary = &ctx.config.github.binary;
    for outcome in &waiting {
        let Some(shipped) = shipped_pr(ctx, outcome).await? else {
            continue;
        };
        let (repo, branch) = (&shipped.repo, &shipped.branch);
        match pass.pull_request(binary, &repo.path, branch).await {
            // The branch may carry a newer pull request than the run's.
            Ok(pr) if pr.url == shipped.url => {
                gh::settle(&mut *ctx.store.lock().await, &repo.id, &pr)?;
                record(ctx, outcome, &pr, &mut seen).await?;
            }
            Ok(_) => {}
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::warn!(run = %outcome.run, %error, "pull request not observed");
                seen.unanswered += 1;
            }
        }
    }
    assert!(
        seen.merged + seen.closed + seen.changes_requested + seen.unanswered <= waiting.len(),
        "each watched run is counted once"
    );
    Ok(seen)
}

/// The pull request the run of `outcome` delivered, when it delivered one
/// into a repository that is still registered.
async fn shipped_pr(
    ctx: &DaemonContext,
    outcome: &Outcome,
) -> Result<Option<ShippedPr>, ArchonError> {
    let store = ctx.store.lock().await;
    let run = match store.run(&outcome.run) {
        Ok(run) => run,
        // Pruned with its item since the outcome was recorded.
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    assert_eq!(run.item, outcome.item, "an outcome is its run's");
    assert!(
        run.state.is_terminal(),
        "only a finished run has an outcome"
    );
    let Some(repo) = outcome.repo.as_ref().filter(|_| run.shipped) else {
        return Ok(None);
    };
    let repo = match store.repo(&crate::resolve::repo_ref(repo)) {
        Ok(repo) => repo,
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if repo.ship != ShipMode::Pr || !is_plain_branch(&run.branch) {
        return Ok(None);
    }
    let stages = store.stage_runs(&run.id)?;
    let url = archon_core::run::pr_url(stages.iter()).filter(|url| is_plain_url(url));
    Ok(url.map(|url| ShippedPr {
        repo,
        branch: run.branch,
        url: url.to_owned(),
    }))
}

async fn record(
    ctx: &DaemonContext,
    outcome: &Outcome,
    pr: &PullRequest,
    seen: &mut Observed,
) -> Result<(), ArchonError> {
    assert!(
        outcome.merged.is_none(),
        "a settled pull request is not asked about"
    );
    let store = ctx.store.lock().await;
    let sent_back = pr.review_decision.as_deref() == Some("CHANGES_REQUESTED");
    match pr.state.as_str() {
        "MERGED" => {
            store.outcome_merged(&outcome.run, true)?;
            seen.merged += 1;
        }
        "CLOSED" => {
            store.outcome_merged(&outcome.run, false)?;
            seen.closed += 1;
        }
        "OPEN" if sent_back && !outcome.corrections.contains(&Correction::ChangesRequested) => {
            store.record_correction(&outcome.run, Correction::ChangesRequested)?;
            seen.changes_requested += 1;
        }
        state => assert_eq!(state, "OPEN", "`gh` admits three states"),
    }
    Ok(())
}
