//! `record_lesson`: what the orchestrator learned from an item.

use archon_core::lesson::MAX_TEXT_CHARS;
use serde_json::json;

use super::{ToolDefinition, define, item_id, object, repo_ref};

pub(super) fn record_lesson() -> ToolDefinition {
    define(
        "record_lesson",
        "Record lesson",
        "Record one reusable lesson after an item failed or had to be corrected. Advice is sanitized and never grants permissions. The source item is required and fixes repository scope. Proposed advice has low weight until a confirming win or operator acceptance.",
        object(
            json!({
                "kind": {"type": "string", "enum": ["routing", "practice", "pitfall", "pipeline"], "description": "routing: which model or effort suits which work. pipeline: how a pipeline should be shaped."},
                "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT_CHARS, "description": "One plain sentence without markup, URLs, paths or instruction overrides."},
                "repo": repo_ref("Must match the source item repository; inherited when omitted."),
                "pipeline": {"type": "string", "minLength": 1, "description": "Only for items of this pipeline."},
                "item": item_id("The item that taught it."),
            }),
            &["kind", "text", "item"],
        ),
        false,
    )
}
