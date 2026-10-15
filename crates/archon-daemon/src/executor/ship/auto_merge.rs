//! Auto-merge as an owed action (ARCHITECTURE §8.7). A delivered pull request
//! whose item allows `merge=auto` is owed `gh pr merge --auto`, recorded
//! before the delivery's outcome, until GitHub takes it: a failure another
//! try may pass waits and tries again, a refusal is the operator's blocker.
//! Delivered is not merged: the feed says which one happened.

use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId, RepoId};
use archon_core::notify::NotificationKind;
use archon_queue::Store;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::finish;
use super::remote::{self, MergeRefusal};
use crate::context::DaemonContext;

/// `kv`: `ship_automerge:<run>` → the JSON [`Owed`] auto-merge of its pull request.
const OWED_PREFIX: &str = "ship_automerge:";
/// Tries before a failure that might pass is given up as a blocker.
const MAX_TRIES: u32 = 8;
/// The wait after the first failed try; each later one waits twice as long.
const FIRST_WAIT: Duration = Duration::from_secs(60);
const MAX_WAIT: Duration = Duration::from_secs(3_600);

/// The auto-merge a run's delivered pull request is owed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Owed {
    pub item: QueueItemId,
    pub repo: RepoId,
    pub url: String,
    /// Failed tries so far.
    pub tries: u32,
    /// The next try waits for this.
    pub next_at: DateTime<Utc>,
}

fn key(run: &PipelineRunId) -> String {
    format!("{OWED_PREFIX}{run}")
}

/// Record that `url`, delivered for `run`, is owed its auto-merge.
pub(super) fn owe(
    store: &mut Store,
    run: &PipelineRunId,
    item: &QueueItemId,
    repo: &RepoId,
    url: &str,
) -> Result<(), ArchonError> {
    assert!(url.starts_with("https://"), "a pull request GitHub listed");
    let owed = Owed {
        item: item.clone(),
        repo: repo.clone(),
        url: url.to_owned(),
        tries: 0,
        next_at: archon_core::now(),
    };
    store.kv_set(&key(run), &serde_json::to_string(&owed)?)
}

/// Drop the auto-merge `run`'s pull request is still owed: the item no longer
/// allows it. One GitHub already took is not undone. Returns whether one was owed.
pub(crate) fn withdraw(store: &mut Store, run: &PipelineRunId) -> Result<bool, ArchonError> {
    let owed = store.kv_get(&key(run))?.is_some();
    if owed {
        store.kv_delete(&key(run))?;
    }
    Ok(owed)
}

/// Try every owed auto-merge that is due once; returns how many GitHub took.
pub(crate) async fn pay(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let owed = ctx.store.lock().await.kv_prefix(OWED_PREFIX)?;
    let mut taken = 0;
    for (raw_key, raw) in owed {
        let run = raw_key.strip_prefix(OWED_PREFIX).expect("ranged by prefix");
        let run = PipelineRunId::parse(run)
            .map_err(|e| ArchonError::Store(format!("owed auto-merge {raw_key}: {e}")))?;
        let owed: Owed = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Store(format!("owed auto-merge {raw_key}: {e}")))?;
        assert_eq!(raw_key, key(&run), "one key per run");
        if owed.next_at > archon_core::now() {
            continue;
        }
        if pay_one(ctx, &run, owed).await? {
            taken += 1;
        }
    }
    Ok(taken)
}

/// Ask GitHub for `owed` once and record how that ended: `true` once it took it.
async fn pay_one(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    owed: Owed,
) -> Result<bool, ArchonError> {
    assert!(owed.tries < MAX_TRIES, "an owed auto-merge has a try left");
    assert!(owed.url.contains("/pull/"), "a pull request's URL");
    let repo = match ctx
        .store
        .lock()
        .await
        .repo(&crate::resolve::repo_ref(&owed.repo))
    {
        Ok(repo) => Some(repo),
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let tried = match &repo {
        Some(repo) => {
            remote::auto_merge(&ctx.config.github.binary, &repo.path, &owed.url, None).await
        }
        None => Err(MergeRefusal::Never(
            "its repository is no longer registered".into(),
        )),
    };
    let refusal = match tried {
        Ok(()) => {
            ctx.store.lock().await.kv_delete(&key(run))?;
            let text = format!(
                "Auto-merge is on for {}: it is delivered, not merged yet; GitHub merges it once \
                 its checks and reviews pass",
                owed.url
            );
            tell(ctx, NotificationKind::Outcome, &text, &owed, run).await?;
            return Ok(true);
        }
        Err(MergeRefusal::Retry(why)) if owed.tries + 1 < MAX_TRIES => {
            wait_and_retry(ctx, run, owed, &why).await?;
            return Ok(false);
        }
        Err(MergeRefusal::Retry(why)) => format!("it failed {MAX_TRIES} times, last with: {why}"),
        Err(MergeRefusal::Never(why)) => why,
    };
    ctx.store.lock().await.kv_delete(&key(run))?;
    let text = format!(
        "Auto-merge could not be enabled for {}: {refusal}. The pull request is delivered but \
         will not merge by itself; merge it on GitHub yourself",
        owed.url
    );
    tell(ctx, NotificationKind::Blocker, &text, &owed, run).await?;
    Ok(false)
}

/// Record a failed try of `owed` and when the next one is due.
async fn wait_and_retry(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    mut owed: Owed,
    why: &str,
) -> Result<(), ArchonError> {
    owed.tries += 1;
    assert!(owed.tries < MAX_TRIES, "a try is left");
    let wait = FIRST_WAIT
        .saturating_mul(1 << (owed.tries - 1).min(16))
        .min(MAX_WAIT);
    let wait = chrono::Duration::from_std(wait).expect("an hour fits");
    owed.next_at = archon_core::now() + wait;
    tracing::warn!(%run, url = %owed.url, tries = owed.tries, %why, "auto-merge not enabled; retrying");
    ctx.store
        .lock()
        .await
        .kv_set(&key(run), &serde_json::to_string(&owed)?)
}

async fn tell(
    ctx: &DaemonContext,
    kind: NotificationKind,
    text: &str,
    owed: &Owed,
    run: &PipelineRunId,
) -> Result<(), ArchonError> {
    let item = ctx.store.lock().await.item(&owed.item);
    let item = match item {
        Ok(_) => Some(&owed.item),
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let run = item.and(Some(run));
    finish::notify(ctx, kind, text, item, run).await
}
