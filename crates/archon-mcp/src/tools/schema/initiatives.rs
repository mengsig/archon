//! Initiative tools: several items built against one shared contract.

use archon_core::initiative::{MAX_CONTRACT_BYTES, MAX_MEMBERS};
use serde_json::json;

use super::{ToolDefinition, define, object, repo_ref};

pub(super) fn enqueue_initiative() -> ToolDefinition {
    let member = object(
        json!({
            "repo": repo_ref("Where this member's change is made."),
            "brief": {"type": "string", "minLength": 1, "description": "This member's task. Its agents also read the shared contract."},
            "title": {"type": "string", "minLength": 1},
            "pipeline": {"type": "string", "minLength": 1},
            "model_plan": {"type": "object", "description": "As in enqueue."},
            "after": {"type": "array", "items": {"type": "integer", "minimum": 0}, "description": "Positions (from 0) of earlier members this one depends on."},
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
                "contract": {"type": "string", "minLength": 1, "maxLength": MAX_CONTRACT_BYTES, "description": "What every member must agree on: names, formats, rollout order."},
                "atomic": {"type": "boolean", "description": "Keep every pull request a draft until all members pass."},
                "members": {"type": "array", "minItems": 1, "items": member, "description": format!("At most {MAX_MEMBERS}.")},
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
        "Add to an initiative's shared contract. Members still running are told; later ones read it from the start.",
        object(
            json!({
                "initiative": {"type": "string", "description": "Initiative id (`ini_…`) from enqueue_initiative."},
                "contract_append": {"type": "string", "minLength": 1, "description": "Text to add; the whole contract stays within its size limit."},
            }),
            &["initiative", "contract_append"],
        ),
        false,
    )
}
