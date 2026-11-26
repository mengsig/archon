//! A repository's saved preview recipe (docs/design-sessions.md): kept in
//! Archon's `kv` table, never in the repository, read by the design agent
//! with `design_recipe`, saved by `design_preview` once the stack answers.

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::design::recipe::{PreviewRecipe, SavedRecipe};
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::protocol::{RepoRef, Reply};
use archon_queue::store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::DesignService;
use crate::context::DaemonContext;

/// One check's connect, request and status line together. All run at once,
/// inside archon-mcp's 15 s `CALL_BUDGET` with the socket check; the agent's
/// own check warms a dev server that compiles the page on its first request.
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);
/// A status line is far shorter; more is not an HTTP answer.
const MAX_STATUS_LINE: usize = 1024;

/// The recipe saved for `repo`; a row that no longer parses is an error,
/// so a broken record is seen rather than silently rediscovered over.
pub(crate) fn load(store: &Store, repo: &RepoId) -> Result<Option<SavedRecipe>, ArchonError> {
    let Some(raw) = store.kv_get(&archon_queue::store::keys::design_recipe(repo))? else {
        return Ok(None);
    };
    let saved: SavedRecipe = serde_json::from_str(&raw).map_err(|e| {
        ArchonError::Corrupt(format!("the saved design recipe of {repo} is unreadable: {e}"))
    })?;
    saved.recipe.check().map_err(|e| {
        ArchonError::Corrupt(format!("the saved design recipe of {repo} is invalid: {e}"))
    })?;
    Ok(Some(saved))
}

/// Replace `repo`'s recipe with `recipe`, checked again here: the store
/// never holds one that could carry a secret value.
pub(crate) fn save(
    store: &mut Store,
    repo: &RepoId,
    item: &QueueItemId,
    recipe: PreviewRecipe,
) -> Result<SavedRecipe, ArchonError> {
    recipe.check().map_err(ArchonError::Invalid)?;
    let saved = SavedRecipe {
        recipe,
        saved_at: archon_core::now(),
        item: item.clone(),
    };
    let raw = serde_json::to_string(&saved)?;
    store.kv_set(&archon_queue::store::keys::design_recipe(repo), &raw)?;
    debug_assert_eq!(load(store, repo)?.as_ref(), Some(&saved));
    Ok(saved)
}

/// Forget `repo`'s recipe; true when there was one.
pub(crate) fn clear(store: &mut Store, repo: &RepoId) -> Result<bool, ArchonError> {
    let key = archon_queue::store::keys::design_recipe(repo);
    let existed = store.kv_get(&key)?.is_some();
    store.kv_delete(&key)?;
    debug_assert!(store.kv_get(&key)?.is_none(), "the recipe is gone");
    Ok(existed)
}

/// `clear_design_recipe`: the operator forgets a repository's recipe, then
/// sees its settings as `archon settings --repo` shows them.
pub(crate) async fn clear_for(ctx: &DaemonContext, repo: &RepoRef) -> Result<bool, ArchonError> {
    let mut store = ctx.store.lock().await;
    let id = store.repo(repo)?.id;
    let existed = clear(&mut store, &id)?;
    tracing::info!(repo = %id, existed, "design: the saved preview recipe was cleared");
    Ok(existed)
}

/// The page answers without a server error, and so does every step of
/// `recipe` with a readiness check or a port. Every failure is named.
pub(crate) async fn check_stack(
    url: &str,
    recipe: Option<&PreviewRecipe>,
) -> Result<(), ArchonError> {
    let steps = recipe.map(|r| r.steps.as_slice()).unwrap_or_default();
    let page = async {
        answers(url)
            .await
            .map_err(|problem| format!("the preview page {url} {problem}"))
    };
    let checks = steps.iter().map(|step| async move {
        let outcome = match (&step.ready, step.port) {
            (Some(ready), _) => answers(ready).await.map_err(|p| format!("{ready} {p}")),
            (None, Some(port)) => listens(port).await,
            (None, None) => Ok(()),
        };
        outcome.map_err(|problem| format!("step {}: {problem}", step.name))
    });
    let (page, steps_done) = tokio::join!(page, futures_util::future::join_all(checks));
    assert_eq!(steps_done.len(), steps.len(), "one outcome per step");
    let failures: Vec<String> = std::iter::once(page)
        .chain(steps_done)
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "the preview stack is not up, so nothing was previewed or saved: {}. \
         Start what is missing, check it answers, then call design_preview again",
        failures.join("; ")
    )))
}

/// An HTTP answer below 500 at the loopback `url`, or why there is none.
async fn answers(url: &str) -> Result<(), String> {
    match tokio::time::timeout(CHECK_TIMEOUT, status_of(url)).await {
        Err(_) => Err(format!(
            "gave no answer within {}s (load it once yourself first if it compiles on demand)",
            CHECK_TIMEOUT.as_secs()
        )),
        Ok(Err(problem)) => Err(problem),
        Ok(Ok(status)) if status >= 500 => Err(format!("answered HTTP {status}, a server error")),
        Ok(Ok(_)) => Ok(()),
    }
}

async fn status_of(url: &str) -> Result<u16, String> {
    let url = archon_core::design::validate_preview_url(url).map_err(|e| e.to_string())?;
    let addresses = super::server_origin::endpoints(&url).map_err(|e| e.to_string())?;
    let rest = &url["http://".len()..];
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(split);
    let target = target.split('#').next().unwrap_or_default();
    let target = match target {
        "" => "/".to_owned(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_owned(),
    };
    assert!(target.starts_with('/'), "an origin-form request target");
    let mut stream = TcpStream::connect(addresses.as_slice())
        .await
        .map_err(|e| format!("does not answer ({e})"))?;
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {authority}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("closed the connection ({e})"))?;
    status_line(&mut stream).await
}

/// The status code of the response's first line.
async fn status_line(stream: &mut TcpStream) -> Result<u16, String> {
    let mut head = Vec::with_capacity(128);
    let mut buf = [0u8; 256];
    while !head.contains(&b'\n') {
        let read = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("broke off its answer ({e})"))?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buf[..read]);
        if head.len() > MAX_STATUS_LINE {
            break;
        }
    }
    let line = String::from_utf8_lossy(&head);
    let line = line.lines().next().unwrap_or_default();
    let mut words = line.split_whitespace();
    let version = words.next().unwrap_or_default();
    let code = words.next().and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(code) if version.starts_with("HTTP/") && (100..600).contains(&code) => Ok(code),
        _ => Err("did not answer with HTTP".to_owned()),
    }
}

async fn listens(port: u16) -> Result<(), String> {
    assert_ne!(port, 0, "recipes refuse port 0");
    let addresses = [
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    ];
    match tokio::time::timeout(CHECK_TIMEOUT, TcpStream::connect(&addresses[..])).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("nothing listens on loopback port {port} ({e})")),
        Err(_) => Err(format!(
            "loopback port {port} did not accept within {}s",
            CHECK_TIMEOUT.as_secs()
        )),
    }
}

/// The repository of the item a design session belongs to.
pub(super) async fn repo_of(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<RepoId, ArchonError> {
    ctx.store.lock().await.item(item)?.repo.ok_or_else(|| {
        ArchonError::Invalid("a design session's item belongs to a repository".into())
    })
}

impl DesignService {
    /// `design_recipe`: the recipe saved for the caller's repository, in words.
    pub(crate) async fn recipe(
        &self,
        ctx: &DaemonContext,
        caller: Option<&AetherAgentId>,
    ) -> Result<Reply, ArchonError> {
        let session = self.agent_session(ctx, caller).await?;
        let repo = repo_of(ctx, &session.item).await?;
        let saved = load(&*ctx.store.lock().await, &repo)?;
        let message = match saved {
            Some(saved) => format!(
                "Saved preview recipe for this repository. Try it first; if a step fails, \
                 rediscover the route and pass the corrected recipe to design_preview.\n{}\n\
                 JSON: {}",
                saved.describe(),
                serde_json::to_string(&saved.recipe)?
            ),
            None => "No preview recipe is saved for this repository yet. Discover how to \
                     start its preview stack, then pass the recipe to design_preview."
                .to_owned(),
        };
        Ok(Reply::DesignAgent { message })
    }
}

#[cfg(test)]
mod tests;
