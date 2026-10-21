//! The git identity a repository's commits are made under (ARCHITECTURE
//! §8.7), recorded when the operator registers or updates it: a ship admits
//! commits by it or by the daemon, and the daemon's merges are made by it.
//! Repository config an agent writes later changes neither.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::repo::Repo;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;
use crate::feed;
use crate::worktree::daemon_git::DaemonGit;
use crate::worktree::stack::Identity;

/// `kv`: the [`RepoIdentity`] of one repository, by id.
const KEY_PREFIX: &str = "repo_identity:";
/// The identity of the commits archond makes itself when it has no other.
pub(crate) const DAEMON_NAME: &str = "Archon";
pub(crate) const DAEMON_EMAIL: &str = "archon@localhost";

/// Who commits in a repository, as the operator's git said at registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepoIdentity {
    /// The committer, who the daemon's merges are made by.
    pub name: String,
    pub email: String,
    /// Every address a commit may carry: the author's and the committer's.
    pub emails: Vec<String>,
}

impl RepoIdentity {
    /// The addresses a shipped commit may carry, the daemon's last.
    pub(crate) fn allowed(identity: Option<&Self>) -> Vec<String> {
        let mut allowed: Vec<String> = identity.map(|i| i.emails.clone()).unwrap_or_default();
        if !allowed.iter().any(|email| email == DAEMON_EMAIL) {
            allowed.push(DAEMON_EMAIL.to_owned());
        }
        assert!(
            !allowed.is_empty(),
            "the daemon's own address is always allowed"
        );
        allowed
    }

    /// Who the daemon's merge commits are made by.
    pub(crate) fn merger(identity: Option<&Self>) -> Identity {
        let (name, email) = match identity {
            Some(found) => (found.name.clone(), found.email.clone()),
            None => (DAEMON_NAME.to_owned(), DAEMON_EMAIL.to_owned()),
        };
        assert!(!email.is_empty(), "a merge has an author");
        Identity { name, email }
    }
}

fn key(repo: &RepoId) -> String {
    format!("{KEY_PREFIX}{repo}")
}

/// What git commits as in `repo` under the operator's config: `None` when
/// it has no identity. No `GIT_*` or `EMAIL` of the daemon's stands in.
fn capture(repo: &Path) -> Result<Option<RepoIdentity>, ArchonError> {
    let Some((_, author)) = ident(repo, "GIT_AUTHOR_IDENT")? else {
        return Ok(None);
    };
    let Some((name, email)) = ident(repo, "GIT_COMMITTER_IDENT")? else {
        return Ok(None);
    };
    let mut emails = vec![author];
    if emails[0] != email {
        emails.push(email.clone());
    }
    assert!(emails.contains(&email) && emails.len() <= 2);
    Ok(Some(RepoIdentity {
        name,
        email,
        emails,
    }))
}

/// The name and address `git var <which>` prints; `None` if git has none.
fn ident(repo: &Path, which: &str) -> Result<Option<(String, String)>, ArchonError> {
    assert!(matches!(which, "GIT_AUTHOR_IDENT" | "GIT_COMMITTER_IDENT"));
    assert!(repo.is_absolute(), "registered paths are canonical");
    let ran = DaemonGit::configured_in(repo)
        .args(["var", which])
        .output()?;
    // `git var` fails exactly when git has no identity to commit under.
    if !ran.status.success() {
        return Ok(None);
    }
    let printed = ran.stdout_text()?;
    let parsed = printed
        .split_once(" <")
        .and_then(|(name, rest)| Some((name, rest.split_once('>')?.0)));
    match parsed {
        Some((name, email)) if !email.is_empty() => Ok(Some((name.to_owned(), email.to_owned()))),
        _ => Err(ArchonError::Protocol(format!(
            "git var {which} printed no name and e-mail address: {printed:?}"
        ))),
    }
}

/// Record `repo`'s identity now, as at `repo add` or `repo update`. The feed
/// hears of it only when it changed one recorded before.
pub(crate) async fn record(ctx: &DaemonContext, repo: &Repo) -> Result<(), ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are canonical");
    let path = repo.path.clone();
    let captured = tokio::task::spawn_blocking(move || capture(&path))
        .await
        .map_err(|e| {
            ArchonError::io(
                "reading the repository's git identity",
                std::io::Error::other(e),
            )
        })??;
    let before = load(ctx, &repo.id).await?;
    if captured == before {
        return Ok(());
    }
    match &captured {
        Some(identity) => {
            let raw = serde_json::to_string(identity).expect("an identity serializes");
            ctx.store.lock().await.kv_set(&key(&repo.id), &raw)?;
        }
        None => ctx.store.lock().await.kv_delete(&key(&repo.id))?,
    }
    tracing::info!(repo = %repo.name, ?captured, "repository git identity recorded");
    if before.is_none() {
        return Ok(());
    }
    feed::system(ctx, changed_text(&repo.name, captured.as_ref()))
        .await
        .map(drop)
}

/// What the feed says when `name`'s recorded identity became `now`.
fn changed_text(name: &str, now: Option<&RepoIdentity>) -> String {
    let Some(identity) = now else {
        return format!(
            "`{name}` has no git identity any more: Archon ships only its own commits there \
             until one is configured and `archon repo set` is run"
        );
    };
    assert!(!identity.emails.is_empty(), "an identity has an address");
    let emails: Vec<String> = identity.emails.iter().map(|e| format!("<{e}>")).collect();
    format!(
        "Archon now ships commits in `{name}` made as {} (and its own); merges it makes there \
         are by {} <{}>",
        emails.join(", "),
        identity.name,
        identity.email
    )
}

/// `repo`'s recorded identity. A repository registered before identities
/// were recorded has its identity recorded now, once.
pub(crate) async fn of(
    ctx: &DaemonContext,
    repo: &Repo,
) -> Result<Option<RepoIdentity>, ArchonError> {
    if let Some(found) = load(ctx, &repo.id).await? {
        return Ok(Some(found));
    }
    record(ctx, repo).await?;
    load(ctx, &repo.id).await
}

async fn load(ctx: &DaemonContext, repo: &RepoId) -> Result<Option<RepoIdentity>, ArchonError> {
    let raw = ctx.store.lock().await.kv_get(&key(repo))?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let identity: RepoIdentity = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("invalid {}: {e}", key(repo))))?;
    assert!(!identity.emails.is_empty(), "an identity has an address");
    Ok(Some(identity))
}

/// Refuse to start a run while git's identity in `repo` is no longer the one
/// recorded: its commits would be refused at ship after the work is done.
pub(crate) async fn ensure_current(ctx: &DaemonContext, repo: &Repo) -> Result<(), ArchonError> {
    assert!(repo.path.is_absolute(), "registered paths are canonical");
    let recorded = of(ctx, repo).await?;
    let path = repo.path.clone();
    let current = tokio::task::spawn_blocking(move || capture(&path))
        .await
        .map_err(|e| {
            ArchonError::io(
                "reading the repository's git identity",
                std::io::Error::other(e),
            )
        })??;
    mismatch(&repo.name, recorded.as_ref(), current.as_ref())
        .map_or(Ok(()), |why| Err(ArchonError::Invalid(why)))
}

/// Why commits made under `current` would be refused at ship against the
/// `recorded` identity, if they would.
fn mismatch(
    name: &str,
    recorded: Option<&RepoIdentity>,
    current: Option<&RepoIdentity>,
) -> Option<String> {
    assert!(!name.is_empty(), "a repository has a name");
    let allowed = RepoIdentity::allowed(recorded);
    let admitted = current.is_none_or(|now| now.emails.iter().all(|e| allowed.contains(e)));
    if admitted && (current.is_some() || recorded.is_none()) {
        return None;
    }
    let show = |identity: Option<&RepoIdentity>| match identity {
        Some(i) => format!("<{}>", i.email),
        None => format!("none (Archon's own <{DAEMON_EMAIL}>)"),
    };
    Some(format!(
        "git in `{name}` now commits as {}, but the identity Archon recorded for it is {}, so \
         the commits of a run would be refused at ship. Run `archon repo set {name}` to record \
         the current identity, or set `user.name` and `user.email` in the repository's own git \
         config first",
        show(current),
        show(recorded)
    ))
}

/// Forget an unregistered repository's identity.
pub(crate) fn forget(store: &mut archon_queue::Store, repo: &RepoId) -> Result<(), ArchonError> {
    store.kv_delete(&key(repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_always_allowed_and_merges_as_archon_without_an_identity() {
        assert_eq!(RepoIdentity::allowed(None), vec![DAEMON_EMAIL.to_owned()]);
        assert_eq!(RepoIdentity::merger(None).email, DAEMON_EMAIL);
        let known = RepoIdentity {
            name: "Op".into(),
            email: "op@example.com".into(),
            emails: vec!["author@example.com".into(), "op@example.com".into()],
        };
        let allowed = RepoIdentity::allowed(Some(&known));
        assert_eq!(allowed.last().map(String::as_str), Some(DAEMON_EMAIL));
        assert_eq!(allowed.len(), 3);
        assert_eq!(RepoIdentity::merger(Some(&known)).name, "Op");
    }

    #[test]
    fn a_changed_git_identity_is_named_before_a_run_starts() {
        let known = RepoIdentity {
            name: "Op".into(),
            email: "op@example.com".into(),
            emails: vec!["op@example.com".into()],
        };
        assert_eq!(mismatch("r", Some(&known), Some(&known)), None);
        assert_eq!(mismatch("r", None, None), None);
        let why = mismatch("r", None, Some(&known)).expect("a mismatch");
        assert!(why.contains("<op@example.com>") && why.contains(DAEMON_EMAIL));
        assert!(why.contains("archon repo set r"));
        assert!(mismatch("r", Some(&known), None).is_some());
        let renamed = RepoIdentity {
            name: "Other".into(),
            ..known.clone()
        };
        assert_eq!(mismatch("r", Some(&known), Some(&renamed)), None);
        let daemon = RepoIdentity {
            name: DAEMON_NAME.into(),
            email: DAEMON_EMAIL.into(),
            emails: vec![DAEMON_EMAIL.into()],
        };
        assert_eq!(mismatch("r", Some(&known), Some(&daemon)), None);
    }
}
