//! `archon pipelines list|check`: the catalog loaded locally, so both work
//! without the daemon (a repo *name* still needs it to find the path).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use archon_core::config::RepoConfig;
use archon_pipeline::{Catalog, CatalogSources};

use super::{emit, to_json};
use crate::cli::PipelineCommand;
use crate::connect::connect_existing;
use crate::format;
use crate::views;

pub async fn run(command: PipelineCommand, json: bool) -> anyhow::Result<ExitCode> {
    if let PipelineCommand::Proposals { name } = &command {
        return proposals(name.as_deref(), json);
    }
    if let PipelineCommand::Accept { name } = &command {
        let path = archon_pipeline::proposals::accept(
            &archon_core::paths::config_dir().join("pipelines"),
            name,
        )?;
        emit(
            json,
            || Ok(serde_json::json!({"name":name,"path":path})),
            &format!("Activated pipeline {name}."),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let (repo, check) = match command {
        PipelineCommand::Proposals { .. } | PipelineCommand::Accept { .. } => {
            unreachable!("accept handled above")
        }
        PipelineCommand::List { repo } => (repo, false),
        PipelineCommand::Check { repo } => (repo, true),
    };
    let repo_root = match repo {
        Some(repo) => Some(resolve_repo(&repo).await?),
        None => None,
    };
    let listing = load(repo_root.as_deref())?.listing();
    let problems = format::problems(&listing);
    assert_eq!(
        problems.len(),
        listing.problems.len(),
        "one line per problem"
    );
    if !check {
        emit(json, || to_json(&listing), &format::catalog(&listing))?;
        return Ok(ExitCode::SUCCESS);
    }
    let summary = match problems.len() {
        0 => format!("All {} pipelines are valid.", listing.pipelines.len()),
        n => format!("{}\n{n} broken pipeline(s).", problems.join("\n")),
    };
    let value =
        || Ok(serde_json::json!({"valid": listing.pipelines.len(), "problems": listing.problems}));
    emit(json, value, &summary)?;
    Ok(if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Built-in, user and (with a repo) repository pipelines, minus the
/// repository's disabled ones.
fn load(repo_root: Option<&Path>) -> anyhow::Result<Catalog> {
    let disabled = match repo_root {
        Some(root) => RepoConfig::load(root)?.pipelines.disabled,
        None => Vec::new(),
    };
    let sources = CatalogSources {
        user_dir: Some(archon_core::paths::config_dir().join("pipelines")),
        repo_root: repo_root.map(Path::to_path_buf),
        disabled,
    };
    Catalog::discover(&sources).context("loading the pipeline catalog")
}

/// `--repo` is a directory, or else the name of a registered repository.
async fn resolve_repo(repo: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(repo);
    if path.is_dir() {
        return path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()));
    }
    let Some(client) = connect_existing().await? else {
        return Err(archon_core::ArchonError::NotFound(format!("repository {repo}")).into());
    };
    let repos = views::fetch_repos(&client).await?;
    let found = repos.into_iter().find(|r| r.name == repo);
    let Some(found) = found else {
        return Err(archon_core::ArchonError::NotFound(format!("repository {repo}")).into());
    };
    assert!(
        found.path.is_absolute(),
        "the registry stores canonical paths"
    );
    Ok(found.path)
}

fn proposals(name: Option<&str>, json: bool) -> anyhow::Result<ExitCode> {
    let root = archon_core::paths::config_dir().join("pipelines");
    if let Some(name) = name {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(
                archon_core::ArchonError::Invalid("invalid proposed pipeline name".into()).into(),
            );
        }
        let path = root.join(format!("{name}.proposed"));
        let files = archon_pipeline::proposals::read(&path)?;
        archon_pipeline::proposals::validate(name, &files)?;
        let text = files
            .iter()
            .map(|(file, text)| format!("{file}\n{text}"))
            .collect::<Vec<_>>()
            .join("\n");
        emit(
            json,
            || Ok(serde_json::json!({"name":name,"path":path,"files":files})),
            &text,
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut names = Vec::new();
    if root.exists() {
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(name) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".proposed"))
            {
                names.push(name.to_owned());
            }
        }
    }
    names.sort();
    let text = if names.is_empty() {
        "No pipeline proposals.".into()
    } else {
        names.join("\n")
    };
    emit(json, || to_json(&names), &text)?;
    Ok(ExitCode::SUCCESS)
}
