//! Inspect or replace the operator overlay at one scope.
use super::{emit, to_json};
use crate::connect::{connect_running, unexpected};
use archon_core::protocol::{Reply, RepoRef, Request};
use std::process::ExitCode;

pub async fn run(
    repo: Option<String>,
    set: Option<String>,
    reset: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let repo = repo.map(RepoRef);
    let request = match (set, reset) {
        (Some(raw), false) => Request::SetSettings {
            repo,
            settings: serde_json::from_str(&raw)?,
        },
        (None, true) => Request::ResetSettings { repo },
        (None, false) => Request::Settings { repo },
        (Some(_), true) => unreachable!("clap excludes set with reset"),
    };
    let client = connect_running().await?;
    let view = match client.request(request).await? {
        Reply::Settings(view) => view,
        other => return Err(unexpected(&other, "settings")),
    };
    emit(json, || to_json(&view), &view.describe())?;
    Ok(ExitCode::SUCCESS)
}
