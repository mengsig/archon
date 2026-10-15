//! Changing an item's autonomy knobs after it was enqueued (ARCHITECTURE
//! §18.1). An item that never ran takes any knob, with the stages it adds
//! or drops; a running one only `merge` and `tools`, because the stages it
//! started with are frozen; a succeeded one only `merge`, which turns on
//! GitHub's auto-merge of its open pull request at the commit reviewed.

use archon_core::autonomy::{
    Autonomy, AutonomyPatch, Knob, MergeMode, ProposalMode, ReportMode, ReviewLevel, ToolsMode,
};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::{ModelPlan, ModelReasons};
use archon_core::queue::{QueueItem, QueueState};
use archon_pipeline::PipelineSpec;
use archon_pipeline::autonomy::augment;
use archon_queue::Store;

use super::items;
use crate::context::DaemonContext;
use crate::executor::{self, RunTable};
use crate::knobs;
use crate::resolve::RepoEnv;

/// Whether the ceiling bounds the change. Only an operator's confirmed
/// request waives it; no request field can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Waived is built by the operator-confirmed authority path"
)]
pub(crate) enum Ceiling {
    Enforced,
    Waived,
}

/// The stages and plan an item that never ran gets from new knobs.
pub(super) struct Restaged {
    /// The plan the revision was worked out against.
    from: ModelPlan,
    plan: ModelPlan,
    reasons: Option<ModelReasons>,
}

/// A validated autonomy change, worked out before the store is locked.
pub(super) struct Revision {
    pub before: Autonomy,
    pub after: Autonomy,
    restaged: Option<Restaged>,
}

impl Revision {
    /// Whether the item's stages and plan change with it.
    pub fn restages(&self) -> bool {
        self.restaged.is_some()
    }

    pub fn changes(&self) -> bool {
        self.before != self.after
    }

    /// `merge manual → auto, tools guarded → yolo`, for the item's history.
    pub fn describe(&self) -> String {
        let (b, a) = (self.before, self.after);
        let knobs = [
            (ReviewLevel::NAME, b.review.as_str(), a.review.as_str()),
            (ProposalMode::NAME, b.proposal.as_str(), a.proposal.as_str()),
            (ReportMode::NAME, b.report.as_str(), a.report.as_str()),
            (MergeMode::NAME, b.merge.as_str(), a.merge.as_str()),
            (ToolsMode::NAME, b.tools.as_str(), a.tools.as_str()),
        ];
        let changed: Vec<String> = knobs
            .into_iter()
            .filter(|(_, from, to)| from != to)
            .map(|(knob, from, to)| format!("{knob} {from} → {to}"))
            .collect();
        assert!(!changed.is_empty(), "a revision that changes something");
        changed.join(", ")
    }
}

/// Whether the stages an item runs differ between `before` and `after`.
fn restages(before: Autonomy, after: Autonomy) -> bool {
    (before.review, before.proposal, before.report) != (after.review, after.proposal, after.report)
}

/// Refuse a change `state` cannot honour. `ran`: the item has a run, whose
/// stages stay as they were.
fn check_state(
    state: &QueueState,
    id: &QueueItemId,
    before: Autonomy,
    after: Autonomy,
    ran: bool,
) -> Result<(), ArchonError> {
    let refuse = |why: &str| Err(ArchonError::InvalidTransition(format!("item {id} {why}")));
    match state {
        QueueState::Dispatching => {
            return refuse("is starting; send the autonomy again in a moment");
        }
        QueueState::Succeeded { .. } => {
            let only_merge = Autonomy {
                merge: after.merge,
                ..before
            };
            if only_merge != after {
                return refuse(
                    "already succeeded; only its merge knob can change, to turn on auto-merge \
                     of its open pull request",
                );
            }
        }
        QueueState::Running { .. } if restages(before, after) => {
            return refuse(
                "is running with the stages it started with; only merge and tools change now, \
                 and the stages for review, proposal or report follow on a retry",
            );
        }
        _ if ran && restages(before, after) => {
            return refuse(
                "already ran, and its stages came from that run; only merge and tools change \
                 now. Enqueue a new item for other review, proposal or report knobs",
            );
        }
        _ => {}
    }
    Ok(())
}

/// Validate `patch` against `item` as it is now: the ceiling (unless
/// waived), the pipeline, the item's state, and the plan for stages the new
/// knobs add.
pub(super) async fn prepare(
    ctx: &DaemonContext,
    item: &QueueItem,
    patch: &AutonomyPatch,
    ceiling: Ceiling,
) -> Result<Revision, ArchonError> {
    if patch.is_empty() {
        return Err(ArchonError::Invalid("autonomy names no knob".into()));
    }
    let env = RepoEnv::load(ctx, item.repo.as_ref()).await?;
    let policy = env.autonomy(ctx);
    let (base, _) = knobs::item_specs(&env, item)?;
    let (before, after) = (
        item.autonomy,
        knobs::revise(&base, item.autonomy, patch, &policy)?,
    );
    if ceiling == Ceiling::Enforced {
        knobs::check_raised(&policy, &env, &base, before, after)?;
    }
    let ran = !ctx.store.lock().await.runs_for_item(&item.id)?.is_empty();
    check_state(&item.state, &item.id, before, after, ran)?;
    let restaged = match restages(before, after) {
        true => Some(restage(ctx, &env, &base, item, after).await?),
        false => None,
    };
    Ok(Revision {
        before,
        after,
        restaged,
    })
}

/// The plan and reasons of `item` for the stages its `after` knobs give it.
async fn restage(
    ctx: &DaemonContext,
    env: &RepoEnv,
    base: &PipelineSpec,
    item: &QueueItem,
    after: Autonomy,
) -> Result<Restaged, ArchonError> {
    let mut revised = item.clone();
    revised.autonomy = after;
    let spec = augment(base, &after)?;
    let spec = executor::steer::shaped(ctx, &revised, spec).await?;
    let mut plan = item.model_plan.clone();
    plan.0.retain(|stage, _| spec.stage(stage).is_some());
    let plan = items::filled_plan(ctx, &item.id, env, base, &spec, plan).await?;
    items::check_plan(ctx, env, &spec, &plan)?;
    let reasons = item.model_reasons.clone().map(|mut reasons| {
        reasons.retain(|stage, _| spec.stage(stage).is_some());
        reasons
    });
    Ok(Restaged {
        from: item.model_plan.clone(),
        plan,
        reasons,
    })
}

/// Store `revision` on `item`, which the caller loaded under the run table
/// and the store lock, still as the revision was worked out against. A
/// running item's delivery takes the new `merge` only before it starts.
pub(super) fn apply(
    store: &mut Store,
    table: &RunTable,
    item: &mut QueueItem,
    revision: &Revision,
) -> Result<(), ArchonError> {
    let id = &item.id;
    let stale = item.autonomy != revision.before
        || revision
            .restaged
            .as_ref()
            .is_some_and(|r| r.from != item.model_plan);
    if stale {
        return Err(ArchonError::Unavailable(format!(
            "item {id} changed while its autonomy was updated; send the update again"
        )));
    }
    let ran = !store.runs_for_item(id)?.is_empty();
    check_state(&item.state, id, revision.before, revision.after, ran)?;
    if let QueueState::Running { run } = &item.state
        && revision.before.merge != revision.after.merge
    {
        if table.ships.contains_key(run) || table.shipping.contains(run) {
            return Err(ArchonError::InvalidTransition(format!(
                "the delivery of item {id} has started with its merge knob as it was; once its \
                 pull request is open and the item succeeded, update merge again"
            )));
        }
        let on = revision.after.merge == MergeMode::Auto;
        executor::set_run_auto_merge(store, run, on)?;
    }
    item.autonomy = revision.after;
    if let Some(restaged) = &revision.restaged {
        item.model_plan = restaged.plan.clone();
        item.model_reasons = restaged.reasons.clone();
    }
    Ok(())
}
