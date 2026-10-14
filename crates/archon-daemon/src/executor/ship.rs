//! PR shipping evidence comes from GitHub, never from an agent's report.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use serde::Deserialize;
use tokio::process::Command;

use super::{frozen, recover};
use crate::context::DaemonContext;

pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    let StageOutcome::Completed {
        summary,
        structured,
    } = outcome
    else {
        return Ok(outcome);
    };
    if run.state.is_terminal() {
        let mut structured = structured;
        if let Some(data) = structured
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
        {
            data.remove("pr_url");
        }
        return Ok(StageOutcome::Completed {
            summary,
            structured,
        });
    }
    let lookup = async {
        let vars = recover::load_run_vars(&*ctx.store.lock().await, &run.id)?;
        match vars.get("ship.mode").map(String::as_str) {
            Some("pr") => {}
            Some("local") => return Ok(None),
            mode => {
                return Err(ArchonError::Protocol(format!(
                    "invalid recorded ship mode: {mode:?}"
                )));
            }
        }
        let spec = frozen::thaw(&ctx.paths.data_dir, &run.id, &run.pipeline).await?;
        if !spec
            .file
            .stages
            .iter()
            .any(|s| s.id == stage.stage_id && s.kind == StageKind::Ship)
        {
            return Ok(None);
        }
        open_pr(&ctx.config.github.binary, &run.workspace, &run.branch)
            .await
            .map(Some)
    }
    .await;
    match lookup {
        Ok(None) => Ok(StageOutcome::Completed {
            summary,
            structured,
        }),
        Err(error) if error.is_store() => Err(error),
        Ok(Some(url)) => {
            super::initiative_ship::record(ctx, run, &url).await?;
            let mut data = structured
                .and_then(|s| s.as_object().cloned())
                .unwrap_or_default();
            data.insert("pr_url".into(), url.into());
            Ok(StageOutcome::Completed {
                summary,
                structured: Some(data.into()),
            })
        }
        Err(error) => Ok(StageOutcome::Failed {
            error: format!(
                "PR shipping verification failed for pushed branch `{}`: {error}. \
                 Create or reopen its PR, confirm `gh pr list --head {} --state open`, then retry the item.",
                run.branch, run.branch
            ),
            infra: false,
        }),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    url: String,
    number: u64,
    head_ref_name: String,
    state: String,
}

async fn open_pr(gh: &Path, workspace: &Path, branch: &str) -> Result<String, ArchonError> {
    let mut command = Command::new(gh);
    command
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--limit",
            "2",
            "--json",
            "url,number,headRefName,state",
        ])
        .current_dir(workspace)
        .env("GH_PROMPT_DISABLED", "1")
        .env_remove("GH_REPO")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| ArchonError::Unavailable("GitHub PR lookup timed out after 30s".into()))?
        .map_err(|e| ArchonError::io("GitHub PR lookup", e))?;
    if !output.status.success() {
        let detail: String = String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(2000)
            .collect();
        return Err(ArchonError::Unavailable(format!(
            "GitHub PR lookup failed ({}): {detail}",
            output.status
        )));
    }
    let prs: Vec<PullRequest> = serde_json::from_slice(&output.stdout)
        .map_err(|e| ArchonError::Protocol(format!("invalid GitHub PR lookup response: {e}")))?;
    let [pr] = prs.as_slice() else {
        return Err(ArchonError::Unavailable(format!(
            "expected one open PR, found {}",
            prs.len()
        )));
    };
    if pr.head_ref_name != branch
        || pr.state != "OPEN"
        || pr.number == 0
        || !pr.url.starts_with("https://")
        || !pr.url.ends_with(&format!("/pull/{}", pr.number))
    {
        return Err(ArchonError::Protocol(
            "GitHub returned a PR that does not match the branch, open state or URL".into(),
        ));
    }
    Ok(pr.url.clone())
}
