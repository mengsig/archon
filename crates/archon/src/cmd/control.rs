//! `status`, `pause`, `resume` and `orchestrator status|restart`.

use std::process::ExitCode;

use archon_core::protocol::{ErrorCode, PauseTarget, Reply, RepoRef, Request};

use super::{emit, to_json};
use crate::cli::{OrchestratorCommand, PauseArgs};
use crate::connect::{
    connect_existing, connect_or_start, connect_running, not_running_text, unexpected,
};
use crate::format;
use crate::ops;
use crate::views;

/// Never starts archond or Aether: without archond, its link to Aether
/// cannot be observed either.
/// `check_updates` first has archond ask the network for the latest releases;
/// `verbose` lists every storage directory.
pub async fn status(check_updates: bool, verbose: bool, json: bool) -> anyhow::Result<ExitCode> {
    let Some(client) = connect_existing().await? else {
        return Err(archon_core::error::ArchonError::Unavailable(format!(
            "{}; aether: disconnected (cannot observe its link without archond){}",
            not_running_text(),
            crash_note() + &quarantine_note()
        ))
        .into());
    };
    if check_updates {
        match client.call(Request::CheckUpdates).await? {
            Ok(Reply::Versions(_)) => {}
            Ok(other) => return Err(unexpected(&other, "versions")),
            Err(refusal) => return Err(archon_client::refusal_error(refusal).into()),
        }
    }
    if json {
        let status = views::fetch_status(&client).await?;
        emit(true, || status_json(&status), "")?;
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{}{}{}",
        views::status_text(&client, verbose).await?,
        crash_note(),
        quarantine_note()
    );
    Ok(ExitCode::SUCCESS)
}

/// The status with the last archond crash under `crash`, `null` when none.
fn status_json(status: &impl serde::Serialize) -> anyhow::Result<serde_json::Value> {
    let mut value = to_json(status)?;
    let crash = archon_core::crash::read_latest(&archon_core::paths::data_dir())
        .map_err(|e| anyhow::anyhow!("reading the crash record: {e}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the daemon status is not a JSON object"))?;
    object.insert("crash".into(), to_json(&crash)?);
    let set_aside = archon_core::quarantine::list(&archon_core::paths::data_dir())
        .map_err(|e| anyhow::anyhow!("reading the quarantine records: {e}"))?;
    object.insert("quarantined".into(), to_json(&set_aside)?);
    Ok(value)
}

/// A line on the last archond crash, empty when none is recorded.
fn crash_note() -> String {
    match archon_core::crash::read_latest(&archon_core::paths::data_dir()) {
        Ok(Some(record)) => format!("\ncrash: {}", record.describe()),
        Ok(None) => String::new(),
        Err(error) => format!("\ncrash: {error}"),
    }
}

/// Lines on the records archond set aside, empty when there are none.
fn quarantine_note() -> String {
    match archon_core::quarantine::list(&archon_core::paths::data_dir()) {
        Ok(set_aside) if set_aside.is_empty() => String::new(),
        Ok(set_aside) => format!(
            "\nset aside: {} record(s) archond could not use; `archon doctor` lists them with the command to fix or cancel each",
            set_aside.len()
        ),
        Err(error) => format!("\nset aside: {error}"),
    }
}

pub fn pause_target(args: PauseArgs) -> anyhow::Result<PauseTarget> {
    let scoped = args.repo.is_some() || args.pipeline.is_some();
    if args.hard && scoped {
        return Err(archon_core::ArchonError::Invalid(
            "--hard applies to a global pause only".into(),
        )
        .into());
    }
    let target = match (args.repo, args.pipeline) {
        (Some(repo), None) => PauseTarget::Repo {
            name: RepoRef(repo),
        },
        (None, Some(pipeline)) => PauseTarget::Pipeline { name: pipeline },
        (None, None) => PauseTarget::Global { hard: args.hard },
        (Some(_), Some(_)) => unreachable!("clap rejects --repo together with --pipeline"),
    };
    Ok(target)
}

pub async fn pause(args: PauseArgs, pause: bool, json: bool) -> anyhow::Result<ExitCode> {
    let target = pause_target(args)?;
    let what = format::pause_target(&target);
    let client = connect_or_start().await?;
    let changed = ops::pause(&client, target, pause).await?;
    let verb = if pause { "Paused" } else { "Resumed" };
    let line = match changed {
        true => format!("{verb} {what}."),
        false => format!("Nothing was paused: {what} was running."),
    };
    emit(json, || Ok(serde_json::json!({"ok": true})), &line)?;
    Ok(ExitCode::SUCCESS)
}

pub async fn orchestrator(command: OrchestratorCommand, json: bool) -> anyhow::Result<ExitCode> {
    let client = match command {
        OrchestratorCommand::Status => connect_running().await?,
        OrchestratorCommand::Restart => connect_or_start().await?,
    };
    let reply = match command {
        OrchestratorCommand::Status => client.call(Request::OrchestratorStatus).await?,
        OrchestratorCommand::Restart => client.call(Request::OrchestratorRestart).await?,
    };
    let info = match reply {
        Ok(Reply::Orchestrator(info)) => Some(info),
        Ok(Reply::Ok) => None,
        // Status before the orchestrator exists (Aether not reached yet).
        Err(refusal)
            if refusal.code == ErrorCode::NotFound
                && matches!(command, OrchestratorCommand::Status) =>
        {
            None
        }
        Err(refusal) => return Err(archon_client::refusal_error(refusal).into()),
        Ok(other) => return Err(unexpected(&other, "orchestrator")),
    };
    let line = info
        .as_ref()
        .map_or_else(|| "not running".to_owned(), format::orchestrator_line);
    let text = match command {
        OrchestratorCommand::Status => format!("orchestrator: {line}"),
        OrchestratorCommand::Restart => {
            format!("Restarted the orchestrator.\norchestrator: {line}")
        }
    };
    emit(json, || to_json(&info), &text)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(repo: Option<&str>, pipeline: Option<&str>, hard: bool) -> PauseArgs {
        PauseArgs {
            repo: repo.map(str::to_owned),
            pipeline: pipeline.map(str::to_owned),
            hard,
        }
    }

    #[test]
    fn pause_targets_follow_the_flags() {
        assert_eq!(
            pause_target(args(None, None, false)).unwrap(),
            PauseTarget::Global { hard: false }
        );
        assert_eq!(
            pause_target(args(None, None, true)).unwrap(),
            PauseTarget::Global { hard: true }
        );
        assert_eq!(
            pause_target(args(Some("api"), None, false)).unwrap(),
            PauseTarget::Repo {
                name: RepoRef("api".into())
            }
        );
        assert_eq!(
            pause_target(args(None, Some("ask"), false)).unwrap(),
            PauseTarget::Pipeline { name: "ask".into() }
        );
        assert!(pause_target(args(Some("api"), None, true)).is_err());
    }
}
