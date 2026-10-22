//! Swarm plans (docs/PIPELINES.md "Swarm plans"): when a stage that a
//! later team stage takes its roles from passes, Archon reads the plan it
//! committed, checks it, and records the roles it gives the team stage.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use archon_core::error::ArchonError;
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_core::steer::RunEdit;
use archon_pipeline::swarm::{self, PLAN_FILE, SwarmPlan};
use archon_pipeline::{PipelineSpec, SpawnPlan};

use crate::context::DaemonContext;
use crate::resolve::RepoEnv;
use crate::worktree;

/// Between a refused planner's stage id and the problems, in its failure.
const PASSED_BUT: &str = " passed, but ";

/// Whether `error` is the failure of a planner whose plan Archon refused.
pub(super) fn is_refusal(error: &str) -> bool {
    error.contains(PASSED_BUT)
}

/// A settling planner stage: the team stage its plan assigns, in the run's
/// pipeline as the driver holds it.
#[derive(Debug, Clone)]
pub(super) struct Planner {
    pub team: String,
    pub spec: Arc<PipelineSpec>,
}

impl Planner {
    /// The planner `stage_id` is in `spec`, if a team stage takes its roles from it.
    pub fn of(spec: &Arc<PipelineSpec>, stage_id: &str) -> Option<Self> {
        let team = spec.assigned_by(stage_id)?;
        Some(Self {
            team: team.id.clone(),
            spec: Arc::clone(spec),
        })
    }
}

/// Tell the planner of `plan` its limits under the agent `cap` (0: none), and
/// the refusal Archon sent it back with, if any. Other stages are left alone.
pub(super) fn brief(
    plan: &mut SpawnPlan,
    spec: &PipelineSpec,
    cap: u32,
    refusal: Option<(&str, usize)>,
) {
    if spec.assigned_by(&plan.stage_id).is_none() {
        return;
    }
    plan.prompt.push_str(&swarm::planner_limits(cap));
    if let Some((refusal, corrections)) = refusal {
        plan.prompt
            .push_str(&swarm::correction(refusal, corrections));
    }
}

/// `outcome` once the plan a completed planner committed is checked, with
/// the edit giving the team its roles. A refused plan fails the stage, which settle may send back.
pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    planner: Option<Planner>,
    outcome: StageOutcome,
) -> Result<(StageOutcome, Option<RunEdit>), ArchonError> {
    let Some(planner) = planner else {
        return Ok((outcome, None));
    };
    if !matches!(outcome, StageOutcome::Completed { .. }) {
        return Ok((outcome, None));
    }
    assert_eq!(stage.run, run.id, "a plan is checked with its own run");
    let named = planner.spec.assigned_by(&stage.stage_id);
    assert_eq!(
        named.map(|team| team.id.as_str()),
        Some(planner.team.as_str()),
        "the planner assigns its own team stage"
    );
    match assignment(ctx, run, &planner).await? {
        Ok(edit) => Ok((outcome, edit)),
        Err(error) => {
            let error = format!("stage `{}`{PASSED_BUT}{error}", stage.stage_id);
            let failed = StageOutcome::Failed {
                error,
                infra: false,
            };
            Ok((failed, None))
        }
    }
}

/// The edit the committed plan makes, or why it is refused. `Err` only when
/// the store cannot be read.
async fn assignment(
    ctx: &DaemonContext,
    run: &PipelineRun,
    planner: &Planner,
) -> Result<Result<Option<RunEdit>, String>, ArchonError> {
    let plan = match read_plan(run.workspace.clone()).await {
        Ok(text) => swarm::parse(&text).map_err(message),
        Err(error) => Err(error),
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => return Ok(Err(error)),
    };
    let (item, ran) = {
        let store = ctx.store.lock().await;
        let stages = store.stage_runs(&run.id)?;
        let ran = stages.iter().any(|s| s.stage_id == planner.team);
        (store.item(&run.item)?, ran)
    };
    assert_eq!(item.id, run.item, "a run's plan is checked for its item");
    let env = match RepoEnv::load(ctx, item.repo.as_ref()).await {
        Ok(env) => env,
        Err(error) => return Ok(Err(format!("its plan cannot be held to the caps: {error}"))),
    };
    if let Err(error) = within_cap(&plan, env.agent_cap(ctx, &run.pipeline)) {
        return Ok(Err(error));
    }
    Ok(swarm::assign(&planner.spec, &planner.team, &plan, ran).map_err(message))
}

/// The plan file the planner left in the run worktree, which must stay
/// uncommitted so no shipped history carries it.
async fn read_plan(workspace: PathBuf) -> Result<String, String> {
    let read = tokio::task::spawn_blocking(move || uncommitted_plan(&workspace)).await;
    read.map_err(|e| format!("reading its plan stopped: {e}"))?
}

fn uncommitted_plan(workspace: &Path) -> Result<String, String> {
    let file = workspace.join(PLAN_FILE);
    let meta = std::fs::symlink_metadata(&file)
        .map_err(|_| format!("it wrote no `{PLAN_FILE}` in the run worktree"))?;
    if !meta.is_file() {
        return Err(format!("its `{PLAN_FILE}` is not a regular file"));
    }
    let bytes = meta.len();
    if bytes == 0 || bytes > swarm::MAX_PLAN_BYTES as u64 {
        return Err(format!(
            "its `{PLAN_FILE}` has {bytes} bytes; a plan has 1 to {}",
            swarm::MAX_PLAN_BYTES
        ));
    }
    let staged = worktree::git(workspace, &["ls-files", "--", PLAN_FILE])
        .map_err(|e| format!("its plan's status is unread: {e}"))?;
    if !staged.is_empty() {
        return Err(format!(
            "`{PLAN_FILE}` is staged or tracked; keep the plan untracked so it never enters \
             the branch (`git rm --cached {PLAN_FILE}`)"
        ));
    }
    let added = worktree::git(
        workspace,
        &[
            "log",
            "-1",
            "--format=%h",
            "--diff-filter=A",
            "HEAD",
            "--",
            PLAN_FILE,
        ],
    )
    .map_err(|e| format!("its plan's history is unread: {e}"))?;
    if !added.is_empty() {
        return Err(format!(
            "it committed `{PLAN_FILE}` (in {added}); keep the plan uncommitted so it never \
             enters the branch history: remove that commit, leaving the file in the workspace"
        ));
    }
    let text = std::fs::read_to_string(&file).map_err(|e| format!("its plan is unread: {e}"))?;
    if text.trim().is_empty() {
        return Err(format!("its `{PLAN_FILE}` is blank"));
    }
    exclude_plan(workspace)?;
    Ok(text)
}

/// Keep the plan out of every `git add -A` and `git status` of the run
/// worktree, in the repository's `info/exclude` rather than a tracked file.
fn exclude_plan(workspace: &Path) -> Result<(), String> {
    let path = worktree::git(
        workspace,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    )
    .map_err(|e| format!("the exclude file is unlocated: {e}"))?;
    let path = PathBuf::from(path);
    assert!(path.is_absolute(), "git printed an absolute exclude path");
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("the exclude file is unread: {e}")),
    };
    if current.lines().any(|line| line == format!("/{PLAN_FILE}")) {
        return Ok(());
    }
    let mut text = current;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("/{PLAN_FILE}\n"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("the exclude dir is unmade: {e}"))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("the exclude file is unwritten: {e}"))
}

/// A team is admitted whole, so a plan with more tasks than the caps allow
/// at once would wait forever.
fn within_cap(plan: &SwarmPlan, cap: u32) -> Result<(), String> {
    let tasks = plan.tasks.len();
    assert!(tasks >= swarm::MIN_TASKS, "a parsed plan has enough tasks");
    if cap == 0 || tasks <= cap as usize {
        return Ok(());
    }
    Err(format!(
        "its plan has {tasks} tasks, while the agent caps run at most {cap} at once here; \
         plan fewer tasks or raise max_parallel_agents, max_per_repo or max_per_pipeline"
    ))
}

/// A refusal's own text, without the error kind's prefix.
fn message(error: ArchonError) -> String {
    match error {
        ArchonError::Invalid(text) | ArchonError::Pipeline(text) => text,
        other => other.to_string(),
    }
}
