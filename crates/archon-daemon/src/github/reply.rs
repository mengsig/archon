//! `github_reply`: the orchestrator's comment on a thread of a repository
//! Archon watches, posted through the outbox at most once, under archond's
//! login, and recorded in the feed (`archon log`).

use archon_core::error::ArchonError;
use archon_core::github::{GithubThread, REPLY_BUDGET, REPLY_MARKER};
use archon_core::id::OutboxToken;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{GithubKind, GithubRef};
use archon_core::repo::GH_HOST;
use archon_queue::store::{OutboxIntent, OutboxPurpose};
use tokio::time::Instant;

use super::outbox;
use super::{require_dedicated, view};
use crate::context::DaemonContext;
use crate::feed;

/// GitHub refuses comments over 65536 characters; the markers need room.
pub const MAX_BODY_CHARS: usize = 65_000;

/// `body` ready to post as `token`'s comment: non-empty, bounded, carrying
/// the token's marker and ending in exactly one reply marker, so Aether's
/// watcher never takes the comment as a new request.
pub fn reply_body(body: &str, token: &OutboxToken) -> Result<String, ArchonError> {
    let text = outbox::strip_markers(&strip_markers(body));
    let text = text.trim();
    if text.is_empty() {
        return Err(ArchonError::Invalid("a GitHub reply needs a body".into()));
    }
    let chars = text.chars().count();
    if chars > MAX_BODY_CHARS {
        return Err(ArchonError::Invalid(format!(
            "a GitHub reply holds at most {MAX_BODY_CHARS} characters, not {chars}; \
             link a report instead"
        )));
    }
    let token_marker = outbox::marker(token);
    let posted = format!("{text}\n\n{token_marker}\n{REPLY_MARKER}");
    assert!(
        posted.ends_with(REPLY_MARKER),
        "the marker closes every reply"
    );
    assert_eq!(posted.matches(REPLY_MARKER).count(), 1, "one marker");
    assert_eq!(
        posted.matches(outbox::MARKER_PREFIX).count(),
        1,
        "one token"
    );
    Ok(posted)
}

/// `body` without the marker, also where removing one marker joins the
/// halves of another; each pass shortens the text, so it ends.
fn strip_markers(body: &str) -> String {
    let mut text = body.to_owned();
    while text.contains(REPLY_MARKER) {
        text = text.replace(REPLY_MARKER, "");
    }
    assert!(!text.contains(REPLY_MARKER), "no marker survives stripping");
    text
}

/// Post `body` on `thread`, only in a repository Archon's watcher sends to
/// the orchestrator, all within [`REPLY_BUDGET`]; the comment's URL.
pub async fn post_reply(
    ctx: &DaemonContext,
    thread: &GithubThread,
    body: &str,
) -> Result<String, ArchonError> {
    require_dedicated(ctx)?;
    let token = OutboxToken::new();
    let body = reply_body(body, &token)?;
    let deadline = Instant::now() + REPLY_BUDGET;
    require_archons_repository(ctx, thread, deadline).await?;
    let intent = OutboxIntent {
        token,
        purpose: OutboxPurpose::Reply,
        item: None,
        run: None,
        target: GithubRef {
            host: GH_HOST.to_owned(),
            repo: thread.repository.as_str().to_owned(),
            number: thread.number.get(),
            kind: GithubKind::Issue,
        },
        body,
    };
    let posted = outbox::send(&ctx.store, &ctx.config.github.binary, intent, deadline).await?;
    let url = posted.url;
    let note = format!("Replied on GitHub {thread}: {url}");
    tracing::info!(%thread, %url, "posted a GitHub reply");
    if let Err(error) = feed::notify(ctx, NotificationKind::Progress, note, None, None).await {
        // Posted already: a retry would post twice, so say where it went.
        return Err(ArchonError::Corrupt(format!(
            "posted {url} but could not record it: {error}"
        )));
    }
    Ok(url)
}

/// Refuse `thread` unless its repository is one Archon's watcher sends to
/// the orchestrator, asking Aether by `deadline`.
async fn require_archons_repository(
    ctx: &DaemonContext,
    thread: &GithubThread,
    deadline: Instant,
) -> Result<(), ArchonError> {
    let client = ctx.aether.client()?;
    let status = tokio::time::timeout_at(deadline, client.github_watcher())
        .await
        .map_err(|_| {
            ArchonError::Unavailable(format!(
                "Aether did not show its GitHub watcher within {REPLY_BUDGET:?}; nothing \
                 was posted on {thread}, so retry shortly"
            ))
        })??;
    let watched = view(&status, None).repositories.into_iter().find(|r| {
        r.repository
            .eq_ignore_ascii_case(thread.repository.as_str())
    });
    match watched {
        None => Err(ArchonError::Forbidden(format!(
            "{} is not a repository Archon watches; github_reply only answers \
             threads of watched repositories",
            thread.repository
        ))),
        Some(repo) if !repo.is_archons() => Err(ArchonError::Forbidden(format!(
            "{} is watched for another agent, not Archon's orchestrator",
            thread.repository
        ))),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_marker(token: &OutboxToken) -> String {
        outbox::marker(token)
    }

    #[test]
    fn a_split_marker_is_stripped_to_one_closing_marker() {
        let token = OutboxToken::new();
        let (head, tail) = REPLY_MARKER.split_at(10);
        let split = format!("hi {head}{REPLY_MARKER}{tail} there");
        let posted = reply_body(&split, &token).unwrap();
        let marker = token_marker(&token);
        assert_eq!(posted, format!("hi  there\n\n{marker}\n{REPLY_MARKER}"));
        assert_eq!(posted.matches(REPLY_MARKER).count(), 1);
    }

    #[test]
    fn every_reply_carries_its_token_and_ends_in_exactly_one_marker() {
        let token = OutboxToken::new();
        let marker = token_marker(&token);
        let posted = reply_body("  Queued as qi_1.  ", &token).unwrap();
        assert_eq!(
            posted,
            format!("Queued as qi_1.\n\n{marker}\n{REPLY_MARKER}")
        );
        let other = token_marker(&OutboxToken::new());
        let marked = reply_body(
            &format!("{REPLY_MARKER}\nDone.{other}\n{REPLY_MARKER}"),
            &token,
        )
        .unwrap();
        assert!(
            !marked.contains(&other),
            "a pasted token is defused: {marked}"
        );
        assert_eq!(marked.matches(outbox::MARKER_PREFIX).count(), 1);
        assert!(
            marked.starts_with("Done.") && marked.ends_with(&format!("{marker}\n{REPLY_MARKER}"))
        );
    }

    #[test]
    fn empty_and_oversized_replies_are_refused() {
        let token = OutboxToken::new();
        for empty in ["", "  \n", REPLY_MARKER] {
            assert!(matches!(
                reply_body(empty, &token),
                Err(ArchonError::Invalid(_))
            ));
        }
        let long = "x".repeat(MAX_BODY_CHARS + 1);
        assert!(matches!(
            reply_body(&long, &token),
            Err(ArchonError::Invalid(_))
        ));
        assert!(reply_body(&"é".repeat(MAX_BODY_CHARS), &token).is_ok());
    }
}
