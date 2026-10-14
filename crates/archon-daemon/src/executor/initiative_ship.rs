//! Recoverable initiative delivery, shared-contract messages and PR links.

use super::dependencies::{is_plain_branch, is_plain_url};
use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, QueueItemId};
use archon_core::initiative::{InitiativeDetail, InitiativeState};
use archon_core::queue::QueueState;
use archon_core::run::PipelineRun;
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PullRequest {
    pub url: String,
    pub state: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: Option<String>,
    pub base_ref_name: String,
    pub body: String,
    pub is_draft: bool,
    /// `CHANGES_REQUESTED`, `APPROVED`, `REVIEW_REQUIRED`, or empty.
    #[serde(default)]
    pub review_decision: Option<String>,
}

pub(super) async fn gh(
    ctx: &DaemonContext,
    repo: &Path,
    args: &[&str],
) -> Result<String, ArchonError> {
    assert!(repo.is_absolute());
    assert!(!args.is_empty());
    let mut command = tokio::process::Command::new(&ctx.config.github.binary);
    command
        .args(args)
        .current_dir(repo)
        .env("GH_PROMPT_DISABLED", "1")
        .env_remove("GH_REPO")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| ArchonError::Unavailable("initiative GitHub command timed out".into()))?
        .map_err(|e| ArchonError::io("initiative GitHub command", e))?;
    if !output.status.success() {
        return Err(ArchonError::Unavailable(format!(
            "gh {}: {}: {}",
            args.join(" "),
            output.status,
            super::clip(&String::from_utf8_lossy(&output.stderr), 2000)
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| ArchonError::Protocol(format!("GitHub output is not UTF-8: {e}")))
}

pub(super) async fn view(
    ctx: &DaemonContext,
    repo: &Path,
    branch: &str,
) -> Result<PullRequest, ArchonError> {
    if !is_plain_branch(branch) {
        return Err(ArchonError::Invalid("unsafe PR branch".into()));
    }
    let raw = gh(
        ctx,
        repo,
        &[
            "pr",
            "view",
            branch,
            "--json",
            "url,state,headRefName,headRefOid,baseRefName,body,isDraft,reviewDecision",
        ],
    )
    .await?;
    let pr: PullRequest = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Protocol(format!("invalid PR response: {e}")))?;
    if !is_plain_url(&pr.url)
        || pr.head_ref_name != branch
        || !is_plain_branch(&pr.base_ref_name)
        || !matches!(pr.state.as_str(), "OPEN" | "CLOSED" | "MERGED")
    {
        return Err(ArchonError::Protocol(
            "PR response does not match its branch, URL or state".into(),
        ));
    }
    if pr.head_ref_oid.as_ref().is_some_and(|sha| {
        !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        return Err(ArchonError::Protocol("invalid PR head commit".into()));
    }
    assert_eq!(pr.head_ref_name, branch);
    assert!(pr.url.starts_with("https://"));
    Ok(pr)
}

/// Whether the pull request of `item` opens as a draft: a member of an
/// atomic initiative stays one until every member passed (§8.9).
pub(super) fn opens_as_draft(
    store: &archon_queue::Store,
    item: &QueueItemId,
) -> Result<bool, ArchonError> {
    let Some(id) = store.item(item)?.initiative else {
        return Ok(false);
    };
    let initiative = store.initiative(&id)?.initiative;
    assert_eq!(initiative.id, id, "the initiative the item names");
    Ok(initiative.atomic && initiative.state != InitiativeState::Done)
}

/// Called once the ship path has verified this PR, before completing the stage.
pub(super) async fn record(
    ctx: &DaemonContext,
    run: &PipelineRun,
    url: &str,
) -> Result<(), ArchonError> {
    let _serial = ctx.coordination.serial.lock().await;
    let item = ctx.store.lock().await.item(&run.item)?;
    check_stack_base(ctx, run).await?;
    let Some(id) = &item.initiative else {
        return Ok(());
    };
    if !is_plain_url(url) {
        return Err(ArchonError::Protocol("invalid initiative PR URL".into()));
    }
    assert_eq!(item.id, run.item);
    assert!(!run.branch.is_empty());
    ctx.store
        .lock()
        .await
        .kv_set(&format!("initiative_pr:{}", item.id), url)?;
    let detail = ctx.store.lock().await.initiative(id)?;
    sync_prs(
        ctx,
        &detail,
        detail.initiative.state == InitiativeState::Done,
    )
    .await
    .map(|_| ())
}

async fn check_stack_base(ctx: &DaemonContext, run: &PipelineRun) -> Result<(), ArchonError> {
    let (owner, base, repo) = {
        let store = ctx.store.lock().await;
        let Some(base) = super::restack::target_base(&store, &run.item)? else {
            return Ok(());
        };
        let fix = store.stack_fix(&run.item)?;
        let owner = fix.map_or_else(|| run.item.clone(), |fix| fix.owner);
        let Some(repo) = store.item(&owner)?.repo else {
            return Err(ArchonError::Store(format!(
                "stacked item {owner} has no repository"
            )));
        };
        let repo = store.repo(&crate::resolve::repo_ref(&repo))?;
        (owner, base, repo)
    };
    assert!(!base.is_empty());
    assert!(repo.path.is_absolute());
    verify_ancestry(ctx, &owner, run).await?;
    let pr = view(ctx, &repo.path, &run.branch).await?;
    if pr.base_ref_name != base {
        gh(
            ctx,
            &repo.path,
            &["pr", "edit", &run.branch, "--base", &base],
        )
        .await?;
    }
    tracing::debug!(item = %owner, %base, "verified stack PR base");
    Ok(())
}

async fn verify_ancestry(
    ctx: &DaemonContext,
    owner: &QueueItemId,
    run: &PipelineRun,
) -> Result<(), ArchonError> {
    let parents = ctx.store.lock().await.stack_parents(owner)?;
    let tree = run.workspace.clone();
    assert!(!parents.is_empty());
    assert!(tree.starts_with(&ctx.paths.worktrees));
    tokio::task::spawn_blocking(move || {
        for parent in parents.iter().filter(|p| !p.merged) {
            if !crate::worktree::stack::contains(&tree, &parent.parent_sha)? {
                return Err(ArchonError::Invalid(format!(
                    "stack is missing parent {} at {}; finish its merge before shipping",
                    parent.parent_item, parent.parent_sha
                )));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| ArchonError::io("verify stack ancestry", std::io::Error::other(e)))?
}

pub(crate) async fn deliver_contract(
    ctx: &DaemonContext,
    detail: &InitiativeDetail,
) -> Result<(), ArchonError> {
    assert!(!detail.members.is_empty());
    let ceiling = archon_core::initiative::InitiativeLimits::CEILING;
    assert!(detail.initiative.contract.len() <= ceiling.contract_bytes);
    for item in &detail.members {
        let stages = {
            let store = ctx.store.lock().await;
            let Some(run) = store
                .runs_for_item(&item.id)?
                .into_iter()
                .find(|r| !r.state.is_terminal())
            else {
                continue;
            };
            store.stage_runs(&run.id)?
        };
        for stage in stages.iter().filter(|s| !s.state.is_terminal()) {
            let Some(agent) = &stage.aether_agent else {
                continue;
            };
            let key = format!(
                "initiative_contract:{}:{}:{}",
                detail.initiative.id,
                detail
                    .initiative
                    .updated_at
                    .timestamp_nanos_opt()
                    .expect("current timestamp"),
                agent
            );
            if ctx.store.lock().await.kv_get(&key)?.is_some() {
                continue;
            }
            let content = format!(
                "Initiative {} shared contract:\n{}",
                detail.initiative.title, detail.initiative.contract
            );
            ctx.backend.send_message(agent, &content, &key).await?;
            ctx.store.lock().await.kv_set(&key, "delivered")?;
        }
    }
    Ok(())
}

pub(crate) async fn cancel_members(
    ctx: &DaemonContext,
    id: &InitiativeId,
) -> Result<(), ArchonError> {
    let detail = ctx.store.lock().await.initiative(id)?;
    assert_eq!(detail.initiative.state, InitiativeState::Cancelled);
    assert!(!detail.members.is_empty());
    for item in detail.members.iter().filter(|m| !m.state.is_terminal()) {
        super::cancel_item(ctx, &item.id, "initiative cancelled").await?;
    }
    Ok(())
}

pub(crate) async fn reconcile(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let initiatives = ctx.store.lock().await.initiatives()?;
    for initiative in initiatives {
        let id = &initiative.id;
        if initiative.state == InitiativeState::Cancelled {
            cancel_members(ctx, id).await?;
        }
        let detail = ctx.store.lock().await.initiative(id)?;
        if !initiative.state.is_terminal() {
            deliver_contract(ctx, &detail).await?;
            advance(ctx, &detail).await?;
        }
        announce(ctx, id).await?;
    }
    Ok(())
}

async fn advance(ctx: &DaemonContext, detail: &InitiativeDetail) -> Result<(), ArchonError> {
    assert!(!detail.members.is_empty());
    assert!(!detail.initiative.state.is_terminal());
    let failed = detail.members.iter().any(|m| {
        matches!(
            m.state,
            QueueState::Failed { .. } | QueueState::Cancelled { .. }
        )
    });
    let passed = detail
        .members
        .iter()
        .all(|m| matches!(m.state, QueueState::Succeeded { .. }));
    let has_prs = sync_prs(ctx, detail, passed && !failed).await?;
    let state = if failed {
        InitiativeState::Failed
    } else if passed {
        InitiativeState::Done
    } else if has_prs {
        InitiativeState::Shipping
    } else {
        return Ok(());
    };
    ctx.store
        .lock()
        .await
        .set_initiative_state(&detail.initiative.id, state)?;
    Ok(())
}

struct MemberPr {
    item: QueueItemId,
    repo: std::path::PathBuf,
    branch: String,
    url: String,
}

async fn member_prs(
    ctx: &DaemonContext,
    detail: &InitiativeDetail,
) -> Result<Vec<MemberPr>, ArchonError> {
    let store = ctx.store.lock().await;
    let mut prs = Vec::new();
    for member in &detail.members {
        let Some(run) = store.runs_for_item(&member.id)?.last().cloned() else {
            continue;
        };
        let stages = store.stage_runs(&run.id)?;
        let url = store
            .kv_get(&format!("initiative_pr:{}", member.id))?
            .or_else(|| archon_core::run::pr_url(stages.iter()).map(str::to_owned));
        let Some(url) = url.filter(|u| is_plain_url(u)) else {
            continue;
        };
        let repo = store.repo(&crate::resolve::repo_ref(
            member.repo.as_ref().expect("member repo"),
        ))?;
        assert!(is_plain_branch(&run.branch));
        prs.push(MemberPr {
            item: member.id.clone(),
            repo: repo.path,
            branch: run.branch,
            url,
        });
    }
    assert!(prs.len() <= detail.members.len());
    Ok(prs)
}

async fn sync_prs(
    ctx: &DaemonContext,
    detail: &InitiativeDetail,
    ready: bool,
) -> Result<bool, ArchonError> {
    let prs = member_prs(ctx, detail).await?;
    let passed = detail
        .members
        .iter()
        .all(|m| matches!(m.state, QueueState::Succeeded { .. }));
    assert!(!ready || passed);
    assert!(!detail.members.is_empty());
    let receipt = format!("initiative_links:{}", detail.initiative.id);
    let desired = serde_json::to_string(&(
        ready,
        prs.iter()
            .map(|p| (&p.item, &p.url, &p.branch))
            .collect::<Vec<_>>(),
    ))?;
    if ctx.store.lock().await.kv_get(&receipt)?.as_deref() == Some(&desired) {
        return Ok(!prs.is_empty());
    }
    for member in &prs {
        let pr = view(ctx, &member.repo, &member.branch).await?;
        if pr.url != member.url {
            return Err(ArchonError::Protocol(
                "initiative PR changed identity".into(),
            ));
        }
        if pr.state != "OPEN" {
            continue;
        }
        let links = prs
            .iter()
            .filter(|p| p.item != member.item)
            .map(|p| p.url.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let body = linked_body(
            &pr.body,
            &detail.initiative.title,
            &detail.initiative.id,
            &links,
        );
        if body != pr.body {
            edit_body(ctx, member, &body).await?;
        }
        if detail.initiative.atomic && pr.is_draft == ready {
            let mut args = vec!["pr", "ready", &member.branch];
            if !ready {
                args.push("--undo");
            }
            gh(ctx, &member.repo, &args).await?;
        }
    }
    ctx.store.lock().await.kv_set(&receipt, &desired)?;
    Ok(!prs.is_empty())
}

fn linked_body(body: &str, title: &str, id: &InitiativeId, links: &str) -> String {
    let start = format!("<!-- archon-initiative:{id} -->");
    let end = "<!-- /archon-initiative -->";
    let section = format!("{start}\nPart of {title}:\n{links}\n{end}");
    if let Some((before, rest)) = body.split_once(&start)
        && let Some((_, after)) = rest.split_once(end)
    {
        return format!("{before}{section}{after}");
    }
    assert!(!title.contains('\n'));
    assert!(!links.contains('`'));
    format!("{}\n\n{section}", body.trim_end())
}

async fn edit_body(ctx: &DaemonContext, member: &MemberPr, body: &str) -> Result<(), ArchonError> {
    let directory = ctx.paths.data_dir.join("initiative-bodies");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|e| ArchonError::io("create PR body directory", e))?;
    let file = directory.join(format!("{}.md", member.item));
    assert!(file.starts_with(&ctx.paths.data_dir));
    assert!(!body.is_empty());
    tokio::fs::write(&file, body)
        .await
        .map_err(|e| ArchonError::io("write initiative PR body", e))?;
    let path = file
        .to_str()
        .ok_or_else(|| ArchonError::Invalid("PR body path is not UTF-8".into()))?;
    gh(
        ctx,
        &member.repo,
        &["pr", "edit", &member.branch, "--body-file", path],
    )
    .await?;
    Ok(())
}

pub(crate) async fn announce(ctx: &DaemonContext, id: &InitiativeId) -> Result<(), ArchonError> {
    let detail = ctx.store.lock().await.initiative(id)?;
    if !detail.initiative.state.is_terminal() {
        return Ok(());
    }
    if detail.initiative.state == InitiativeState::Cancelled
        && detail.members.iter().any(|m| !m.state.is_terminal())
    {
        return Ok(());
    }
    let cycle = ctx
        .store
        .lock()
        .await
        .kv_get(&format!("initiative_cycle:{id}"))?
        .unwrap_or_else(|| "initial".into());
    let key = format!("initiative_finished:{id}:{cycle}");
    let event = crate::triggers::TriggerEvent::InitiativeFinished {
        initiative: id.clone(),
        title: detail.initiative.title,
        state: detail.initiative.state.as_str().into(),
        members: detail.members.into_iter().map(|m| m.id).collect(),
    };
    crate::triggers::push_once(ctx, event, &key).await
}
