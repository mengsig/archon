//! Swarm plans (docs/PIPELINES.md "Swarm plans"): when a stage that a
//! later team stage takes its roles from passes, Archon reads the plan it
//! committed, checks it, and records the roles it gives the team stage.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use archon_core::error::ArchonError;
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_core::steer::RunEdit;
use archon_pipeline::PipelineSpec;
use archon_pipeline::swarm::{self, PLAN_FILE, SwarmPlan};

use crate::context::DaemonContext;
use crate::resolve::RepoEnv;
use crate::worktree;

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

/// `outcome` once the plan a completed planner committed is checked, with
/// the edit giving the team its roles. A refused plan fails the stage.
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
            let error = format!("stage `{}` passed, but {error}", stage.stage_id);
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

/// The plan as committed on the run branch, never the working tree.
async fn read_plan(workspace: PathBuf) -> Result<String, String> {
    let read = tokio::task::spawn_blocking(move || committed_plan(&workspace)).await;
    read.map_err(|e| format!("reading its plan stopped: {e}"))?
}

fn committed_plan(workspace: &Path) -> Result<String, String> {
    let object = format!("HEAD:{PLAN_FILE}");
    let size = worktree::git(workspace, &["cat-file", "-s", &object])
        .map_err(|_| format!("it committed no `{PLAN_FILE}` on the run branch"))?;
    let bytes: usize = size
        .parse()
        .map_err(|_| format!("git printed {size:?} as the size of `{PLAN_FILE}`"))?;
    if bytes == 0 || bytes > swarm::MAX_PLAN_BYTES {
        return Err(format!(
            "its `{PLAN_FILE}` has {bytes} bytes; a plan has 1 to {}",
            swarm::MAX_PLAN_BYTES
        ));
    }
    let text = worktree::git(workspace, &["show", &object])
        .map_err(|e| format!("its plan is unread: {e}"))?;
    // git's output comes back trimmed, so a blank file reads as nothing.
    if text.is_empty() {
        return Err(format!("its `{PLAN_FILE}` is blank"));
    }
    Ok(text)
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
