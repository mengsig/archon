//! Shared MCP tool names and Aether side-effect classes (ARCHITECTURE §11).
//! The daemon applies stage visibility exceptions when spawning agents.

/// The `[[mcp.servers]] name` Archon registers in Aether.
pub const SERVER_NAME: &str = "archon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolName {
    pub name: &'static str,
    pub class: ToolClass,
}

/// The Aether side-effect class `archon setup` declares for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    ReadOnly,
    /// Changes Archon's own state.
    Mutation,
    /// Acts outside Archon (posts on GitHub); Aether's default approvals gate it.
    External,
}

impl ToolClass {
    /// The class name in Aether's `[[mcp.servers]] classes` table.
    pub const fn aether_name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Mutation => "mutation",
            Self::External => "external",
        }
    }
}

impl ToolName {
    pub const fn read_only(&self) -> bool {
        matches!(self.class, ToolClass::ReadOnly)
    }
}

pub const TOOLS: &[ToolName] = &[
    tool("list_pipelines", ToolClass::ReadOnly),
    tool("list_repos", ToolClass::ReadOnly),
    tool("model_policy", ToolClass::ReadOnly),
    tool("list_queue", ToolClass::ReadOnly),
    tool("get_item", ToolClass::ReadOnly),
    tool("get_stage", ToolClass::ReadOnly),
    tool("queue_status", ToolClass::ReadOnly),
    tool("cost_report", ToolClass::ReadOnly),
    tool("model_evidence", ToolClass::ReadOnly),
    tool("register_repo", ToolClass::Mutation),
    tool("enqueue", ToolClass::Mutation),
    tool("update_item", ToolClass::Mutation),
    tool("cancel_item", ToolClass::Mutation),
    tool("retry_item", ToolClass::Mutation),
    tool("pause", ToolClass::Mutation),
    tool("resume", ToolClass::Mutation),
    tool("notify", ToolClass::Mutation),
    tool("ask_operator", ToolClass::Mutation),
    tool("record_chat_answer", ToolClass::Mutation),
    tool("publish_report", ToolClass::Mutation),
    tool("autonomy_policy", ToolClass::ReadOnly),
    tool("set_directive", ToolClass::Mutation),
    tool("clear_directive", ToolClass::Mutation),
    tool("github_reply", ToolClass::External),
    tool("message_orchestrator", ToolClass::Mutation),
    tool("enqueue_initiative", ToolClass::Mutation),
    tool("update_initiative", ToolClass::Mutation),
    tool("steer_item", ToolClass::Mutation),
    tool("record_lesson", ToolClass::Mutation),
    tool("refresh_item", ToolClass::Mutation),
];

const fn tool(name: &'static str, class: ToolClass) -> ToolName {
    ToolName { name, class }
}

/// How Aether names a tool to agents: `mcp__<server>__<tool>`.
pub fn qualified(tool: &str) -> String {
    format!("mcp__{SERVER_NAME}__{tool}")
}

/// Every Archon tool, before the daemon applies stage visibility exceptions.
pub fn qualified_names() -> Vec<String> {
    TOOLS.iter().map(|t| qualified(t.name)).collect()
}

/// Mutation tools, before the daemon exempts stage messaging (§18.4).
pub fn mutating_names() -> Vec<String> {
    TOOLS
        .iter()
        .filter(|t| !t.read_only())
        .map(|t| qualified(t.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_qualified() {
        let names = qualified_names();
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), TOOLS.len());
        assert!(names.contains(&"mcp__archon__enqueue".to_owned()));
        let mutating = mutating_names();
        assert!(mutating.contains(&"mcp__archon__set_directive".to_owned()));
        assert!(!mutating.contains(&"mcp__archon__get_item".to_owned()));
        assert!(!mutating.contains(&"mcp__archon__get_stage".to_owned()));
        assert!(mutating.contains(&"mcp__archon__github_reply".to_owned()));
        assert!(mutating.iter().all(|name| names.contains(name)));
    }
}
