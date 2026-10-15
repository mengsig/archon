//! What a steer still owes the agents of a live stage (ARCHITECTURE §6.6):
//! a patch on its lead, and a message to the lead or to its helpers. Each is
//! stored with the steer, so a crash or an unreachable Aether only delays
//! it: the sweep repeats it until Aether answers or the stage run settled.

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

use crate::context::DaemonContext;
use crate::executor::{FollowUp, notify, resume};

/// `kv`: `steer_owed:<stage run>` → the JSON list of [`Delivery`], oldest first.
const KEY_PREFIX: &str = "steer_owed:";

/// One step a steer owes a stage run: for its lead (patch, turn end, then
/// message) or for the agents beneath it, never both, so a repeat is safe.
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

fn key(stage: &StageRunId) -> String {
    format!("{KEY_PREFIX}{stage}")
}

fn load(store: &Store, stage: &StageRunId) -> Result<Vec<Delivery>, ArchonError> {
    let Some(raw) = store.kv_get(&key(stage))? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("steer owed to stage run {stage}: {e}")))
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
        "a step is for the lead or for its helpers"
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
            .map_err(|e| ArchonError::Store(format!("steer owed key {key}: {e}")))?;
        stages.push(stage);
    }
    Ok(stages)
}

/// Make what `id` is owed, oldest first, and forget each step once made;
/// an unanswered step waits for the next sweep, a settled run is owed none.
pub(crate) async fn deliver(ctx: &DaemonContext, id: &StageRunId) -> Result<FollowUp, ArchonError> {
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
        ctx.backend.update_agent(agent, patch).await?;
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
