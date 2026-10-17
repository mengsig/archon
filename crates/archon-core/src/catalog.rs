//! The pipeline vocabulary shared by the loader, the engine, the daemon and
//! the MCP catalog. The full parsed spec lives in `archon-pipeline`; this is
//! what everyone else needs to see (ARCHITECTURE §8).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::id::QueueItemId;
use crate::model::Effort;

fn default_effort() -> Effort {
    Effort::Medium
}

/// Where a pipeline was found. Later sources replace earlier ones by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineSource {
    Builtin,
    User,
    Repo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    /// Writes a proposal page and changes nothing; at most one, first.
    Propose,
    Work,
    Review,
    Test,
    Ship,
    Report,
    /// Parallel role workers, each writing its own worktree and branch,
    /// merged onto the run branch once every role passes.
    Team,
}

impl StageKind {
    /// Kinds whose agents may run in parallel on one worktree: they read,
    /// they do not write.
    pub fn allows_fan_out(self) -> bool {
        matches!(self, Self::Review | Self::Test | Self::Report)
    }

    pub fn allows_gate(self) -> bool {
        self.allows_fan_out()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    #[default]
    None,
    AllPass,
    Majority,
    Any,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceMode {
    /// A fresh git worktree per pipeline run.
    #[default]
    Worktree,
    /// A detached snapshot of the repository's `HEAD` commit, removed when
    /// the run ends; read-only by instruction.
    RepoReadonly,
    /// `<data>/scratch/<item>`; no repository needed.
    Scratch,
}

/// The directory, under an item's worktree directory, holding the snapshot
/// a `repo-readonly` run works in. Nothing in it is ever kept.
pub const SNAPSHOT_DIR: &str = "snapshot";

/// Whether `workspace` is laid out as the snapshot of `item`:
/// `<…>/<item>/snapshot`, which no other workspace is.
pub fn is_snapshot_of(workspace: &Path, item: &QueueItemId) -> bool {
    let item_dir = workspace.parent().and_then(Path::file_name);
    workspace.file_name() == Some(SNAPSHOT_DIR.as_ref()) && item_dir == Some(item.as_str().as_ref())
}

pub use crate::model_tier::Tier;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Nudges {
    pub tier: Option<Tier>,
    /// Prefer distinct models or vendors across fan-out agents and between
    /// implementer and reviewers.
    pub independent_reviewers: bool,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageSummary {
    pub id: String,
    pub kind: StageKind,
    pub fan_out: u32,
    pub gate: Gate,
    pub timeout_minutes: u32,
    /// The effort a plan gets for this stage when it names none.
    #[serde(default = "default_effort")]
    pub effort: Effort,
    pub nudge: Option<String>,
    /// `on_fail.goto` target and its bound, when the stage loops back.
    pub loops_to: Option<String>,
    pub max_loops: u32,
    /// Role names of a team stage, in merge order; empty for other kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// The earlier stage whose swarm plan gives this team stage its roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles_from: Option<String>,
    /// The tier this stage gets when the model plan names no model for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<crate::model_tier::Tier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineSummary {
    pub name: String,
    pub description: String,
    pub source: PipelineSource,
    /// Directory on disk; `None` for built-ins compiled into the binary.
    pub path: Option<PathBuf>,
    pub workspace: WorkspaceMode,
    pub requires_repo: bool,
    pub nudges: Nudges,
    pub stages: Vec<StageSummary>,
}

impl PipelineSummary {
    pub fn stage(&self, id: &str) -> Option<&StageSummary> {
        self.stages.iter().find(|s| s.id == id)
    }
}

/// A pipeline directory that failed to load; its name is unavailable until
/// fixed, never silently replaced by a lower-precedence pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineProblem {
    pub name: String,
    pub source: PipelineSource,
    pub path: Option<PathBuf>,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogListing {
    pub pipelines: Vec<PipelineSummary>,
    pub problems: Vec<PipelineProblem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_items_own_snapshot_directory_is_its_snapshot() {
        let item = QueueItemId::parse("qi_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let dir = Path::new("/wt/demo").join(item.as_str());
        assert!(is_snapshot_of(&dir.join(SNAPSHOT_DIR), &item));
        assert!(!is_snapshot_of(&dir, &item), "a worktree run's workspace");
        assert!(!is_snapshot_of(&dir.join("main"), &item), "a team run's");
        let other = Path::new("/wt/demo/qi_01ARZ3NDEKTSV4RRFFQ69G5FAW/snapshot");
        assert!(!is_snapshot_of(other, &item), "another item's snapshot");
        assert!(!is_snapshot_of(Path::new("/snapshot"), &item));
    }
}
