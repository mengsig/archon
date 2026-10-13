//! `record_lesson`: what the orchestrator learned from an item.

use archon_core::lesson::MAX_TEXT_CHARS;
use serde_json::json;

use super::{ToolDefinition, define, item_id, object, repo_ref};

pub(super) fn record_lesson() -> ToolDefinition {
    define(
        "record_lesson",
        "Record lesson",
        "Record one reusable lesson after an item failed or had to be corrected. Stage agents of later items read it word for word, so state what to do, not what happened.",
        object(
            json!({
                "kind": {"type": "string", "enum": ["routing", "practice", "pitfall", "pipeline"], "description": "routing: which model or effort suits which work. pipeline: how a pipeline should be shaped."},
                "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS, "description": "One line."},
                "repo": repo_ref("Where it applies; everywhere when omitted."),
                "pipeline": {"type": "string", "minLength": 1, "description": "Only for items of this pipeline."},
                "item": item_id("The item that taught it."),
            }),
            &["kind", "text"],
        ),
        false,
    )
}
