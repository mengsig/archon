//! Team stages (ARCHITECTURE §13.1): where each role's worker works, the
//! branch it commits on, and the merge the daemon runs onto the run branch
//! once every role passed. Pure naming and data; git lives in the daemon.

use std::path::{Path, PathBuf};

use archon_core::id::PipelineRunId;
use serde::{Deserialize, Serialize};

use crate::gate;

/// The run worktree's directory under a team run's root; workers sit
/// beside it, never inside it.
pub const RUN_TREE_DIR: &str = "main";

/// A worker's directory under the team root: `<stage>-<role>`. Never
/// [`RUN_TREE_DIR`], which has no `-`.
pub fn worker_dir(stage_id: &str, role: &str) -> String {
    assert!(
        !stage_id.is_empty() && !role.is_empty(),
        "validated: stage ids and roles are non-empty"
    );
    let dir = format!("{stage_id}-{role}");
    debug_assert_ne!(dir, RUN_TREE_DIR);
    dir
}

/// Characters of the run id's ULID a worker branch carries.
const RUN_SHORT_CHARS: usize = 8;

/// A worker's branch for one attempt of one run:
/// `<run branch>--<role>-<run8>-a<attempt>`, where `<run8>` is the tail of
/// the run id. Every run of an item shares its run branch and restarts at
/// attempt 1, so the run id keeps a retried item off an earlier run's
/// branches, and each attempt forks afresh from the run branch.
pub fn worker_branch(run_branch: &str, run_id: &PipelineRunId, role: &str, attempt: u32) -> String {
    assert!(attempt >= 1, "attempts are 1-based");
    assert!(
        !run_branch.is_empty() && !role.is_empty(),
        "a worker branch needs a run branch and a role"
    );
    let ulid = &run_id.as_str()[PipelineRunId::PREFIX.len() + 1..];
    assert!(ulid.len() >= RUN_SHORT_CHARS, "ULIDs have 26 characters");
    let run = ulid[ulid.len() - RUN_SHORT_CHARS..].to_ascii_lowercase();
    format!("{run_branch}--{role}-{run}-a{attempt}")
}

/// The role whose [`worker_branch`] of `run_branch`, in any run or
/// attempt, `branch` is; `None` for any other branch.
pub fn worker_branch_role<'a>(run_branch: &str, branch: &'a str) -> Option<&'a str> {
    assert!(!run_branch.is_empty(), "team runs have a run branch");
    let rest = branch.strip_prefix(run_branch)?.strip_prefix("--")?;
    let (rest, attempt) = rest.rsplit_once("-a")?;
    let (role, run) = rest.rsplit_once('-')?;
    let is_run = run.len() == RUN_SHORT_CHARS
        && run
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
    let is_attempt = !attempt.is_empty() && attempt.bytes().all(|b| b.is_ascii_digit());
    (is_run && is_attempt && !role.is_empty()).then_some(role)
}

/// The directory holding a team run's worktrees, given its run worktree
/// `<root>/main`; `None` when `workspace` is not laid out that way.
pub fn team_root(workspace: &Path) -> Option<&Path> {
    let parent = workspace.parent()?;
    (workspace.file_name()? == RUN_TREE_DIR && parent.file_name().is_some()).then_some(parent)
}

/// One team slot: its role, the branch it commits on and its worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamSlot {
    pub role: String,
    pub branch: String,
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleBranch {
    pub role: String,
    pub branch: String,
}

/// Merge the branches run in a passed team attempt onto the run branch,
/// `--no-ff`, in roles order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlan {
    pub stage_id: String,
    pub attempt: u32,
    /// The run branch, checked out in the run worktree.
    pub into: String,
    pub roles: Vec<RoleBranch>,
}

/// What merging one team attempt came to; persisted by the daemon and fed
/// back through [`crate::RunDriver::on_team_merged`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MergeOutcome {
    /// Every role's branch is on the run branch.
    Merged,
    /// `role`'s branch conflicted on `files`; the merge was aborted, and
    /// roles merged before it stay on the run branch.
    Conflict { role: String, files: Vec<String> },
    /// `role`'s worktree is not on its branch, or holds an unfinished
    /// merge, so its work may not be on its branch; nothing was merged.
    Refused { role: String, reason: String },
}

impl MergeOutcome {
    /// The role a failed merge names; `None` when every role merged.
    pub fn role(&self) -> Option<&str> {
        match self {
            MergeOutcome::Merged => None,
            MergeOutcome::Conflict { role, .. } | MergeOutcome::Refused { role, .. } => Some(role),
        }
    }
}

/// The `{{feedback}}` a merge conflict sends through `on_fail`.
pub(crate) fn conflict_report(plan: &MergePlan, role: &str, files: &[String]) -> String {
    let at = role_index(plan, role);
    let mut report = format!(
        "**Stage `{}` attempt {}: merging role `{role}` onto `{}` conflicted**\n\n\
         The merge was aborted. Roles merged before `{role}` stay on the run branch. \
         Files that conflicted, or that are untracked in the run worktree where \
         `{role}`'s branch adds them:\n\n",
        plan.stage_id, plan.attempt, plan.into
    );
    for file in files {
        report.push_str(&format!("- `{file}`\n"));
    }
    if files.is_empty() {
        report.push_str("- (git named no file)\n");
    }
    report.push_str(&branch_list(plan, Some(at)));
    gate::clip(&report, gate::MAX_FEEDBACK_CHARS)
}

/// The `{{feedback}}` a refused merge sends through `on_fail`.
pub(crate) fn refusal_report(plan: &MergePlan, role: &str, reason: &str) -> String {
    assert!(!reason.is_empty(), "a refusal says why");
    assert!(
        plan.roles.iter().any(|r| r.role == role),
        "the refused role belongs to the plan"
    );
    let report = format!(
        "**Stage `{}` attempt {}: role `{role}`'s work could not be merged onto `{}`**\n\n\
         Role `{role}` was not merged: {reason}.\n\n\
         The merge stopped before any role not yet on the run branch.\n{}",
        plan.stage_id,
        plan.attempt,
        plan.into,
        branch_list(plan, None)
    );
    gate::clip(&report, gate::MAX_FEEDBACK_CHARS)
}

fn role_index(plan: &MergePlan, role: &str) -> usize {
    let at = plan.roles.iter().position(|r| r.role == role);
    at.expect("the failed role belongs to the plan")
}

/// Every role's branch, and when known how many were merged, in order:
/// the next attempt forks afresh, so it may need to merge them itself.
fn branch_list(plan: &MergePlan, merged: Option<usize>) -> String {
    assert!(!plan.roles.is_empty(), "a team has roles");
    assert!(
        merged.is_none_or(|m| m < plan.roles.len()),
        "a failed merge left a role out"
    );
    let mut list = String::from(
        "\nEach role's work of this attempt stays on its branch; the next attempt \
         forks afresh from the run branch, so merge or cherry-pick what it still needs:\n\n",
    );
    for (i, role) in plan.roles.iter().enumerate() {
        let state = match merged {
            None => "",
            Some(m) if i < m => " (merged)",
            Some(_) => " (not merged)",
        };
        list.push_str(&format!("- `{}`: `{}`{state}\n", role.role, role.branch));
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_layout() {
        assert_eq!(worker_dir("build", "api"), "build-api");
        let run = PipelineRunId::parse("prun_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(
            worker_branch("archon/w1-x", &run, "api", 2),
            "archon/w1-x--api-q69g5fav-a2"
        );
        let other = PipelineRunId::new();
        assert_ne!(
            worker_branch("archon/w1-x", &other, "api", 2),
            worker_branch("archon/w1-x", &run, "api", 2),
            "another run of the item gets other branches"
        );
        let branch = worker_branch("archon/w1-x", &run, "api-v2", 12);
        assert_eq!(worker_branch_role("archon/w1-x", &branch), Some("api-v2"));
        for other in [
            "archon/w1-x",
            "main",
            "archon/w1-x--api-a2",
            "archon/w1-x--api-Q69G5FAV-a2",
            "archon/w1-x--api-q69g5fav-a",
            "archon/w1-x--q69g5fav-a2",
            "archon/w1-y--api-q69g5fav-a2",
        ] {
            assert_eq!(worker_branch_role("archon/w1-x", other), None, "{other}");
        }
        assert_eq!(
            team_root(Path::new("/wt/repo/item/main")),
            Some(Path::new("/wt/repo/item"))
        );
        assert_eq!(team_root(Path::new("/wt/repo/item")), None);
        assert_eq!(team_root(Path::new("main")), None);
    }

    #[test]
    fn a_conflict_report_lists_every_file() {
        let plan = MergePlan {
            stage_id: "build".into(),
            attempt: 1,
            into: "archon/x".into(),
            roles: vec![RoleBranch {
                role: "api".into(),
                branch: "archon/x--api-q69g5fav-a1".into(),
            }],
        };
        let report = conflict_report(&plan, "api", &["src/a.rs".into(), "b.md".into()]);
        assert!(report.contains("`src/a.rs`") && report.contains("`b.md`"));
        assert!(report.contains("role `api`"));
        assert!(report.contains("`api`: `archon/x--api-q69g5fav-a1` (not merged)"));
    }

    #[test]
    fn reports_name_every_role_branch_and_whether_it_merged() {
        let plan = MergePlan {
            stage_id: "build".into(),
            attempt: 2,
            into: "archon/x".into(),
            roles: ["api", "ui", "db"]
                .map(|role| RoleBranch {
                    role: role.into(),
                    branch: format!("archon/x--{role}-a2"),
                })
                .to_vec(),
        };
        let report = conflict_report(&plan, "ui", &["a".into()]);
        assert!(
            report.contains("`api`: `archon/x--api-a2` (merged)"),
            "{report}"
        );
        assert!(
            report.contains("`ui`: `archon/x--ui-a2` (not merged)"),
            "{report}"
        );
        assert!(
            report.contains("`db`: `archon/x--db-a2` (not merged)"),
            "{report}"
        );

        let report = refusal_report(&plan, "db", "its worktree is on `other`");
        assert!(report.contains("its worktree is on `other`"), "{report}");
        assert!(report.contains("- `api`: `archon/x--api-a2`\n"), "{report}");
    }

    #[test]
    fn merge_outcomes_round_trip_as_json() {
        let conflict = MergeOutcome::Conflict {
            role: "api".into(),
            files: vec!["a".into()],
        };
        let text = serde_json::to_string(&conflict).unwrap();
        assert_eq!(
            serde_json::from_str::<MergeOutcome>(&text).unwrap(),
            conflict
        );
    }
}
