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
        ("undo", [seq]) => Ok(Request::UndoAuthority {
            change: seq.parse().map_err(|_| {
                ArchonError::Invalid(
                    "undo needs the numeric change id from the Authority feed".into(),
                )
            })?,
        }),
        ("pause", ["--hard"]) => Ok(Request::Pause(PauseTarget::Global { hard: true })),
        ("resume", ["--hard"]) => Ok(Request::Resume(PauseTarget::Global { hard: true })),
        ("repo", ["remove", name]) => Ok(Request::RepoRemove {
            repo: RepoRef((*name).into()),
        }),
        ("apply", _) => serde_json::from_str(rest)
            .map_err(|e| ArchonError::Invalid(format!("invalid authority command: {e}"))),
        ("watcher", _) => Ok(Request::ConfigureGithubWatcher {
            change: serde_json::from_str(rest)
                .map_err(|e| ArchonError::Invalid(format!("invalid watcher change: {e}")))?,
        }),
        ("reset", []) => Ok(Request::ResetSettings { repo: None }),
        ("reset", ["--repo", name]) => Ok(Request::ResetSettings {
            repo: Some(RepoRef((*name).into())),
        }),
        ("item", [id, settings @ ..]) if !settings.is_empty() => Ok(Request::UpdateItem {
            item: archon_core::id::QueueItemId::parse(id)?,
            patch: archon_core::protocol::ItemPatch {
                autonomy: Some(AutonomyPatch::parse_list(&settings.join(","))?),
                ..Default::default()
            },
        }),
        ("set", ["--repo", name, settings @ ..]) if !settings.is_empty() => {
            let is_repo_key = |s: &&str| {
                REPO_KEYS
                    .iter()
                    .any(|key| s.starts_with(&format!("{key}=")))
            };
            if settings.iter().any(is_repo_key) {
                repo_settings(ctx, name, settings).await
            } else {
                overlay_settings(ctx, Some(name), settings).await
            }
        }
        ("set", settings) if !settings.is_empty() => overlay_settings(ctx, None, settings).await,
        _ => Err(usage_error(verb)),
    }
}

const USAGE: &str = "usage: /archon answer N TEXT | approve [N] | reject [N] | merge ITEM [--ignore-checks] | set [--repo NAME] key=value … | item ID knob=value … | undo N | reset [--repo NAME] | pause --hard | resume --hard | repo remove NAME | watcher JSON | apply JSON";

const VERBS: [&str; 13] = [
    "answer", "approve", "reject", "merge", "set", "undo", "reset", "pause", "resume", "repo",
    "watcher", "apply", "item",
];

/// The usage line, led by the verb when it is not one `/archon` knows.
fn usage_error(verb: &str) -> ArchonError {
    match VERBS.contains(&verb) {
        true => ArchonError::Invalid(USAGE.into()),
        false => ArchonError::Invalid(format!("unknown verb `{verb}`; {USAGE}")),
    }
}

/// Keys `set --repo` stores in the repository registry, not the overlay.
const REPO_KEYS: [&str; 6] = [
    "grant_merge",
    "merge_waive_checks",
    "ship",
    "base_branch",
    "attribution",
    "gh_user",
];

/// Repository-scope overlay keys; the rest of the settings are global only.
const REPO_OVERLAY_KEYS: [&str; 7] = [
    "budgets_enabled",
    "budget_usd_per_item",
    "max_per_repo",
    "default_pipeline",
    "checks",
    "autonomy_default",
    "autonomy_max",
];

/// The refusal for a `set --repo` key that is neither a repository key nor
/// an overlay one, suggesting the repository key it abbreviates.
fn unknown_repo_key(key: &str) -> ArchonError {
    let guess = REPO_KEYS
        .iter()
        .find(|known| !key.is_empty() && known.starts_with(key));
    let hint = guess.map_or(String::new(), |known| format!("; did you mean `{known}`?"));
    ArchonError::Invalid(format!(
        "unknown key `{key}`{hint} (repository keys: {}; overlay keys: {}; or default.KNOB / max.KNOB)",
        REPO_KEYS.join(", "),
        REPO_OVERLAY_KEYS.join(", "),
    ))
}

/// `value` without one pair of matching quotes around it.
fn unquoted(value: &str) -> &str {
    let quoted = |q: char| value.len() >= 2 && value.starts_with(q) && value.ends_with(q);
    match quoted('"') || quoted('\'') {
        true => &value[1..value.len() - 1],
        false => value,
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
        let value = unquoted(value);
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
            _ if REPO_OVERLAY_KEYS.contains(&key) || key.contains('.') => {
                return Err(ArchonError::Invalid(format!(
                    "set {} separately from saved overlay settings such as `{key}`",
                    REPO_KEYS.join(", ")
                )));
            }
            _ => {
                return Err(unknown_repo_key(key));
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
            if repo.is_some() && !REPO_OVERLAY_KEYS.contains(&key) {
                return Err(unknown_repo_key(key));
            }
            value[key] = if matches!(key, "default_pipeline" | "checks") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_quotes_around_a_value_are_dropped() {
        assert_eq!(unquoted("\"main\""), "main");
        assert_eq!(unquoted("'main'"), "main");
        assert_eq!(unquoted("main"), "main");
        assert_eq!(unquoted("\"main"), "\"main");
        assert_eq!(unquoted("\""), "\"");
    }

    #[test]
    fn an_abbreviated_repo_key_names_the_full_one_and_lists_the_keys() {
        let refusal = unknown_repo_key("base").to_string();
        assert!(refusal.contains("unknown key `base`"), "{refusal}");
        assert!(refusal.contains("did you mean `base_branch`?"), "{refusal}");
        let unrelated = unknown_repo_key("colour").to_string();
        assert!(!unrelated.contains("did you mean"), "{unrelated}");
        for key in REPO_KEYS {
            assert!(unrelated.contains(key), "{unrelated}");
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn the_usage_lists_undo_and_reset_and_a_bogus_verb_is_named() {
        assert!(USAGE.contains("undo N") && USAGE.contains("reset [--repo NAME]"));
        let bogus = usage_error("bogus").to_string();
        assert!(
            bogus.starts_with("invalid input: unknown verb `bogus`")
                || bogus.contains("unknown verb `bogus`"),
            "{bogus}"
        );
        assert!(!usage_error("set").to_string().contains("unknown verb"));
        for verb in VERBS {
            assert!(USAGE.contains(verb), "{verb} is missing from the usage");
        }
    }
}
