//! A reviewer child of a lead whose item names no `<stage>.review` plan is
//! held to the reviewer models the lead's contract named, as its stored
//! delegation policy records them (`exempt_models`), so a lead cannot pass
//! off a clone of its own model as a reviewer.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::model::ModelRef;
use serde_json::Value;

use crate::context::DaemonContext;
use crate::executor;

/// Why a child with role `reviewer` of stage agent `parent` breaks the
/// reviewer models its contract named; `None` for any other child, or when
/// the contract named none.
pub(super) async fn violation(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
    model: &ModelRef,
    held: &Value,
) -> Result<Option<String>, ArchonError> {
    let role = held.get("role").and_then(Value::as_str).unwrap_or_default();
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

/// The reviewer models stage agent `parent` was sent; `None` when it is no
/// stage agent, its request is gone, or its policy names none.
async fn named_models(
    ctx: &DaemonContext,
    parent: &AetherAgentId,
) -> Result<Option<Vec<ModelRef>>, ArchonError> {
    let Some(stage_run) = stage_run_of(ctx, parent).await? else {
        return Ok(None);
    };
    let store = ctx.store.lock().await;
    let spawn = match executor::load_spawn(&store, &stage_run) {
        Ok(spawn) => spawn,
        // Pruned once its stage settled: no live lead delegates from it.
        Err(ArchonError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let models = spawn.request.delegation_policy.map(|p| p.exempt_models);
    Ok(models.filter(|models| !models.is_empty()))
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
}
