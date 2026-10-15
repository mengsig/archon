//! Tool definitions and the tool-call to `Request` mapping (ARCHITECTURE
//! §11). Names come from `archon_core::mcp_tools::TOOLS`; every one must be
//! defined here, and nothing else.

mod args;
mod render;
mod schema;

use std::sync::OnceLock;

use archon_core::protocol::{Reply, Request};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: &'static str,
    /// Short display name (MCP `title`).
    pub title: &'static str,
    pub description: &'static str,
    /// JSON Schema for `arguments`.
    pub input_schema: Value,
    /// Only reads state (Aether class `read_only`).
    pub read_only: bool,
    /// May discard work that cannot be recovered without a retry.
    pub destructive: bool,
    /// Acts outside Archon (Aether class `external`).
    pub open_world: bool,
}

impl ToolDefinition {
    /// The entry for `tools/list`.
    pub fn to_mcp(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.read_only,
                "openWorldHint": self.open_world,
            },
        })
    }

    /// The argument names the schema declares.
    pub fn properties(&self) -> impl Iterator<Item = &str> {
        self.input_schema["properties"]
            .as_object()
            .into_iter()
            .flat_map(|props| props.keys().map(String::as_str))
    }

    /// The argument names the schema marks required.
    pub fn required(&self) -> impl Iterator<Item = &str> {
        self.input_schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
    }
}

/// A call the model got wrong; returned as an `isError` tool result so the
/// model can correct itself, never as a JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

pub fn definitions() -> Vec<ToolDefinition> {
    catalog().to_vec()
}

/// The definition of one tool, if it exists.
pub fn definition(tool: &str) -> Option<&'static ToolDefinition> {
    catalog().iter().find(|d| d.name == tool)
}

fn catalog() -> &'static [ToolDefinition] {
    static CATALOG: OnceLock<Vec<ToolDefinition>> = OnceLock::new();
    CATALOG.get_or_init(schema::all)
}

/// Validate arguments and build the daemon request. Operator-only requests
/// are never produced (`register_repo` forces `auto_merge` and `github_watch` off).
pub fn to_request(tool: &str, arguments: Value) -> Result<Request, ToolError> {
    let request = args::to_request(tool, arguments)?;
    assert!(
        !request.is_operator_only(),
        "tool `{tool}` built an operator-only request"
    );
    Ok(request)
}

/// `content[0].text`: a compact human summary of the reply; the reply data
/// itself goes in `structuredContent`. Native agents see only this text, so
/// it carries every id and field needed to act on the reply.
pub fn render(tool: &str, reply: &Reply) -> String {
    render::reply(tool, reply)
}

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod cost_tests;
#[cfg(test)]
mod stage_tests;
#[cfg(test)]
mod tests;
