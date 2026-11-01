//! Keeping a `pr` repository's local base branch at the `origin/<base>`
//! Archon fetched, so the orchestrator's mirror, which follows the local
//! branch, does not go stale. Only a fast-forward ([`fast_forward`]); why the
//! branch stays is told the operator once, until it moves again.

use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::repo::Repo;

use super::finish;
use crate::context::DaemonContext;
use crate::worktree::fast_forward::{Advance, fast_forward};

/// `kv`: the reason last told for a repository's base staying behind, by
/// repository id and base branch.
const TOLD_KEY_PREFIX: &str = "local_base_kept_";

/// Fast-forward `repo`'s local `base` to its fetched `origin/<base>`;
/// whether it moved. A git failure is told, not fatal: items fork from
/// `origin/<base>` all the same.
pub(crate) async fn follow_origin(
    ctx: &DaemonContext,
    repo: &Repo,
    base: &str,
) -> Result<bool, ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are absolute");
    assert!(!base.is_empty(), "a base branch is named");
    let (path, branch) = (repo.path.clone(), base.to_owned());
    let advanced = tokio::task::spawn_blocking(move || fast_forward(&path, &branch))
        .await
        .map_err(|e| ArchonError::io("base fast-forward task", std::io::Error::other(e)))?;
    let moved = matches!(advanced, Ok(Advance::Moved { .. }));
    let kept = match advanced {
        Ok(Advance::Current) => None,
        Ok(Advance::Moved { from, to }) => {
            tracing::info!(repo = %repo.name, %base, %from, %to, "fast-forwarded the local base to origin");
            None
        }
        Ok(Advance::Kept(kept)) => Some((kept.kind.to_owned(), kept.why)),
        Err(error) => Some(("failed".to_owned(), error.to_string())),
    };
    tell_once(ctx, repo, base, kept).await?;
    Ok(moved)
}

/// Tell the operator why `base` stays, unless that reason was told last;
/// with none, forget the last so the next one is told again.
async fn tell_once(
    ctx: &DaemonContext,
    repo: &Repo,
    base: &str,
    kept: Option<(String, String)>,
) -> Result<(), ArchonError> {
    let key = format!("{TOLD_KEY_PREFIX}{}:{base}", repo.id);
    let mut store = ctx.store.lock().await;
    let told = store.kv_get(&key)?;
    let Some((kind, why)) = kept else {
        if told.is_some() {
            store.kv_delete(&key)?;
        }
        return Ok(());
    };
    assert!(!kind.is_empty() && !why.is_empty(), "a reason is named");
    if told.as_deref() == Some(kind.as_str()) {
        tracing::debug!(repo = %repo.name, %base, %why, "local base still not fast-forwarded");
        return Ok(());
    }
    store.kv_set(&key, &kind)?;
    drop(store);
    tracing::warn!(repo = %repo.name, %base, %why, "local base not fast-forwarded");
    let text = format!(
        "Left the local `{base}` of {} where it is: {why}. Items still fork from \
         `origin/{base}`, but the orchestrator's mirror follows the local `{base}` and stays \
         behind until it can move by fast-forward (`git merge --ff-only origin/{base}` in {}).",
        repo.name,
        repo.path.display()
    );
    finish::notify(ctx, NotificationKind::Decision, &text, None, None).await
}
