//! `archon repo add|list|remove|set`.

use std::process::ExitCode;

use anyhow::Context as _;
use archon_core::autonomy::{AutonomyPatch, MergeMode};
use archon_core::protocol::{Clearable, Reply, RepoAdd, RepoPatch, RepoRef, Request};
use archon_core::repo::{GhUser, ShipMode};

use super::{emit, to_json};
use crate::cli::{RepoCommand, Ship};
use crate::connect::{connect_or_start, connect_running, unexpected};
use crate::format;
use crate::views;

pub async fn run(command: RepoCommand, json: bool) -> anyhow::Result<ExitCode> {
    match command {
        RepoCommand::Add {
            path,
            name,
            ship,
            base,
            pipeline,
            auto_merge,
            github,
        } => {
            // The daemon may run with another working directory.
            let path = path
                .canonicalize()
                .with_context(|| format!("resolving {}", path.display()))?;
            let add = RepoAdd {
                path,
                name,
                ship: ship.map(ship_mode),
                base_branch: base,
                default_pipeline: pipeline,
                auto_merge,
                github_watch: github,
            };
            add_repo(add, json).await?;
        }
        RepoCommand::List => {
            let repos = views::fetch_repos(&connect_running().await?).await?;
            emit(json, || to_json(&repos), &format::repos(&repos))?;
        }
        RepoCommand::Remove { repo } => {
            let client = connect_or_start().await?;
            let request = Request::RepoRemove {
                repo: RepoRef(repo.clone()),
            };
            match client.request(request).await? {
                Reply::Ok | Reply::Repo(_) => {}
                other => return Err(unexpected(&other, "ok")),
            }
            let text = format!("Removed {repo} from the registry (its files are untouched).");
            emit(json, || Ok(serde_json::json!({"ok": true})), &text)?;
        }
        RepoCommand::Set {
            repo,
            ship,
            base,
            pipeline,
            autonomy_max,
            auto_merge,
            grant_merge,
            attribution,
            gh_user,
        } => {
            let ceiling = ceiling(&repo, autonomy_max, auto_merge).await?;
            let patch = RepoPatch {
                grant_merge,
                attribution,
                gh_user: gh_user_patch(gh_user)?,
                ..repo_patch(ship, base, pipeline, ceiling)
            };
            set(repo, patch, json).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn add_repo(add: RepoAdd, json: bool) -> anyhow::Result<()> {
    let watch = add.github_watch;
    let client = connect_or_start().await?;
    let repo = match client.request(Request::RepoAdd(add)).await? {
        Reply::Repo(repo) => repo,
        other => return Err(unexpected(&other, "repo")),
    };
    let watched = match watch {
        true => "; watching its GitHub origin for `@archon`",
        false => "",
    };
    let text = format!("Added {}{watched}", format::repo_line(&repo));
    emit(json, || to_json(&repo), &text)
}

async fn set(repo: String, patch: RepoPatch, json: bool) -> anyhow::Result<()> {
    let client = connect_or_start().await?;
    let request = Request::RepoUpdate {
        repo: RepoRef(repo),
        patch,
    };
    let repo = match client.request(request).await? {
        Reply::Repo(repo) => repo,
        other => return Err(unexpected(&other, "repo")),
    };
    emit(
        json,
        || to_json(&repo),
        &format!("Updated {}", format::repo_line(&repo)),
    )
}

/// The new registry ceiling: `--autonomy-max` replaces it whole, while
/// `--auto-merge` changes only the merge knob of the current one.
async fn ceiling(
    repo: &str,
    autonomy_max: Option<String>,
    auto_merge: Option<bool>,
) -> anyhow::Result<Option<AutonomyPatch>> {
    if let Some(list) = autonomy_max {
        return Ok(Some(AutonomyPatch::parse_list(&list)?));
    }
    let Some(auto) = auto_merge else {
        return Ok(None);
    };
    let repos = views::fetch_repos(&connect_or_start().await?).await?;
    let current = repos
        .iter()
        .find(|r| r.name == repo || r.id.to_string() == repo)
        .with_context(|| format!("no registered repository {repo:?}"))?;
    Ok(Some(with_merge(current.autonomy_max, auto)))
}

fn with_merge(mut ceiling: AutonomyPatch, auto: bool) -> AutonomyPatch {
    ceiling.merge = Some(if auto {
        MergeMode::Auto
    } else {
        MergeMode::Manual
    });
    ceiling
}

/// An empty `--base` or `--pipeline` clears the setting; no option at all
/// changes nothing but the git identity the daemon records again.
fn repo_patch(
    ship: Option<Ship>,
    base: Option<String>,
    pipeline: Option<String>,
    autonomy_max: Option<AutonomyPatch>,
) -> RepoPatch {
    let clearable = |value: Option<String>| match value.as_deref().map(str::trim) {
        None => Clearable::Keep,
        Some("") => Clearable::Clear,
        Some(v) => Clearable::Set(v.to_owned()),
    };
    RepoPatch {
        ship: ship.map(ship_mode),
        base_branch: clearable(base),
        default_pipeline: clearable(pipeline),
        autonomy_max,
        grant_merge: None,
        attribution: None,
        gh_user: Clearable::Keep,
    }
}

/// `--gh-user`: a login sets it, an empty value clears it.
fn gh_user_patch(value: Option<String>) -> anyhow::Result<Clearable<GhUser>> {
    Ok(match value.as_deref().map(str::trim) {
        None => Clearable::Keep,
        Some("") => Clearable::Clear,
        Some(login) => Clearable::Set(login.parse()?),
    })
}

fn ship_mode(ship: Ship) -> ShipMode {
    match ship {
        Ship::Pr => ShipMode::Pr,
        Ship::Local => ShipMode::Local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_values_clear_and_no_flags_change_nothing() {
        let patch = repo_patch(None, Some(String::new()), Some("fix".into()), None);
        assert_eq!(patch.base_branch, Clearable::Clear);
        assert_eq!(patch.default_pipeline, Clearable::Set("fix".into()));
        let blank = repo_patch(None, None, Some("  ".into()), None);
        assert_eq!(blank.default_pipeline, Clearable::Clear);
        assert_eq!(blank.base_branch, Clearable::Keep);
        let identity_only = repo_patch(None, None, None, None);
        assert_eq!(identity_only, RepoPatch::default());
        let cleared = repo_patch(None, None, None, Some(AutonomyPatch::default()));
        assert_eq!(cleared.autonomy_max, Some(AutonomyPatch::default()));
        let patch = repo_patch(Some(Ship::Local), None, None, None);
        assert_eq!(patch.ship, Some(ShipMode::Local));
    }

    #[test]
    fn a_gh_user_is_set_by_a_login_and_cleared_by_an_empty_value() {
        assert_eq!(gh_user_patch(None).unwrap(), Clearable::Keep);
        assert_eq!(gh_user_patch(Some(" ".into())).unwrap(), Clearable::Clear);
        let set = gh_user_patch(Some("devGunnin".into())).unwrap();
        assert_eq!(set, Clearable::Set("devGunnin".parse().unwrap()));
        let refused = gh_user_patch(Some("--user=x".into())).unwrap_err();
        assert!(refused.to_string().contains("no GitHub login"), "{refused}");
    }

    #[test]
    fn auto_merge_changes_only_the_merge_knob() {
        let ceiling = AutonomyPatch::parse_list("review=standard").unwrap();
        let raised = with_merge(ceiling, true);
        assert_eq!(raised.describe(), "review=standard, merge=auto");
        assert_eq!(with_merge(raised, false).merge, Some(MergeMode::Manual));
    }
}
