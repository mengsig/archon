//! `steer_item`: change an item without cancelling it.

use archon_core::steer::MAX_EXTEND_MINUTES;
use serde_json::json;

use super::{ToolDefinition, define, item_id, model_ref, object};

pub(super) fn steer_item() -> ToolDefinition {
    let insert = object(
        json!({
            "stage": {"type": "string", "minLength": 1, "description": "Stage id to copy."},
            "from": {"type": "string", "minLength": 1, "description": "Pipeline it is copied from; the item's own when omitted."},
            "after": {"type": "string", "minLength": 1, "description": "Stage it follows; before the next unstarted stage when omitted."},
        }),
        &["stage"],
    );
    define(
        "steer_item",
        "Steer item",
        "Change a queued or running item without cancelling it; give at least one change. Stages that already started keep their shape.",
        object(
            json!({
                "item": item_id("The item to steer."),
                "note": {"type": "string", "minLength": 1, "description": "Guidance added to the brief and sent to the item's live agents."},
                "stage": {"type": "string", "minLength": 1, "description": "Stage that model, effort and abort_stage are about; the running one when omitted."},
                "model": model_ref(),
                "effort": {"type": "string", "enum": ["low", "medium", "high", "x_high", "xhigh", "max"]},
                "budget_usd": {"type": "number", "exclusiveMinimum": 0, "description": "The item's new dollar cap."},
                "extend_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_EXTEND_MINUTES, "description": "Minutes added to the running stage's deadline."},
                "abort_stage": {"type": "boolean", "description": "Stop the stage's agents now, then do `then`."},
                "then": {"type": "string", "enum": ["retry", "skip", "goto"], "description": "After abort_stage; retry when omitted."},
                "goto": {"type": "string", "minLength": 1, "description": "Stage `then: goto` continues at."},
                "insert_stage": insert,
                "switch_pipeline": {"type": "string", "minLength": 1, "description": "Pipeline whose stages replace the item's unstarted ones."},
            }),
            &["item"],
        ),
        true,
    )
}
