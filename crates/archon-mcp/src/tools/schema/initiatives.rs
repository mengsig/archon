//! Initiative tools: several items built against one shared contract.

use archon_core::initiative::InitiativeLimits;
use serde_json::json;

use super::{ToolDefinition, define, object, onto, repo_ref};

pub(super) fn enqueue_initiative() -> ToolDefinition {
    let limits = InitiativeLimits::DEFAULT;
    let member = object(
        json!({
            "repo": repo_ref("Where this member's change is made."),
            "brief": {"type": "string", "minLength": 1, "description": "This member's task. Its agents also read the shared contract."},
            "title": {"type": "string", "minLength": 1},
            "pipeline": {"type": "string", "minLength": 1},
            "model_plan": {"type": "object", "description": "As in enqueue."},
            "autonomy": {"type": "object", "description": "As in enqueue; an atomic initiative's members merge by hand."},
            "after": {"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "Positions (from 0) of earlier members this one waits for. Members build against the contract at once: wait only for one whose code this one cannot build or test without."},
            "onto": onto(),
        }),
        &["repo", "brief"],
    );
    define(
        "enqueue_initiative",
        "Enqueue initiative",
        "Enqueue one change that spans repositories: an item per member, all built against one shared contract. Each member's agents read the contract and the other members' branches and pull requests. Returns the initiative id and its items.",
        object(
            json!({
                "title": {"type": "string", "minLength": 1, "description": "One line."},
                "contract": {"type": "string", "minLength": 1, "description": format!("What every member must agree on: names, formats, rollout order. At most {} bytes unless the operator raised it.", limits.contract_bytes)},
                "atomic": {"type": "boolean", "description": "Keep every pull request a draft until all members pass."},
                "members": {"type": "array", "minItems": 1, "items": member, "description": format!("At most {} unless the operator raised it.", limits.members)},
                "idempotency_key": {"type": "string", "minLength": 1},
            }),
            &["title", "contract", "members"],
        ),
        false,
    )
}

pub(super) fn update_initiative() -> ToolDefinition {
    define(
        "update_initiative",
        "Update initiative",
        "Add to an initiative's shared contract, or replace it. Give exactly one of `contract_append` and `contract`. Members still running are told the whole new contract; later ones read it from the start.",
        object(
            json!({
                "initiative": {"type": "string", "description": "Initiative id (`ini_…`) from enqueue_initiative."},
                "contract_append": {"type": "string", "minLength": 1, "description": "Text to add; the whole contract stays within its size limit."},
                "contract": {"type": "string", "minLength": 1, "description": "The whole new contract, for a revision that changes what was agreed."},
            }),
            &["initiative"],
        ),
        false,
    )
}
