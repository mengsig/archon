//! A reviewer child of a lead whose item names no `<stage>.review` plan is
//! held to the reviewer models the lead's contract named, as its stored
//! delegation policy records them (`exempt_models`), so a lead cannot pass
//! off a clone of its own model as a reviewer. A lead sent no permitted
//! worker (`workers` empty) may delegate only its exempt roles: Aether reads
//! that list as its tier rule, so Archon holds the lead to working alone.

use archon_core::aether_ids::AetherAgentId;
use archon_core::delegation::WorkerPolicy;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::model::ModelRef;
use serde_json::Value;

use crate::context::DaemonContext;
use crate::executor::{self, StoredSpawn};

/// Why a child with role `reviewer` of stage agent `parent` breaks the
/// reviewer models its contract named; `None` for any other child, or when
/// the contract named none.
pub(super) async fn violation(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
    model: &ModelRef,
    held: &Value,
) -> Result<Option<String>, ArchonError> {
    let role = role_of(held);
    if !role.eq_ignore_ascii_case(archon_core::delegation::REVIEWER_ROLE) {
        return Ok(None);
    }
    let Some(named) = named_models(ctx, parent).await? else {
        return Ok(None);
    };
    assert!(!named.is_empty(), "named_models gives a list or none");
    Ok(refusal(&named, model))
}

/// The refusal of a reviewer on `model` when the contract named `named`.
fn refusal(named: &[ModelRef], model: &ModelRef) -> Option<String> {
    assert!(
        !named.is_empty(),
        "a contract that names reviewers names one"
    );
    if named.contains(model) {
        return None;
    }
    let listed: Vec<String> = named.iter().map(ToString::to_string).collect();
    Some(format!(
        "reviewer on {model} is not a model the stage contract names for its reviewers ({}); \
         delegate it again on one of them",
        listed.join(", ")
    ))
}

/// Why child `held` of stage agent `parent` breaks a policy that left the
/// lead no permitted worker; `None` for an exempt role or any other policy.
pub(super) async fn alone_violation(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
    held: &Value,
) -> Result<Option<String>, ArchonError> {
    let Some(policy) = sent_policy(ctx, parent).await? else {
        return Ok(None);
    };
    Ok(alone_refusal(&policy, role_of(held)))
}

fn role_of(held: &Value) -> &str {
    held.get("role").and_then(Value::as_str).unwrap_or_default()
}

/// The refusal of a child with `role` under `policy` when it names no worker.
fn alone_refusal(policy: &WorkerPolicy, role: &str) -> Option<String> {
    if !policy.workers.is_empty() {
        return None;
    }
    assert!(
        policy.max_parallel == Some(1),
        "a policy with no workers is only the fallback"
    );
    let exempt = policy
        .exempt_roles
        .iter()
        .any(|r| r.eq_ignore_ascii_case(role));
    if exempt {
        return None;
    }
    Some(format!(
        "child with role {role:?}: no permitted worker model resolved for this stage, so \
         its lead works alone and delegates only {}",
        match policy.exempt_roles.is_empty() {
            true => "nothing".to_owned(),
            false => format!("role {}", policy.exempt_roles.join(", ")),
        }
    ))
}

/// The reviewer models stage agent `parent` was sent; `None` when it is no
/// stage agent, its request is gone, or its policy names none.
async fn named_models(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
) -> Result<Option<Vec<ModelRef>>, ArchonError> {
    let models = sent_policy(ctx, parent).await?.map(|p| p.exempt_models);
    Ok(models.filter(|models| !models.is_empty()))
}

/// The delegation policy stage agent `parent` was sent; `None` when it is
/// no stage agent, its request is gone, or it was sent none.
async fn sent_policy(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
) -> Result<Option<WorkerPolicy>, ArchonError> {
    let Some(stage_run) = stage_run_of(ctx, parent).await? else {
        return Ok(None);
    };
    let store = ctx.store.lock().await;
    // A resumed or woken agent holds the policy it was created with.
    if let Some(issuing) = executor::issuing_spawn(&store, &stage_run)? {
        return Ok(issuing.request.delegation_policy);
    }
    let spawn = match executor::load_spawn(&store, &stage_run) {
        Ok(spawn) => spawn,
        // Pruned once its stage settled: no live lead delegates from it.
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(spawn.request.delegation_policy)
}

/// The spawn that issued the contract of the stage agent above `parent`
/// (itself, or the lead it was delegated below); `None` when no stage agent
/// or spawn is found, as once a settled stage's spawn is pruned.
pub(super) async fn lead_spawn(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
) -> Result<Option<StoredSpawn>, ArchonError> {
    let Some(stage_run) = lead_stage_run(ctx, parent).await? else {
        return Ok(None);
    };
    let store = ctx.store.lock().await;
    executor::issuing_spawn(&store, &stage_run)
}

/// The stage run of the stage agent `agent` works for: its own, or that of
/// the nearest agent above it with one, walking up the recorded delegations.
async fn lead_stage_run(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
) -> Result<Option<StageRunId>, ArchonError> {
    let most = ctx.config.limits.max_delegation_depth;
    let mut at = agent.clone();
    for _ in 0..=most {
        let up = {
            let store = ctx.store.lock().await;
            if let Some(stage) = store.stage_run_by_agent(&at)? {
                assert_eq!(stage.aether_agent.as_ref(), Some(&at), "bound to it");
                return Ok(Some(stage.id));
            }
            store.delegated_parent(&at)?
        };
        let Some(up) = up else {
            // The top agent: a stage agent whose spawn reply is not recorded yet.
            return stage_run_of(ctx, &at).await;
        };
        assert_ne!(up, at, "an agent is not its own parent");
        at = up;
    }
    Ok(None)
}

/// The stage run `parent` works for: as Archon recorded it, else by the
/// label it was spawned with, since a child may precede the spawn's reply.
async fn stage_run_of(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
) -> Result<Option<StageRunId>, ArchonError> {
    if let Some(stage) = ctx.store.lock().await.stage_run_by_agent(parent)? {
        return Ok(Some(stage.id));
    }
    let Some(held) = super::held_by_aether(ctx, parent).await? else {
        return Ok(None);
    };
    let Some(raw) = held
        .pointer("/scope/labels/archon.stage_run")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let id = StageRunId::parse(raw).map_err(|e| {
        ArchonError::Protocol(format!("agent {parent} label archon.stage_run: {e}"))
    })?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reviewer_off_the_named_models_is_refused_with_them_listed() {
        let named: Vec<ModelRef> = vec!["codex/terra".parse().unwrap()];
        assert_eq!(refusal(&named, &"codex/terra".parse().unwrap()), None);
        let clone: ModelRef = "claude-code/fable".parse().unwrap();
        let why = refusal(&named, &clone).expect("a clone of the lead is refused");
        assert!(
            why.contains("claude-code/fable") && why.contains("codex/terra"),
            "{why}"
        );
    }

    #[test]
    fn a_lead_sent_no_worker_delegates_only_its_exempt_roles() {
        use archon_core::delegation::WorkerChoice;
        use archon_core::model::Effort;
        use archon_core::model_tier::{ModelCatalog, Tier};
        use archon_core::policy::ModelPolicy;
        let planned = WorkerPolicy::new(
            vec![WorkerChoice::of_tier(Tier::Balanced, Effort::Medium)],
            None,
            Some(2),
        )
        .unwrap()
        .with_reviewers(vec!["codex/terra".parse().unwrap()]);
        let open = ModelPolicy::from_lists(&[], &[], None);
        let alone = planned.within(&open, &ModelCatalog::default(), None).policy;
        assert!(alone.workers.is_empty());
        assert_eq!(
            alone_refusal(&alone, "reviewer"),
            None,
            "reviewers are never failed closed"
        );
        assert_eq!(alone_refusal(&alone, "Reviewer"), None);
        let why = alone_refusal(&alone, "worker").expect("a worker is cancelled");
        assert!(
            why.contains("works alone") && why.contains("role reviewer"),
            "{why}"
        );
        assert_eq!(
            alone_refusal(&planned, "worker"),
            None,
            "a policy with workers is Aether's to hold"
        );
    }
}
