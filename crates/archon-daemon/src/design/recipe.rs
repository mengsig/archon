//! A repository's saved preview recipe (docs/design-sessions.md): kept in
//! Archon's `kv` table, never in the repository, read by the design agent
//! with `design_recipe`, saved by `design_preview` once the stack answers.

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::design::recipe::{PreviewRecipe, SavedRecipe};
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::protocol::{Reply, RepoRef};
use archon_queue::store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::DesignService;
use crate::context::DaemonContext;

/// One check's connects, requests and response heads, redirects included. All run at once,
/// inside archon-mcp's 15 s `CALL_BUDGET` with the socket check; the agent's
/// own check warms a dev server that compiles the page on its first request.
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);
/// The most of a response head read for its status and `Location`.
const MAX_HEAD: usize = 16 * 1024;
/// Redirects followed before a check gives up.
const MAX_REDIRECTS: usize = 5;
/// Informational (1xx) answers skipped before the final one.
const MAX_INFORMATIONAL: usize = 5;

/// The recipe saved for `repo`; a row that no longer parses is an error,
/// so a broken record is seen rather than silently rediscovered over.
pub(crate) fn load(store: &Store, repo: &RepoId) -> Result<Option<SavedRecipe>, ArchonError> {
    let Some(raw) = store.kv_get(&archon_queue::store::keys::design_recipe(repo))? else {
        return Ok(None);
    };
    let saved: SavedRecipe = serde_json::from_str(&raw).map_err(|e| {
        ArchonError::Corrupt(format!(
            "the saved design recipe of {repo} is unreadable: {e}"
        ))
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

/// The stored row of `repo`'s recipe, as it is, to tell whether a save or a
/// clear came in between.
pub(super) fn stored_row(store: &Store, repo: &RepoId) -> Result<Option<String>, ArchonError> {
    store.kv_get(&archon_queue::store::keys::design_recipe(repo))
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

/// An HTTP answer below 500 at the loopback `url`, after its loopback
/// redirects, or why there is none.
async fn answers(url: &str) -> Result<(), String> {
    match tokio::time::timeout(CHECK_TIMEOUT, final_status(url)).await {
        Err(_) => Err(format!(
            "gave no answer within {}s (load it once yourself first if it compiles on demand)",
            CHECK_TIMEOUT.as_secs()
        )),
        Ok(Err(problem)) => Err(problem),
        Ok(Ok(status)) if status >= 500 => Err(format!("answered HTTP {status}, a server error")),
        Ok(Ok(_)) => Ok(()),
    }
}

/// The status at the end of at most [`MAX_REDIRECTS`] redirects, each of
/// which must stay on a loopback `http://` URL, so a 302 to a 500 is a 500.
async fn final_status(url: &str) -> Result<u16, String> {
    let mut url = archon_core::design::validate_preview_url(url).map_err(|e| e.to_string())?;
    for _ in 0..=MAX_REDIRECTS {
        let (status, location) = response_head(&url).await?;
        let Some(location) = location.filter(|_| (300..400).contains(&status)) else {
            return Ok(status);
        };
        let next = redirect_target(&url, &location);
        url = archon_core::design::validate_preview_url(&next)
            .map_err(|_| format!("redirects (HTTP {status}) off loopback http://"))?;
    }
    Err(format!("redirects more than {MAX_REDIRECTS} times"))
}

/// `location` resolved against the loopback `base` as RFC 3986 does:
/// absolute, scheme-relative, absolute-path, query or fragment only, or
/// relative to the base's directory.
fn redirect_target(base: &str, location: &str) -> String {
    assert!(base.starts_with("http://"), "a checked loopback URL");
    if location.contains("://") {
        return location.to_owned();
    }
    if let Some(rest) = location.strip_prefix("//") {
        return format!("http://{rest}");
    }
    let (authority, target) = split_target(base);
    let origin = format!("http://{authority}");
    if location.starts_with('/') {
        return format!("{origin}{location}");
    }
    let path = target.split(['?', '#']).next().unwrap_or("/");
    if location.starts_with('?') {
        return format!("{origin}{path}{location}");
    }
    if location.is_empty() || location.starts_with('#') {
        return format!("{origin}{target}");
    }
    let directory = &path[..=path.rfind('/').unwrap_or(0)];
    debug_assert!(directory.starts_with('/'), "an absolute directory");
    format!("{origin}{directory}{location}")
}

/// The URL's authority and its origin-form request target.
fn split_target(url: &str) -> (&str, String) {
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
    (authority, target)
}

/// One GET of the checked loopback `url`: its status and any `Location`.
async fn response_head(url: &str) -> Result<(u16, Option<String>), String> {
    let addresses = super::server_origin::endpoints(url).map_err(|e| e.to_string())?;
    let (authority, target) = split_target(url);
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
    let head = read_head(&mut stream).await?;
    let mut lines = head.lines();
    let status = status_code(lines.next().unwrap_or_default())?;
    let location = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.trim().to_owned());
    Ok((status, location))
}

/// The final response's head, past any informational (1xx) ones; a head
/// that is cut off or longer than [`MAX_HEAD`] is no answer.
async fn read_head(stream: &mut TcpStream) -> Result<String, String> {
    let mut data = Vec::with_capacity(512);
    for _ in 0..=MAX_INFORMATIONAL {
        let end = next_head_end(stream, &mut data).await?;
        let head = String::from_utf8_lossy(&data[..end]).into_owned();
        let status = status_code(head.lines().next().unwrap_or_default())?;
        if !(100..200).contains(&status) {
            return Ok(head);
        }
        data.drain(..end);
    }
    Err(format!(
        "sent more than {MAX_INFORMATIONAL} informational answers"
    ))
}

/// Read until `data` holds a whole head, and return where it ends.
async fn next_head_end(stream: &mut TcpStream, data: &mut Vec<u8>) -> Result<usize, String> {
    let mut buf = [0u8; 1024];
    loop {
        let end = data
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| at + 4);
        if end.unwrap_or(data.len()) > MAX_HEAD {
            return Err(format!("sent a response head over {MAX_HEAD} bytes"));
        }
        if let Some(end) = end {
            debug_assert!(end <= data.len(), "the head ends inside the data");
            return Ok(end);
        }
        if data.contains(&b'\n') && !data.starts_with(b"HTTP/") {
            return Err("did not answer with HTTP".to_owned());
        }
        let read = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("broke off its answer ({e})"))?;
        if read == 0 {
            return Err(if data.is_empty() || data.starts_with(b"HTTP/") {
                "closed the connection before its answer's head ended".to_owned()
            } else {
                "did not answer with HTTP".to_owned()
            });
        }
        data.extend_from_slice(&buf[..read]);
    }
}

/// The status code of a response's first line.
fn status_code(line: &str) -> Result<u16, String> {
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
