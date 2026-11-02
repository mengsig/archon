//! A review carried over to its pull request's later head (ARCHITECTURE
//! §18.6): archond's own base merges, a squash landing merged over or a
//! refresh in place, move the head without changing what was reviewed.
//! Only the item's own pull request on GitHub is carried over; what the base
//! cannot explain keeps the refusal (`crate::worktree::carried`).

use archon_core::error::ArchonError;
use archon_core::merge::{CarriedReview, PullRequestFacts};
use archon_core::pull_request::PullRequestRef;

use super::Shipped;
use crate::context::DaemonContext;
use crate::executor::gh;
use crate::worktree::carried;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack::{self, is_commit_id};

/// Which commit the item's review covers on its pull request as shown.
pub(super) enum Covered {
    /// The reviewed commit: the pull request shows it, or nothing is carried.
    Reviewed,
    /// The head the pull request shows, which the review carries over to.
    Carried(CarriedReview),
    /// The head moved and the review does not carry over, with why.
    Moved(String),
}

/// What the review of `shipped` covers on its pull request as `facts` show
/// it. A check that cannot run carries nothing over and says why.
pub(super) async fn covered(
    ctx: &DaemonContext,
    shipped: &Shipped,
    facts: &PullRequestFacts,
) -> Covered {
    assert!(
        is_commit_id(&shipped.head),
        "a reviewed head is a full commit id"
    );
    let own = shipped.review_of.is_none() && shipped.local_branch().is_none();
    if facts.head == shipped.head || facts.state != "OPEN" || !own {
        return Covered::Reviewed;
    }
    assert!(
        shipped.carried.is_none(),
        "carried from the reviewed commit"
    );
    match base_merges(ctx, shipped, facts).await {
        Ok(Ok(why)) => Covered::Carried(CarriedReview {
            reviewed: shipped.head.clone(),
            why,
        }),
        Ok(Err(why)) => Covered::Moved(why),
        Err(error) => Covered::Moved(format!("archond could not check it: {error}")),
    }
}

/// Why the review covers the pull request's head `facts` show, its base and
/// head fetched from `origin` by commit id; `Err` within why not.
async fn base_merges(
    ctx: &DaemonContext,
    shipped: &Shipped,
    facts: &PullRequestFacts,
) -> Result<Result<String, String>, ArchonError> {
    let binary = &ctx.config.github.binary;
    let shown = gh::base_commit(binary, &shipped.repo, &shipped.pr_url).await?;
    if shown.branch != facts.base {
        return Ok(Err(format!(
            "GitHub showed its base as `{}`, then as `{}`",
            facts.base, shown.branch
        )));
    }
    let Some(base) = shown.commit else {
        return Ok(Err("GitHub shows no commit for its base".into()));
    };
    let Some(pr) = PullRequestRef::from_url(&shipped.pr_url) else {
        return Ok(Err(format!("{} is not a pull request URL", shipped.pr_url)));
    };
    let head_ref = format!("refs/pull/{}/head", pr.number);
    let stacked_on = stacked_on(ctx, shipped).await?;
    let (data_dir, path) = (ctx.paths.data_dir.clone(), shipped.repo.path.clone());
    let (reviewed, head, branch) = (shipped.head.clone(), facts.head.clone(), shown.branch);
    tokio::task::spawn_blocking(move || {
        let shadow = Shadow::of(&data_dir, &path)?;
        stack::fetch(&shadow, &path, &branch, &base)?;
        stack::fetch_ref(&shadow, &path, &head_ref, &head)?;
        let covering = (&reviewed[..], &head[..]);
        carried::base_merges(&shadow, &path, covering, &base, &stacked_on)
    })
    .await
    .map_err(|e| ArchonError::io("carried review task", std::io::Error::other(e)))?
}

/// The heads of the item's stack parents its branch was last built on.
async fn stacked_on(ctx: &DaemonContext, shipped: &Shipped) -> Result<Vec<String>, ArchonError> {
    let parents = ctx.store.lock().await.stack_parents(&shipped.item.id)?;
    let heads: Vec<String> = parents.into_iter().map(|p| p.parent_sha).collect();
    if let Some(bad) = heads.iter().find(|head| !is_commit_id(head)) {
        return Err(ArchonError::Protocol(format!(
            "a stack parent of {} is recorded at {bad:?}, not a commit id",
            shipped.item.id
        )));
    }
    Ok(heads)
}
