//! Recoverable initiative delivery, shared-contract messages and PR links.

use super::dependencies::{is_plain_branch, is_plain_url};
use super::gh::{self, Pass};
use crate::context::DaemonContext;
use crate::triggers::MemberResult;
use crate::worktree::stack;
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, QueueItemId};
use archon_core::initiative::{Initiative, InitiativeDetail, InitiativeState};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::Repo;
use archon_core::run::PipelineRun;

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
    let done = detail.initiative.state == InitiativeState::Done;
    sync_prs(ctx, &detail, done, &mut Pass::default())
        .await
        .map(|_| ())
}

/// The repository of the stacked item `owner`, as registered.
fn stack_repo(store: &archon_queue::Store, owner: &QueueItemId) -> Result<Repo, ArchonError> {
    let Some(repo) = store.item(owner)?.repo else {
        return Err(ArchonError::Store(format!(
            "stacked item {owner} has no repository"
        )));
    };
    let repo = store.repo(&crate::resolve::repo_ref(&repo))?;
    assert!(repo.path.is_absolute(), "registered paths are canonical");
    Ok(repo)
}

/// Point the stacked run's pull request at the branch its change is for now.
async fn check_stack_base(ctx: &DaemonContext, run: &PipelineRun) -> Result<(), ArchonError> {
    let (owner, base, repo) = {
        let store = ctx.store.lock().await;
        let Some(base) = super::restack::target_base(&store, &run.item)? else {
            return Ok(());
        };
        let fix = store.stack_fix(&run.item)?;
        let owner = fix.map_or_else(|| run.item.clone(), |fix| fix.owner);
        let repo = stack_repo(&store, &owner)?;
        (owner, base, repo)
    };
    assert!(is_plain_branch(&base), "recorded at the fork");
    let binary = &ctx.config.github.binary;
    let pr = gh::view(binary, &repo.path, &run.branch).await?;
    if pr.base_ref_name != base {
        gh::text(
            binary,
            &repo.path,
            &["pr", "edit", &pr.url, "--base", &base],
        )
        .await?;
    }
    tracing::debug!(item = %owner, %base, "verified stack PR base");
    Ok(())
}

/// Refuse to deliver `rev` of a stacked run, before anything is pushed,
/// unless it holds every parent commit its stack has not seen merged (§8.8),
/// or of a refresh unless it holds the base commit it was queued to merge.
pub(super) async fn verify_stack(
    ctx: &DaemonContext,
    run: &PipelineRun,
    rev: &str,
) -> Result<(), ArchonError> {
    let (needed, repo) = {
        let store = ctx.store.lock().await;
        let fix = store.stack_fix(&run.item)?;
        let refresh = fix.as_ref().and_then(|fix| fix.refresh.clone());
        let owner = fix.map_or_else(|| run.item.clone(), |fix| fix.owner);
        let parents = store.stack_parents(&owner)?;
        let mut needed: Vec<(String, String)> = parents
            .into_iter()
            .filter(|p| !p.merged)
            .map(|p| {
                let why = format!(
                    "stack is missing parent {} at {}",
                    p.parent_item, p.parent_sha
                );
                (p.parent_sha, why)
            })
            .collect();
        if let Some(refresh) = refresh {
            let why = format!(
                "the refresh has not merged `{}` at {}",
                refresh.base, refresh.sha
            );
            needed.push((refresh.sha, why));
        }
        if needed.is_empty() {
            return Ok(());
        }
        (needed, stack_repo(&store, &owner)?)
    };
    assert!(needed.iter().all(|(sha, _)| stack::is_commit_id(sha)));
    let rev = rev.to_owned();
    tokio::task::spawn_blocking(move || {
        for (sha, why) in &needed {
            if !stack::contains(&repo.path, sha, &rev)? {
                return Err(ArchonError::Invalid(format!(
                    "{why}; merge it into the branch before shipping"
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

/// Maintain every initiative once; one that fails is its own blocker and
/// the others, and the stacks after them, go on.
pub(crate) async fn reconcile(ctx: &DaemonContext, pass: &mut Pass) -> Result<(), ArchonError> {
    let initiatives = ctx.store.lock().await.initiatives()?;
    for initiative in initiatives {
        let result = reconcile_one(ctx, &initiative, pass).await;
        let subject = format!("initiative:{}", initiative.id);
        let what = format!("Initiative `{}`", initiative.title);
        super::coordination::isolate(ctx, &subject, None, &what, result).await?;
    }
    Ok(())
}

async fn reconcile_one(
    ctx: &DaemonContext,
    initiative: &Initiative,
    pass: &mut Pass,
) -> Result<(), ArchonError> {
    let id = &initiative.id;
    if initiative.state == InitiativeState::Cancelled {
        cancel_members(ctx, id).await?;
    }
    let detail = ctx.store.lock().await.initiative(id)?;
    assert_eq!(&detail.initiative.id, id, "the initiative asked for");
    if !initiative.state.is_terminal() {
        deliver_contract(ctx, &detail).await?;
        advance(ctx, &detail, pass).await?;
    }
    announce(ctx, id).await
}

async fn advance(
    ctx: &DaemonContext,
    detail: &InitiativeDetail,
    pass: &mut Pass,
) -> Result<(), ArchonError> {
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
    let has_prs = sync_prs(ctx, detail, passed && !failed, pass).await?;
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
    repo: Repo,
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
        let Some(repo) = &member.repo else {
            return Err(ArchonError::Protocol(format!(
                "initiative member {} has no repository",
                member.id
            )));
        };
        if !is_plain_branch(&run.branch) {
            return Err(ArchonError::Protocol(format!(
                "initiative member {} ran on {:?}, which is no plain branch name",
                member.id, run.branch
            )));
        }
        prs.push(MemberPr {
            item: member.id.clone(),
            repo: store.repo(&crate::resolve::repo_ref(repo))?,
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
    pass: &mut Pass,
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
        sync_member(ctx, detail, &prs, member, ready, pass).await?;
    }
    ctx.store.lock().await.kv_set(&receipt, &desired)?;
    Ok(!prs.is_empty())
}

/// Link `member`'s open pull request to the other members' and, in an atomic
/// initiative, mark it ready or draft; remember it once it is merged or closed.
async fn sync_member(
    ctx: &DaemonContext,
    detail: &InitiativeDetail,
    prs: &[MemberPr],
    member: &MemberPr,
    ready: bool,
    pass: &mut Pass,
) -> Result<(), ArchonError> {
    assert!(
        prs.iter().any(|p| p.item == member.item),
        "one of the members"
    );
    let settled = gh::settled(&*ctx.store.lock().await, &member.repo.id, &member.branch)?;
    if settled.is_some() {
        return Ok(());
    }
    let binary = &ctx.config.github.binary;
    let pr = pass
        .pull_request(binary, &member.repo.path, &member.branch)
        .await?;
    if pr.url != member.url {
        return Err(ArchonError::Protocol(
            "initiative PR changed identity".into(),
        ));
    }
    if !pr.is_open() {
        return gh::settle(&mut *ctx.store.lock().await, &member.repo.id, &pr);
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
        gh::text(binary, &member.repo.path, &args).await?;
    }
    Ok(())
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
    let args = ["pr", "edit", &member.branch, "--body-file", path];
    gh::text(&ctx.config.github.binary, &member.repo.path, &args).await?;
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
    let results = member_results(ctx, &detail.members).await?;
    let event = crate::triggers::TriggerEvent::InitiativeFinished {
        initiative: id.clone(),
        title: detail.initiative.title,
        state: detail.initiative.state.as_str().into(),
        members: detail.members.into_iter().map(|m| m.id).collect(),
        results,
    };
    crate::triggers::push_once(ctx, event, &key).await
}

/// Each finished member's outcome, so the orchestrator relays them without reading each item.
async fn member_results(
    ctx: &DaemonContext,
    members: &[QueueItem],
) -> Result<Vec<MemberResult>, ArchonError> {
    assert!(!members.is_empty(), "an initiative has members");
    let store = ctx.store.lock().await;
    let mut results = Vec::with_capacity(members.len());
    for member in members.iter().filter(|m| m.state.is_terminal()) {
        let text = super::finish::outcome_text(member);
        results.push(MemberResult {
            item: member.id.clone(),
            state: member.state.kind().to_owned(),
            summary: super::finish::finished_summary(&text, member),
            cost_usd: store.spend_for_item(&member.id, ctx.config.limits.count_estimated_cost)?,
        });
    }
    assert!(results.len() <= members.len(), "one result per member");
    Ok(results)
}
