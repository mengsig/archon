//! The one way Archon comments on GitHub (ARCHITECTURE §f, vNext D3). Each
//! comment is first recorded in the store's outbox under a token its body
//! carries as `<!-- archon-outbox:<token> -->`; then posted; then marked
//! posted with GitHub's comment id. A row left pending (a failed post, or a
//! crash before the mark) is never posted blindly: [`run`] first looks for
//! the token among the thread's comments and posts only when it is absent.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::github::REPLY_BUDGET;
use archon_core::id::OutboxToken;
use archon_core::protocol::{GithubOutboxPending, GithubRef, MAX_OUTBOX_SHOWN};
use archon_core::repo::GH_HOST;
use archon_queue::store::{OutboxIntent, OutboxPurpose, OutboxRow, OutboxState, Store};
use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::context::DaemonContext;

/// Every outbox marker starts so; text an author supplies has it removed.
pub const MARKER_PREFIX: &str = "<!-- archon-outbox:";

/// A new row waits this long before [`run`] may take it, so the post its
/// writer makes at once, bounded by [`REPLY_BUDGET`], always finishes first.
const LEASE: Duration = Duration::from_secs(60);

/// The wait after a first failed attempt; it doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// How often [`run`] looks for due rows, and how many one pass takes.
const PASS_EVERY: Duration = Duration::from_secs(30);
const DUE_PER_PASS: usize = 10;

/// GitHub's clock and ours may differ: the lookup reaches back this far
/// before the row was written.
const LOOKUP_SLACK: chrono::Duration = chrono::Duration::minutes(10);

/// Most characters of `gh`'s complaint an error carries.
const MAX_ERROR_CHARS: usize = 2_000;

/// The hidden marker that finds `token`'s comment on GitHub again.
pub fn marker(token: &OutboxToken) -> String {
    format!("{MARKER_PREFIX}{token} -->")
}

/// `text` with every outbox marker defused, so no supplied text can claim
/// another row's comment; each pass shortens it, so it ends.
pub fn strip_markers(text: &str) -> String {
    let mut text = text.to_owned();
    while text.contains(MARKER_PREFIX) {
        text = text.replace(MARKER_PREFIX, "");
    }
    assert!(!text.contains(MARKER_PREFIX), "no marker survives");
    text
}

/// A comment GitHub holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub comment_id: u64,
    pub url: String,
}

/// The exponential wait after `attempts` failures, at most [`MAX_BACKOFF`].
fn backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(16);
    FIRST_BACKOFF
        .saturating_mul(1 << doublings)
        .min(MAX_BACKOFF)
}

fn later(now: Timestamp, wait: Duration) -> Timestamp {
    now + chrono::Duration::from_std(wait).expect("waits are bounded")
}

/// `owner/name#number`, as errors and status name a target.
fn target_name(target: &GithubRef) -> String {
    format!("{}#{}", target.repo, target.number)
}

/// Record `intent`, post it and mark it posted, all by `deadline`. On any
/// failure the row stays pending and [`run`] settles it; it is never dropped.
pub async fn send(
    store: &Mutex<Store>,
    gh: &Path,
    intent: OutboxIntent,
    deadline: Instant,
) -> Result<Posted, ArchonError> {
    assert!(
        intent.body.contains(&marker(&intent.token)),
        "the body carries its marker"
    );
    assert!(
        deadline <= Instant::now() + REPLY_BUDGET,
        "the post ends before the lease does"
    );
    let now = archon_core::now();
    let row = store
        .lock()
        .await
        .enqueue_outbox(&intent, &now, &later(now, LEASE))?;
    if row.intent.token != intent.token {
        return earlier_intent(&row);
    }
    let posted = post_once(store, gh, &row, deadline).await;
    posted.map_err(|error| {
        ArchonError::Unavailable(format!(
            "the comment on {} was not confirmed ({error}); archond keeps it as {} and \
             posts it at most once by itself, so never send it again",
            target_name(&row.intent.target),
            row.intent.token
        ))
    })
}

/// A ship comment of the same (item, run) recorded before this one.
fn earlier_intent(row: &OutboxRow) -> Result<Posted, ArchonError> {
    assert_eq!(
        row.intent.purpose,
        OutboxPurpose::Ship,
        "only ship rows are shared"
    );
    match &row.state {
        OutboxState::Posted {
            comment_id, url, ..
        } => Ok(Posted {
            comment_id: *comment_id,
            url: url
                .clone()
                .unwrap_or_else(|| fallback_url(&row.intent.target, *comment_id)),
        }),
        OutboxState::Pending { .. } => Err(ArchonError::Unavailable(format!(
            "this run's ship comment is already recorded as {}; archond posts it at most once",
            row.intent.token
        ))),
    }
}

/// Post `row` and mark it posted. A failed post is recorded on the row; a
/// failed mark leaves the row pending, so the next lookup finds the comment.
async fn post_once(
    store: &Mutex<Store>,
    gh: &Path,
    row: &OutboxRow,
    deadline: Instant,
) -> Result<Posted, ArchonError> {
    assert!(row.is_pending(), "only a pending row is posted");
    let posted = match create_comment(gh, &row.intent.target, &row.intent.body, deadline).await {
        Ok(posted) => posted,
        Err(error) => {
            record_failure(store, row, &error.to_string()).await?;
            return Err(error);
        }
    };
    let now = archon_core::now();
    store.lock().await.mark_outbox_posted(
        &row.intent.token,
        posted.comment_id,
        Some(&posted.url),
        &now,
    )?;
    Ok(posted)
}

async fn record_failure(
    store: &Mutex<Store>,
    row: &OutboxRow,
    error: &str,
) -> Result<(), ArchonError> {
    let OutboxState::Pending { attempts, .. } = row.state else {
        unreachable!("only a pending row fails");
    };
    let next = later(archon_core::now(), backoff(attempts + 1));
    tracing::warn!(token = %row.intent.token, %error, %next, "a GitHub comment was not confirmed");
    store
        .lock()
        .await
        .mark_outbox_failed(&row.intent.token, error, &next)
}

/// Settle one due pending row: marked posted when its marker is on GitHub,
/// else posted once. The only path that posts a row a second time.
pub async fn reconcile(
    store: &Mutex<Store>,
    gh: &Path,
    row: &OutboxRow,
) -> Result<Posted, ArchonError> {
    assert!(row.is_pending(), "only a pending row is settled");
    let deadline = Instant::now() + REPLY_BUDGET;
    let since = row.created_at - LOOKUP_SLACK;
    let found = match find_comment(gh, &row.intent.target, &row.intent.token, since, deadline).await
    {
        Ok(found) => found,
        Err(error) => {
            // Unknown whether it is there: posting now could post it twice.
            record_failure(store, row, &format!("looking for it: {error}")).await?;
            return Err(error);
        }
    };
    if let Some(posted) = found {
        let now = archon_core::now();
        store.lock().await.mark_outbox_posted(
            &row.intent.token,
            posted.comment_id,
            Some(&posted.url),
            &now,
        )?;
        return Ok(posted);
    }
    post_once(store, gh, row, Instant::now() + REPLY_BUDGET).await
}

/// The pending rows `status` lists, and how many there are.
pub async fn pending(ctx: &DaemonContext) -> Result<(Vec<GithubOutboxPending>, u64), ArchonError> {
    let (rows, total) = ctx.read_store().await?.pending_outbox(MAX_OUTBOX_SHOWN)?;
    let shown = rows
        .into_iter()
        .map(|row| {
            let OutboxState::Pending {
                attempts,
                next_attempt_at,
                last_error,
            } = row.state
            else {
                unreachable!("pending_outbox reads pending rows");
            };
            let purpose = match row.intent.purpose {
                OutboxPurpose::Reply => "reply",
                OutboxPurpose::Ship => "ship",
            };
            GithubOutboxPending {
                token: row.intent.token.to_string(),
                purpose: purpose.to_owned(),
                target: target_name(&row.intent.target),
                attempts,
                next_attempt_at,
                last_error,
                created_at: row.created_at,
            }
        })
        .collect::<Vec<_>>();
    assert!(shown.len() <= MAX_OUTBOX_SHOWN, "a bounded list");
    Ok((shown, total))
}

/// Settle every due row once; how many were settled. Only a store failure
/// is an error: a GitHub one stays on its row, backed off.
pub async fn pass(store: &Mutex<Store>, gh: &Path) -> Result<usize, ArchonError> {
    let due = store
        .lock()
        .await
        .due_outbox(&archon_core::now(), DUE_PER_PASS)?;
    assert!(due.len() <= DUE_PER_PASS, "a bounded pass");
    let mut settled = 0;
    for row in &due {
        match reconcile(store, gh, row).await {
            Ok(posted) => {
                tracing::info!(token = %row.intent.token, url = %posted.url, "settled a GitHub comment");
                settled += 1;
            }
            Err(error) if error.is_store() => return Err(error),
            Err(error) => {
                tracing::warn!(token = %row.intent.token, %error, "a GitHub comment stays pending");
            }
        }
    }
    Ok(settled)
}

/// The outbox worker: settles what a restart or a failure left pending, at
/// start and then every [`PASS_EVERY`]. Only a store failure ends it.
pub async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    loop {
        pass(&ctx.store, &ctx.config.github.binary).await?;
        tokio::time::sleep(PASS_EVERY).await;
    }
}

/// `gh api` pinned to github.com, as Aether's watcher is, whatever
/// `GH_HOST` says; `method` and the rest of `args` follow.
fn gh_api(gh: &Path, method: &str, args: &[&str]) -> Command {
    assert!(
        matches!(method, "GET" | "POST"),
        "the outbox only reads and posts"
    );
    let mut command = Command::new(gh);
    command
        .args(["api", "--hostname", GH_HOST, "--method", method])
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn comments_endpoint(target: &GithubRef) -> String {
    assert_eq!(target.host, GH_HOST, "the outbox posts on github.com only");
    format!("repos/{}/issues/{}/comments", target.repo, target.number)
}

fn fallback_url(target: &GithubRef, comment_id: u64) -> String {
    format!(
        "https://github.com/{}/issues/{}#issuecomment-{comment_id}",
        target.repo, target.number
    )
}

/// What `gh` printed, by `deadline`, with `input` on its stdin; past the
/// deadline `gh` is killed and whether it reached GitHub is unknown.
async fn run_gh(
    mut command: Command,
    what: &str,
    input: &str,
    deadline: Instant,
) -> Result<Vec<u8>, ArchonError> {
    let mut child = command
        .spawn()
        .map_err(|e| ArchonError::io(what.to_owned(), e))?;
    let mut stdin = child.stdin.take().expect("stdin was requested as a pipe");
    let input = input.to_owned();
    // Written while `gh` runs: one that stops reading cannot outlast the deadline.
    let write = async move {
        let written = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
        written
    };
    let run = async { tokio::join!(write, child.wait_with_output()) };
    let (written, output) = tokio::time::timeout_at(deadline, run).await.map_err(|_| {
        ArchonError::Unavailable(format!(
            "{what} did not finish within {REPLY_BUDGET:?} and was stopped"
        ))
    })?;
    let output = output.map_err(|e| ArchonError::io(what.to_owned(), e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let why: String = stderr.trim().chars().take(MAX_ERROR_CHARS).collect();
        return Err(ArchonError::Unavailable(format!(
            "{what} failed ({}): {why}",
            output.status
        )));
    }
    written.map_err(|e| ArchonError::io(what.to_owned(), e))?;
    Ok(output.stdout)
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: u64,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

impl Comment {
    fn posted(self, target: &GithubRef) -> Option<Posted> {
        let url = self
            .html_url
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_else(|| fallback_url(target, self.id));
        (self.id > 0).then_some(Posted {
            comment_id: self.id,
            url,
        })
    }
}

/// POST `body` as a comment on `target`; GitHub's id and URL for it.
async fn create_comment(
    gh: &Path,
    target: &GithubRef,
    body: &str,
    deadline: Instant,
) -> Result<Posted, ArchonError> {
    let endpoint = comments_endpoint(target);
    let what = format!("{} api POST {endpoint}", gh.display());
    let command = gh_api(gh, "POST", &[&endpoint, "--input", "-"]);
    let input = serde_json::json!({ "body": body }).to_string();
    let stdout = run_gh(command, &what, &input, deadline).await?;
    let comment: Option<Comment> = serde_json::from_slice(&stdout).ok();
    comment
        .and_then(|c| c.posted(target))
        .ok_or_else(|| ArchonError::Protocol(format!("{what} answered without a comment id")))
}

/// GET `target`'s comments updated since `since` and find `token`'s marker.
async fn find_comment(
    gh: &Path,
    target: &GithubRef,
    token: &OutboxToken,
    since: Timestamp,
    deadline: Instant,
) -> Result<Option<Posted>, ArchonError> {
    let endpoint = format!(
        "{}?per_page=100&since={}",
        comments_endpoint(target),
        since.format("%Y-%m-%dT%H:%M:%SZ")
    );
    let what = format!("{} api GET {endpoint}", gh.display());
    let command = gh_api(gh, "GET", &["--paginate", &endpoint]);
    let stdout = run_gh(command, &what, "", deadline).await?;
    let wanted = marker(token);
    // `--paginate` prints one array per page, back to back.
    let pages = serde_json::Deserializer::from_slice(&stdout).into_iter::<Vec<Comment>>();
    for page in pages {
        let page = page.map_err(|e| ArchonError::Protocol(format!("{what} did not parse: {e}")))?;
        let found = page
            .into_iter()
            .find(|c| c.body.as_deref().is_some_and(|b| b.contains(&wanted)));
        if let Some(comment) = found {
            let posted = comment.posted(target).ok_or_else(|| {
                ArchonError::Protocol(format!("{what} gave the marked comment no id"))
            })?;
            return Ok(Some(posted));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
