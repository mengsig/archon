//! Deliberate whole-message commands; prose goes through cited authority.

use archon_core::autonomy::AutonomyPatch;
use archon_core::error::ArchonError;
use archon_core::protocol::{Clearable, PauseTarget, RepoPatch, RepoRef, Request};

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
        ("reset", []) => Ok(Request::ResetSettings { repo: None }),
        ("reset", ["--repo", name]) => Ok(Request::ResetSettings { repo: Some(RepoRef((*name).into())) }),
        ("item", [id, settings @ ..]) if !settings.is_empty() => Ok(Request::UpdateItem {
            item: archon_core::id::QueueItemId::parse(id)?,
            patch: archon_core::protocol::ItemPatch {
                autonomy: Some(AutonomyPatch::parse_list(&settings.join(","))?),
                ..Default::default()
            },
        }),
        ("set", ["--repo", name, settings @ ..]) if !settings.is_empty() => {
            let registry = ["grant_merge=", "merge_waive_checks=", "ship=", "base_branch=", "attribution=", "gh_user="];
            if settings.iter().any(|s| registry.iter().any(|key| s.starts_with(key))) {
                repo_settings(ctx, name, settings).await
            } else { overlay_settings(ctx, Some(name), settings).await }
        }
        ("set", settings) if !settings.is_empty() => overlay_settings(ctx, None, settings).await,
        _ => Err(ArchonError::Invalid("usage: /archon set --repo NAME key=value … | pause --hard | resume --hard | repo remove NAME | watcher JSON | apply JSON".into())),
    }
}

async fn repo_settings(
    ctx: &DaemonContext,
    name: &str,
    settings: &[&str],
) -> Result<Request, ArchonError> {
    let reference = RepoRef(name.into());
    ctx.store.lock().await.repo(&reference)?;
    let mut patch = RepoPatch::default();
    for pair in settings {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| ArchonError::Invalid(format!("expected key=value, got {pair:?}")))?;
        match key {
            "grant_merge" => {
                patch.grant_merge = Some(value.parse().map_err(|_| {
                    ArchonError::Invalid("grant_merge must be true or false".into())
                })?)
            }
            "merge_waive_checks" => patch.merge_waive_checks = Some(waived_bases(value)?),
            "ship" => patch.ship = Some(value.parse()?),
            "attribution" => patch.attribution = Some(value.parse()?),
            "gh_user" if value.is_empty() => patch.gh_user = Clearable::Clear,
            "gh_user" => patch.gh_user = Clearable::Set(value.parse()?),
            "base_branch" => patch.base_branch = Clearable::Set(value.into()),
            _ => {
                return Err(ArchonError::Invalid(
                    "set grant_merge, merge_waive_checks, ship, base_branch, attribution and gh_user \
                     separately from \
                     saved overlay settings"
                        .into(),
                ));
            }
        }
    }
    Ok(Request::RepoUpdate {
        repo: reference,
        patch,
    })
}

/// Base branch names from a comma-separated value; empty clears the list.
fn waived_bases(value: &str) -> Result<Vec<String>, ArchonError> {
    let mut bases = Vec::new();
    for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if name.starts_with('-') || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ArchonError::Invalid(format!(
                "{name:?} is not a branch name for merge_waive_checks"
            )));
        }
        if !bases.iter().any(|b| b == name) {
            bases.push(name.to_owned());
        }
    }
    Ok(bases)
}

async fn overlay_settings(
    ctx: &DaemonContext,
    name: Option<&str>,
    pairs: &[&str],
) -> Result<Request, ArchonError> {
    let repo = name.map(|name| RepoRef(name.into()));
    let id = match &repo {
        Some(repo) => Some(ctx.store.lock().await.repo(repo)?.id),
        None => None,
    };
    let mut value = serde_json::to_value(crate::settings::read(ctx, id.as_ref()).await?)?;
    for pair in pairs {
        let (key, raw) = pair
            .split_once('=')
            .ok_or_else(|| ArchonError::Invalid(format!("expected key=value, got {pair:?}")))?;
        if let Some((group, knob)) = key.split_once('.') {
            let group = match group {
                "default" => "autonomy_default",
                "max" => "autonomy_max",
                _ => {
                    return Err(ArchonError::Invalid(format!(
                        "unknown knob group {group:?}"
                    )));
                }
            };
            let patch = AutonomyPatch::parse_list(&format!("{knob}={raw}"))?;
            if value.get(group).is_none() {
                value[group] = serde_json::json!({});
            }
            for (knob, set) in serde_json::to_value(patch)?
                .as_object()
                .expect("patch is an object")
            {
                value[group][knob] = set.clone();
            }
        } else {
            value[key] = if key == "default_pipeline" {
                serde_json::json!(raw)
            } else {
                serde_json::from_str(raw)
                    .map_err(|e| ArchonError::Invalid(format!("invalid {key}: {e}")))?
            };
        }
    }
    Ok(Request::SetSettings {
        repo,
        settings: serde_json::from_value(value)?,
    })
}
