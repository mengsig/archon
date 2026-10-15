//! Deliberate whole-message commands; prose goes through cited authority.

use archon_core::autonomy::AutonomyPatch;
use archon_core::error::ArchonError;
use archon_core::protocol::{PauseTarget, RepoPatch, RepoRef, Request};

use crate::context::DaemonContext;

pub(crate) async fn parse(ctx: &DaemonContext, text: &str) -> Option<Result<Request, ArchonError>> {
    let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if !name.eq_ignore_ascii_case("/archon") {
        return None;
    }
    Some(command(ctx, rest.trim()).await)
}

async fn command(ctx: &DaemonContext, text: &str) -> Result<Request, ArchonError> {
    let (verb, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let words: Vec<_> = rest.split_whitespace().collect();
    match (verb, words.as_slice()) {
        ("undo", [seq]) => Ok(Request::UndoAuthority { change: seq.parse().map_err(|_| ArchonError::Invalid("undo needs the numeric change id from the Authority feed".into()))? }),
        ("pause", ["--hard"]) => Ok(Request::Pause(PauseTarget::Global { hard: true })),
        ("resume", ["--hard"]) => Ok(Request::Resume(PauseTarget::Global { hard: true })),
        ("repo", ["remove", name]) => Ok(Request::RepoRemove { repo: RepoRef((*name).into()) }),
        ("apply", _) => serde_json::from_str(rest).map_err(|e| ArchonError::Invalid(format!("invalid authority command: {e}"))),
        ("watcher", _) => Ok(Request::ConfigureGithubWatcher { change: serde_json::from_str(rest).map_err(|e| ArchonError::Invalid(format!("invalid watcher change: {e}")))? }),
        ("set", ["--repo", name, settings @ ..]) if !settings.is_empty() => repo_settings(ctx, name, settings).await,
        _ => Err(ArchonError::Invalid("usage: /archon set --repo NAME key=value … | pause --hard | resume --hard | repo remove NAME | watcher JSON | apply JSON".into())),
    }
}

async fn repo_settings(
    ctx: &DaemonContext,
    name: &str,
    settings: &[&str],
) -> Result<Request, ArchonError> {
    let reference = RepoRef(name.into());
    let repo = ctx.store.lock().await.repo(&reference)?;
    let mut patch = RepoPatch::default();
    let mut ceiling = serde_json::to_value(repo.autonomy_max)?;
    let mut changed_ceiling = false;
    for pair in settings {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| ArchonError::Invalid(format!("expected key=value, got {pair:?}")))?;
        if let Some(knob) = key.strip_prefix("max.") {
            let checked = AutonomyPatch::parse_list(&format!("{knob}={value}"))?;
            for (key, value) in serde_json::to_value(checked)?
                .as_object()
                .expect("a patch is an object")
            {
                ceiling[key] = value.clone();
            }
            changed_ceiling = true;
        } else {
            match key {
                "grant_merge" => {
                    patch.grant_merge = Some(value.parse().map_err(|_| {
                        ArchonError::Invalid("grant_merge must be true or false".into())
                    })?)
                }
                "ship" => patch.ship = Some(value.parse()?),
                "default_pipeline" => {
                    patch.default_pipeline = archon_core::protocol::Clearable::Set(value.into())
                }
                "base_branch" => {
                    patch.base_branch = archon_core::protocol::Clearable::Set(value.into())
                }
                _ => {
                    return Err(ArchonError::Invalid(format!(
                        "unknown repository setting {key:?}"
                    )));
                }
            }
        }
    }
    if changed_ceiling {
        patch.autonomy_max = Some(serde_json::from_value(ceiling)?);
    }
    Ok(Request::RepoUpdate {
        repo: reference,
        patch,
    })
}
