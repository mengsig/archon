//! What a steer still owes the agents of a live stage (ARCHITECTURE §6.6):
//! a patch on its lead, whose later deadline or larger budget also reaches
//! its helpers, and a message to the lead or to its helpers. Each is
//! stored with the steer, so a crash or an unreachable Aether only delays
//! it: the sweep repeats it until Aether answers or the stage run settled.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::model::ModelChoice;
use archon_core::notify::NotificationKind;
use archon_core::ports::{AgentUpdate, DescendantState};
use archon_core::run::StageRun;
use archon_queue::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::context::DaemonContext;
use crate::executor::{FollowUp, notify, resume};

/// `kv`: `steer_owed:<stage run>` → the JSON list of [`Delivery`], oldest first.
const KEY_PREFIX: &str = "steer_owed:";

/// One step a steer owes a stage run: for its lead (patch, which raises its
/// helpers' deadline and budget too, turn end, then message) or a message for
/// the agents beneath it, never both, so a repeat is safe.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Delivery {
    /// Unique per step: keys its messages, so a repeat is delivered once.
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<ModelChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Timestamp>,
    /// End the lead's turn, so a new model runs now and not at its next turn.
    #[serde(default)]
    pub interrupt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<String>,
}

impl Delivery {
    fn patch(&self) -> AgentUpdate {
        AgentUpdate {
            choice: self.choice.clone(),
            cost_budget_usd: self.cost_budget_usd,
            deadline: self.deadline,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.patch().is_empty() && self.lead.is_none() && self.helpers.is_none()
    }
}

/// One lock per stage run being delivered to, kept while anyone holds or
/// awaits it, so a handler's delivery and the sweep's never make one step twice.
#[derive(Debug, Default)]
pub(in crate::executor) struct Locks {
    by_stage: std::sync::Mutex<BTreeMap<StageRunId, Arc<Mutex<()>>>>,
}

impl Locks {
    async fn hold(&self, stage: &StageRunId) -> OwnedMutexGuard<()> {
        let lock = {
            let mut by_stage = self.by_stage.lock().unwrap_or_else(PoisonError::into_inner);
            // An entry only this map refers to is neither held nor awaited.
            by_stage.retain(|_, lock| Arc::strong_count(lock) > 1);
            Arc::clone(by_stage.entry(stage.clone()).or_default())
        };
        assert!(Arc::strong_count(&lock) >= 2, "the map keeps it while held");
        lock.lock_owned().await
    }
}

fn key(stage: &StageRunId) -> String {
    format!("{KEY_PREFIX}{stage}")
}

fn load(store: &Store, stage: &StageRunId) -> Result<Vec<Delivery>, ArchonError> {
    let Some(raw) = store.kv_get(&key(stage))? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("steer owed to stage run {stage}: {e}")))
}

/// Owe `delivery` to live `stage`, after what it is owed already; store it
/// in the same store lock as the steer.
pub(super) fn owe(
    store: &mut Store,
    stage: &StageRunId,
    delivery: Delivery,
) -> Result<(), ArchonError> {
    assert!(!delivery.is_empty(), "a delivery brings something");
    assert!(!delivery.key.is_empty(), "a delivery is keyed");
    assert!(
        delivery.helpers.is_none() || (delivery.lead.is_none() && delivery.patch().is_empty()),
        "a helpers step is a message alone"
    );
    let mut all = load(store, stage)?;
    all.push(delivery);
    store.kv_set(&key(stage), &serde_json::to_string(&all)?)
}

/// Stage runs a steer still owes something.
pub(crate) fn owed(store: &Store) -> Result<Vec<StageRunId>, ArchonError> {
    let mut stages = Vec::new();
    for (key, _) in store.kv_prefix(KEY_PREFIX)? {
        let raw = key.strip_prefix(KEY_PREFIX).expect("ranged by prefix");
        let stage = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("steer owed key {key}: {e}")))?;
        stages.push(stage);
    }
    Ok(stages)
}

/// Make what `id` is owed, oldest first, and forget each step once made;
/// an unanswered step waits for the next sweep, a settled run is owed none.
pub(crate) async fn deliver(ctx: &DaemonContext, id: &StageRunId) -> Result<FollowUp, ArchonError> {
    // Steps are owed only by appending, so under this lock the oldest `made`
    // steps loaded below are still the oldest when they are forgotten.
    let _delivering = ctx.coordination.steering.hold(id).await;
    let (stage, all) = {
        let store = ctx.store.lock().await;
        let stage = match store.stage_run(id) {
            Ok(stage) => Some(stage),
            Err(ArchonError::NotFound(_)) => None,
            Err(error) => return Err(error),
        };
        (stage, load(&store, id)?)
    };
    let live = stage.filter(|stage| !stage.state.is_terminal());
    let Some(stage) = live else {
        ctx.store.lock().await.kv_delete(&key(id))?;
        return Ok(FollowUp::default());
    };
    let Some(agent) = stage.aether_agent.clone() else {
        return Ok(FollowUp::default());
    };
    assert_eq!(&stage.id, id, "the store returns the stage run asked for");
    assert!(
        all.iter().all(|step| !step.is_empty()),
        "only steps that bring something are owed"
    );
    let mut made = 0;
    for delivery in &all {
        if !make(ctx, &stage, &agent, delivery).await? {
            break;
        }
        made += 1;
    }
    forget(&mut *ctx.store.lock().await, id, made)?;
    Ok(FollowUp::default())
}

/// Drop the `made` oldest steps of `stage`; newer ones stay owed.
fn forget(store: &mut Store, stage: &StageRunId, made: usize) -> Result<(), ArchonError> {
    let mut all = load(store, stage)?;
    assert!(made <= all.len(), "steps are only added while one is made");
    all.drain(..made);
    if all.is_empty() {
        return store.kv_delete(&key(stage));
    }
    store.kv_set(&key(stage), &serde_json::to_string(&all)?)
}

/// Make one step. `false` when Aether gave no answer and it must be
/// repeated; a step Aether refuses is reported on the item and dropped.
async fn make(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: &AetherAgentId,
    delivery: &Delivery,
) -> Result<bool, ArchonError> {
    assert!(
        !stage.state.is_terminal(),
        "only a live stage run is steered"
    );
    assert!(
        !delivery.key.is_empty(),
        "Aether dedupes a repeat by its key"
    );
    if delivery.helpers.is_some() {
        return tell_helpers(ctx, agent, delivery).await;
    }
    // A resumed attempt settles on its turn's last word, so the turn is left to end (§6.7).
    let resumed = stage.aether_task.is_none();
    let told = async {
        update_choice(ctx, stage, agent, delivery).await?;
        if delivery.interrupt && !resumed {
            ctx.backend.interrupt(agent).await?;
        }
        match &delivery.lead {
            Some(text) => ctx.backend.send_message(agent, text, &delivery.key).await,
            None => Ok(()),
        }
    };
    match told.await {
        Ok(()) => {}
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(stage = %stage.id, %agent, %error, "steer not delivered; the next sweep retries");
            return Ok(false);
        }
        Err(error) if error.is_store() => return Err(error),
        Err(refusal) => return refused(ctx, stage, &refusal).await.map(|()| true),
    }
    if let Some(text) = delivery.lead.as_deref().filter(|_| resumed) {
        resume::told(&mut *ctx.store.lock().await, &stage.id, text)?;
    }
    Ok(true)
}

async fn update_choice(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: &AetherAgentId,
    delivery: &Delivery,
) -> Result<(), ArchonError> {
    assert_eq!(stage.aether_agent.as_ref(), Some(agent));
    assert!(!stage.state.is_terminal());
    let patch = delivery.patch();
    if !patch.is_empty() {
        patch_lead(ctx, agent, delivery).await?;
        if let Some(choice) = &delivery.choice {
            ctx.store.lock().await.record_model_segment(
                &stage.id,
                &delivery.key,
                choice,
                archon_core::now(),
                archon_core::cost::API_EQUIVALENT,
            )?;
        }
    }
    Ok(())
}

/// Patch the lead. A deadline or budget it sets also raises every live agent
/// beneath it whose own is lower: a helper copied the lead's at its spawn.
async fn patch_lead(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    delivery: &Delivery,
) -> Result<(), ArchonError> {
    let patch = delivery.patch();
    assert!(!patch.is_empty(), "a patch changes something");
    assert!(delivery.helpers.is_none(), "a step for the lead");
    if patch.deadline.is_none() && patch.cost_budget_usd.is_none() {
        return ctx.backend.update_agent(lead, patch).await;
    }
    if ctx.backend.supports_subtree_update().await? {
        return ctx.backend.update_subtree(lead, patch).await;
    }
    ctx.backend.update_agent(lead, patch).await?;
    raise_helpers(ctx, lead, delivery).await
}

/// One keyed update per live agent beneath `lead` whose deadline or budget
/// is lower than the step's, for an Aether that cannot update a subtree.
async fn raise_helpers(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    delivery: &Delivery,
) -> Result<(), ArchonError> {
    assert!(
        delivery.deadline.is_some() || delivery.cost_budget_usd.is_some(),
        "only a deadline or a budget reaches the helpers"
    );
    let helpers = ctx.backend.descendants(lead).await?;
    for helper in helpers.iter().filter(|h| h.state == DescendantState::Live) {
        assert_ne!(&helper.agent, lead, "an agent is not beneath itself");
        let raise = AgentUpdate {
            choice: None,
            cost_budget_usd: raised(helper.cost_budget_usd, delivery.cost_budget_usd),
            deadline: raised(helper.deadline, delivery.deadline),
        };
        if raise.is_empty() {
            continue;
        }
        let key = format!("{}:{}", delivery.key, helper.agent);
        match ctx
            .backend
            .update_agent_keyed(&helper.agent, raise, &key)
            .await
        {
            Ok(()) => {}
            Err(error) if resume::unanswered(&error) || error.is_store() => return Err(error),
            Err(refusal) => {
                tracing::warn!(agent = %helper.agent, %refusal, "helper's deadline or budget not raised; it has most likely ended");
            }
        }
    }
    Ok(())
}

/// `target` where it lifts `current`; a helper with no limit keeps none.
fn raised<T: PartialOrd + Copy>(current: Option<T>, target: Option<T>) -> Option<T> {
    let (current, target) = (current?, target?);
    (target > current).then_some(target)
}

/// Say on the item that Aether refused a steer for a running agent; what the
/// steer stored still applies to the stage's next agents.
async fn refused(
    ctx: &DaemonContext,
    stage: &StageRun,
    refusal: &ArchonError,
) -> Result<(), ArchonError> {
    tracing::warn!(stage = %stage.id, %refusal, "Aether refused a steer for a running agent");
    let run = ctx.store.lock().await.run(&stage.run)?;
    assert_eq!(run.id, stage.run, "the store returns the run asked for");
    let text = format!(
        "A steer did not reach the running agent of stage `{}` (attempt {}): {refusal}. What it \
         stored applies to the stage's next agents.",
        stage.stage_id, stage.attempt
    );
    notify(
        ctx,
        NotificationKind::Progress,
        &text,
        Some(&run.item),
        Some(&run.id),
    )
    .await
}

/// Tell every live agent beneath `lead`; one that ended needs no telling.
async fn tell_helpers(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    delivery: &Delivery,
) -> Result<bool, ArchonError> {
    let text = delivery.helpers.as_deref().expect("a helpers step");
    let helpers = match ctx.backend.descendants(lead).await {
        Ok(helpers) => helpers,
        Err(error) if resume::unanswered(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    for helper in helpers.iter().filter(|h| h.state == DescendantState::Live) {
        assert_ne!(&helper.agent, lead, "an agent is not beneath itself");
        let key = format!("{}:{}", delivery.key, helper.agent);
        match ctx.backend.send_message(&helper.agent, text, &key).await {
            Ok(()) => {}
            Err(error) if resume::unanswered(&error) => return Ok(false),
            Err(refusal) => {
                tracing::info!(agent = %helper.agent, %refusal, "helper not told of a steer; it has most likely ended");
            }
        }
    }
    Ok(true)
}
