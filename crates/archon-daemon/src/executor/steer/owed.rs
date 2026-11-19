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
    /// What the lead is told instead of `lead` when its model step is refused.
    #[serde(default)]
    pub note: RefusedNote,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<String>,
}

/// What a refused model step tells the lead, since its model did not change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RefusedNote {
    /// A step owed before notes were split from `lead`: `lead` is told as before.
    #[default]
    Lead,
    /// The operator's note alone, or nothing.
    Note(Option<String>),
}

impl Delivery {
    /// The message the lead is told and whether its turn ends, with its model step
    /// `refused` or not: a refused step keeps the lead's turn and claims no new model.
    fn told_lead(&self, refused: bool) -> (Option<&str>, bool) {
        match (&self.note, refused) {
            (RefusedNote::Note(note), true) => (note.as_deref(), false),
            _ => (self.lead.as_deref(), self.interrupt),
        }
    }

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
/// awaits it, so a handler's delivery and the sweep's never make one step twice,
/// and a held stage's deadline never moves while a steer's is sent.
#[derive(Debug, Default)]
pub(in crate::executor) struct Locks {
    by_stage: std::sync::Mutex<BTreeMap<StageRunId, Arc<Mutex<()>>>>,
}

impl Locks {
    pub(in crate::executor) async fn hold(&self, stage: &StageRunId) -> OwnedMutexGuard<()> {
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
    // A refused model is reported; the lead keeps its turn and model and hears the note alone.
    let told = async {
        let refused_model = update_choice(ctx, stage, agent, delivery).await?;
        let (lead, interrupt) = delivery.told_lead(refused_model.is_some());
        if interrupt && !resumed {
            ctx.backend.interrupt(agent).await?;
        }
        if let Some(text) = lead {
            ctx.backend.send_message(agent, text, &delivery.key).await?;
        }
        Ok((refused_model, lead))
    };
    let (refused_model, lead) = match told.await {
        Ok(told) => told,
        Err(error) if resume::unanswered(&error) => {
            tracing::info!(stage = %stage.id, %agent, %error, "steer not delivered; the next sweep retries");
            return Ok(false);
        }
        Err(error) if error.is_store() => return Err(error),
        Err(refusal) => return refused(ctx, stage, &refusal, &[]).await.map(|()| true),
    };
    assert!(
        refused_model.is_none() || delivery.choice.is_some(),
        "only a model step has its model refused"
    );
    if let Some(text) = lead.filter(|_| resumed) {
        resume::told(&mut *ctx.store.lock().await, &stage.id, text)?;
    }
    if let Some(refusal) = refused_model {
        let arrived = arrived(delivery, lead.is_some());
        refused(ctx, stage, &refusal, &arrived).await?;
    }
    Ok(true)
}

/// Send `delivery`'s model and limits; the model's refusal when the catalog,
/// policy or Aether refuses it for good, its limits then sent alone.
async fn update_choice(
    ctx: &DaemonContext,
    stage: &StageRun,
    agent: &AetherAgentId,
    delivery: &Delivery,
) -> Result<Option<ArchonError>, ArchonError> {
    assert_eq!(stage.aether_agent.as_ref(), Some(agent));
    assert!(!stage.state.is_terminal());
    let mut delivery = current_limits(&*ctx.store.lock().await, &stage.id, delivery)?;
    if let Some(choice) = &mut delivery.choice
        && let Err(error) = check_choice(ctx, stage, choice).await
    {
        // A deadline or budget never waits for the model check; the whole step stays owed.
        if resume::unanswered(&error) {
            send_limits(ctx, stage, agent, &delivery).await?;
        }
        if resume::unanswered(&error) || error.is_store() {
            return Err(error);
        }
        return limits_alone(ctx, agent, &delivery)
            .await
            .map(|()| Some(error));
    }
    let delivery = &delivery;
    if delivery.patch().is_empty() {
        return Ok(None);
    }
    match patch_lead(ctx, agent, delivery).await {
        Err(error) if delivery.choice.is_some() && is_refusal(&error) => {
            return limits_alone(ctx, agent, delivery)
                .await
                .map(|()| Some(error));
        }
        sent => sent?,
    }
    if let Some(choice) = &delivery.choice {
        ctx.store.lock().await.record_model_segment(
            &stage.id,
            &delivery.key,
            choice,
            archon_core::now(),
            archon_core::cost::API_EQUIVALENT,
        )?;
    }
    Ok(None)
}

/// An answer that refuses for good, neither unanswered nor the store's own failure.
fn is_refusal(error: &ArchonError) -> bool {
    !resume::unanswered(error) && !error.is_store()
}

/// Patch the lead with `delivery`'s deadline and budget, its refused model left out.
async fn limits_alone(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    delivery: &Delivery,
) -> Result<(), ArchonError> {
    assert!(delivery.choice.is_some(), "only a model step is split");
    let limits = Delivery {
        choice: None,
        ..delivery.clone()
    };
    assert!(limits.patch().choice.is_none(), "the model is left out");
    if limits.patch().is_empty() {
        return Ok(());
    }
    patch_lead(ctx, agent, &limits).await
}

/// `delivery` carrying the deadline and budget stored for `stage` now: a later
/// steer or a held stage's sweep may have moved them since it was owed (§6.3).
fn current_limits(
    store: &Store,
    stage: &StageRunId,
    delivery: &Delivery,
) -> Result<Delivery, ArchonError> {
    if delivery.deadline.is_none() && delivery.cost_budget_usd.is_none() {
        return Ok(delivery.clone());
    }
    let stored = match crate::executor::load_spawn(store, stage) {
        Ok(spawn) => spawn.request,
        Err(ArchonError::NotFound(_)) => return Ok(delivery.clone()),
        Err(error) => return Err(error),
    };
    let deadline = delivery
        .deadline
        .map(|asked| stored.deadline.map_or(asked, |now| now.max(asked)));
    let cost_budget_usd = delivery
        .cost_budget_usd
        .map(|asked| stored.cost_budget_usd.unwrap_or(asked));
    assert!(
        deadline >= delivery.deadline,
        "a deadline is only ever raised"
    );
    assert_eq!(
        cost_budget_usd.is_some(),
        delivery.cost_budget_usd.is_some()
    );
    Ok(Delivery {
        deadline,
        cost_budget_usd,
        ..delivery.clone()
    })
}

/// `choice` by its canonical id, once the catalog lists it and the policy permits it.
async fn check_choice(
    ctx: &DaemonContext,
    stage: &StageRun,
    choice: &mut ModelChoice,
) -> Result<(), ArchonError> {
    let catalog = crate::learned::catalog(ctx).await?;
    if let Some(reason) = catalog.not_ready_reason() {
        return Err(ArchonError::Unavailable(reason.to_owned()));
    }
    choice.model = catalog.check(&choice.model)?;
    assert_eq!(&choice.model, catalog.canonical_alias(&choice.model));
    let item = {
        let store = ctx.store.lock().await;
        store.item(&store.run(&stage.run)?.item)?
    };
    let env = crate::resolve::RepoEnv::load(ctx, item.repo.as_ref()).await?;
    env.policy_in(&ctx.config, &catalog.aliases()).check(choice)
}

/// While model step `delivery` waits, send the stored deadline and budget
/// any step `stage` is owed asks for; each step still goes on in order.
async fn send_limits(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    delivery: &Delivery,
) -> Result<(), ArchonError> {
    assert!(delivery.choice.is_some(), "only a model step is split");
    let store = ctx.store.lock().await;
    let owed = load(&store, &stage.id)?;
    let for_lead: Vec<&Delivery> = owed.iter().filter(|s| s.helpers.is_none()).collect();
    let asked = Delivery {
        key: delivery.key.clone(),
        deadline: for_lead.iter().filter_map(|s| s.deadline).max(),
        cost_budget_usd: for_lead.iter().find_map(|s| s.cost_budget_usd),
        ..Delivery::default()
    };
    if asked.patch().is_empty() {
        return Ok(());
    }
    let limits = current_limits(&store, &stage.id, &asked)?;
    drop(store);
    assert!(limits.choice.is_none() && limits.lead.is_none());
    patch_lead(ctx, lead, &limits).await
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
        let key = limit_key(&helper.agent, &raise);
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

/// Keys a helper's raise by the limits it applies: a repeat is made once, and
/// a later raise is never taken for a repeat of an earlier one.
fn limit_key(helper: &AetherAgentId, raise: &AgentUpdate) -> String {
    assert!(raise.choice.is_none(), "a helper is only raised");
    let deadline = raise
        .deadline
        .map_or_else(|| "-".to_owned(), |at| at.timestamp_millis().to_string());
    let budget = raise
        .cost_budget_usd
        .map_or_else(|| "-".to_owned(), |usd| usd.to_string());
    format!("limit:{helper}:{deadline}:{budget}")
}

/// `target` where it lifts `current`; a helper with no limit keeps none.
fn raised<T: PartialOrd + Copy>(current: Option<T>, target: Option<T>) -> Option<T> {
    let (current, target) = (current?, target?);
    (target > current).then_some(target)
}

/// What of model step `delivery` reached the lead once its model was refused.
fn arrived(delivery: &Delivery, told: bool) -> Vec<&'static str> {
    assert!(
        delivery.choice.is_some(),
        "only a model step is refused alone"
    );
    let sent = [
        (told, "note"),
        (delivery.deadline.is_some(), "deadline extension"),
        (delivery.cost_budget_usd.is_some(), "budget"),
    ];
    sent.into_iter()
        .filter(|(sent, _)| *sent)
        .map(|(_, part)| part)
        .collect()
}

/// Say on the item that Aether refused a steer for a running agent, naming what
/// of it `arrived` all the same; what it stored applies to the stage's next agents.
async fn refused(
    ctx: &DaemonContext,
    stage: &StageRun,
    refusal: &ArchonError,
    arrived: &[&str],
) -> Result<(), ArchonError> {
    tracing::warn!(stage = %stage.id, %refusal, "Aether refused a steer for a running agent");
    let run = ctx.store.lock().await.run(&stage.run)?;
    assert_eq!(run.id, stage.run, "the store returns the run asked for");
    let (stage_id, attempt) = (&stage.stage_id, stage.attempt);
    let text = match arrived {
        [] => format!(
            "A steer did not reach the running agent of stage `{stage_id}` (attempt {attempt}): \
             {refusal}. What it stored applies to the stage's next agents."
        ),
        parts => format!(
            "A steer's model was refused for the running agent of stage `{stage_id}` (attempt \
             {attempt}): {refusal}. Its {} reached the agent; the model it stored applies to \
             the stage's next agents.",
            parts.join(" and ")
        ),
    };
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
