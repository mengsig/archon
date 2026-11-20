//! Presentation of dispatch reasons. Admission decisions come from the planner;
//! only dependency names and operator actions are resolved here.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model_tier::ModelCatalog;
use archon_core::protocol::{WaitingOn, WaitingReason, WaitingReasonCode as Code};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageRun, StageState};
use archon_queue::dispatch::ModelHoldKind;
use archon_queue::{DispatchSnapshot, Store};

use crate::context::DaemonContext;
use crate::dispatcher;
use crate::executor;

pub(super) struct WaitingView {
    snapshot: DispatchSnapshot,
    connected: bool,
    reasons: BTreeMap<QueueItemId, (Code, usize)>,
    /// Running items whose next stage waits for a usable model, with why.
    held_stages: BTreeMap<QueueItemId, (ModelHoldKind, String)>,
}

impl WaitingView {
    pub(super) async fn read(ctx: &DaemonContext) -> Result<Self, ArchonError> {
        let snapshot = dispatcher::read_snapshot(ctx).await?;
        let codes = archon_queue::dispatch::waiting_reasons(&snapshot);
        let reasons = snapshot
            .queued
            .iter()
            .enumerate()
            .map(|(index, candidate)| (candidate.item.clone(), (codes[&candidate.item], index)))
            .collect();
        let mut held_stages = model_held_stages(&snapshot);
        let catalog = dispatcher::catalog_for_view(ctx).await?;
        held_reserved(&*ctx.read_store().await?, &catalog, &mut held_stages)?;
        Ok(Self {
            snapshot,
            reasons,
            held_stages,
            connected: ctx.aether.is_connected(),
        })
    }

    pub(super) fn reason(
        &self,
        store: &Store,
        item: &QueueItem,
        operator: Option<&WaitingOn>,
    ) -> Result<Option<WaitingReason>, ArchonError> {
        if let Some(wait) = operator {
            return Ok(Some(WaitingReason {
                code: Code::OperatorGate,
                message: wait.prompt.clone(),
                next_action: wait.next_action.clone(),
            }));
        }
        let code = match item.state {
            QueueState::Queued => {
                // The listing may include work enqueued after our dispatch snapshot.
                let Some((code, _)) = self.reasons.get(&item.id) else {
                    return Ok(None);
                };
                if !self.connected && *code == Code::NextInLine {
                    Code::AetherDisconnected
                } else {
                    *code
                }
            }
            QueueState::Blocked { .. } => blocked_code(store, item)?,
            _ if item.state.is_active() => match self.held_stages.get(&item.id) {
                Some((ModelHoldKind::CatalogUnavailable, _)) => Code::ModelCatalogUnavailable,
                Some(_) => Code::Blocked,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let (message, next_action) = self.describe(store, item, code)?;
        Ok(Some(WaitingReason {
            code,
            message,
            next_action,
        }))
    }

    fn describe(
        &self,
        store: &Store,
        item: &QueueItem,
        code: Code,
    ) -> Result<(String, String), ArchonError> {
        Ok(match code {
            Code::DependencyFailed | Code::DependencyWaiting => dependency(store, item, code)?,
            Code::GlobalPause => ("Global dispatch is paused".into(), "archon resume".into()),
            Code::RepoPause => {
                let repo = item.repo.as_ref().ok_or_else(|| {
                    ArchonError::Corrupt(format!(
                        "item {} waits on a repository pause but names no repository",
                        item.id
                    ))
                })?;
                let name = store.repo(&crate::resolve::repo_ref(repo))?.name;
                (
                    format!("Repository {name} is paused"),
                    format!("archon resume --repo {repo}"),
                )
            }
            Code::PipelinePause => (
                format!("Pipeline {} is paused", item.pipeline),
                format!("archon resume --pipeline {}", shell_arg(&item.pipeline)),
            ),
            Code::TotalBudget => (
                format!(
                    "Total budget reached ({} / {})",
                    archon_core::cost::format_usd(self.snapshot.spend_total_usd),
                    archon_core::cost::format_usd(self.snapshot.caps.budget_usd_total)
                ),
                format!(
                    "archon queue caps --budget-total {:.2}",
                    higher_budget(self.snapshot.spend_total_usd)
                ),
            ),
            Code::DailyBudget => (
                format!(
                    "Daily budget reached ({} / {}); resets at local midnight",
                    archon_core::cost::format_usd(self.snapshot.spend_today_usd),
                    archon_core::cost::format_usd(self.snapshot.caps.budget_usd_daily)
                ),
                format!(
                    "archon queue caps --budget-daily {:.2}",
                    higher_budget(self.snapshot.spend_today_usd)
                ),
            ),
            Code::Draining => (
                "Dispatch held while an upgrade drains Archon before it restarts".into(),
                "archon status".into(),
            ),
            Code::LowDisk => (
                "Dispatch held by low disk space or a recent disk-full failure".into(),
                "archon gc".into(),
            ),
            Code::HostLoad => {
                let hold = self
                    .snapshot
                    .host_load
                    .as_ref()
                    .expect("a host-load reason names its hold");
                (hold.describe(), hold.next_action())
            }
            Code::Capacity => self.capacity(store, item)?,
            Code::ProviderQuota => self.provider_quota(item),
            Code::ModelCatalogUnavailable => {
                let reason = self.catalog_hold(item);
                let action = if reason.contains("configure a provider") {
                    "configure a provider in Aether"
                } else {
                    "start or reconnect Aether"
                };
                (
                    format!("Model catalog unavailable: {reason}"),
                    action.into(),
                )
            }
            Code::AetherDisconnected => (
                "Dispatch held: Aether disconnected".into(),
                "archon doctor".into(),
            ),
            Code::NextInLine => (
                "Next in line for the next dispatch pass".into(),
                format!("archon queue show {}", item.id),
            ),
            Code::Blocked if item.state.is_active() => self.held_stage_model(item),
            Code::Blocked => {
                let model_reason = match &item.state {
                    QueueState::Blocked { reason } if item.state.is_model_pin_block() => {
                        Some(reason.as_str())
                    }
                    QueueState::Queued => self.reasons.get(&item.id).and_then(|(_, index)| {
                        self.snapshot.queued[*index]
                            .model_hold
                            .as_ref()
                            .map(|hold| hold.reason.as_str())
                    }),
                    _ => None,
                };
                // Retry takes a blocked item only; a queued one is re-planned in place.
                let replan = match item.state {
                    QueueState::Queued => {
                        format!("Re-plan the stage's model for {} with update_item", item.id)
                    }
                    _ => format!("archon queue retry {} --model-plan-file <path>", item.id),
                };
                match model_reason {
                    Some(reason) => (reason.to_owned(), replan),
                    None => ("Dependencies recovered; inspect the recorded block before cancelling and retrying".into(), format!("archon queue show {} --full", item.id)),
                }
            }
            Code::OperatorGate => {
                unreachable!("operator waits are handled before dispatch reasons")
            }
        })
    }

    /// Why the model catalog holds `item`: its queued admission or, once it
    /// runs, the creation of its next stage.
    fn catalog_hold(&self, item: &QueueItem) -> &str {
        let held = self.held_stages.get(&item.id);
        if let Some((kind, reason)) = held.filter(|_| item.state.is_active()) {
            assert_eq!(*kind, ModelHoldKind::CatalogUnavailable, "a catalog hold");
            assert!(!reason.is_empty(), "a catalog hold says why");
            return reason;
        }
        let (_, index) = self.reasons[&item.id];
        let candidate = &self.snapshot.queued[index];
        assert_eq!(
            candidate.item, item.id,
            "the reason indexes the item's candidate"
        );
        candidate
            .model_hold
            .as_ref()
            .map(|hold| hold.reason.as_str())
            .expect("model hold has a reason")
    }

    /// Why a running item's next stage has no usable model, and how to re-plan it.
    fn held_stage_model(&self, item: &QueueItem) -> (String, String) {
        let (kind, reason) = &self.held_stages[&item.id];
        assert_ne!(
            *kind,
            ModelHoldKind::CatalogUnavailable,
            "a model, not a catalog, hold"
        );
        assert!(!reason.is_empty(), "a model hold says why");
        (
            format!("Next stage waits for a valid model: {reason}"),
            format!("Re-plan the stage's model for {} with update_item", item.id),
        )
    }

    /// The stage no provider has quota for, and the earliest reset.
    fn provider_quota(&self, item: &QueueItem) -> (String, String) {
        let (_, index) = self.reasons[&item.id];
        let candidate = &self.snapshot.queued[index];
        assert_eq!(
            candidate.item, item.id,
            "the reason indexes the item's candidate"
        );
        let hold = candidate
            .quota_hold
            .as_ref()
            .expect("a provider quota reason names its hold");
        let reset = hold
            .until
            .map_or("no provider reported a reset".into(), |at| {
                format!("the earliest reset is {}", at.format("%Y-%m-%d %H:%M UTC"))
            });
        (
            format!(
                "Waiting for provider quota: every provider stage {} may use is out ({reset})",
                hold.stage
            ),
            format!(
                "It starts once a provider has quota again; or name a provider or model for {} with update_item",
                item.id
            ),
        )
    }

    fn capacity(&self, store: &Store, item: &QueueItem) -> Result<(String, String), ArchonError> {
        // The map indexes the immutable snapshot built by `read`, avoiding a
        // scan of the backlog for every capacity-held row in a status view.
        let (_, index) = self.reasons[&item.id];
        let candidate = &self.snapshot.queued[index];
        let own = item
            .repo
            .as_ref()
            .map(|r| self.snapshot.repo_caps.get(r).copied().unwrap_or(0));
        let caps = &self.snapshot.caps;
        let cap = archon_queue::dispatch::effective_agent_cap(caps, own, &item.pipeline);
        let wanted = candidate.first_stage_agents;
        if candidate.gang && cap != 0 && wanted > cap {
            let mut actions = Vec::new();
            if caps.max_parallel_agents != 0 && caps.max_parallel_agents < wanted {
                actions.push(format!("archon queue caps --max-parallel-agents {wanted}"));
            }
            if own.is_some_and(|cap| cap != 0 && cap < wanted) {
                let Some(item_repo) = item.repo.as_ref() else {
                    return Err(ArchonError::Corrupt(format!(
                        "item {} waits on a repository cap but names no repository",
                        item.id
                    )));
                };
                let repo = store.repo(&crate::resolve::repo_ref(item_repo))?;
                actions.push(format!(
                    "Set [limits].max_per_repo = {wanted} in {}",
                    repo.path.join(".archon/config.toml").display()
                ));
            }
            if caps.max_per_repo != 0 && caps.max_per_repo < wanted {
                actions.push(format!("archon queue caps --max-per-repo {wanted}"));
            }
            if caps
                .max_per_pipeline
                .get(&item.pipeline)
                .is_some_and(|cap| *cap != 0 && *cap < wanted)
            {
                actions.push(format!("Set the {} entry under [limits.per_pipeline] to {wanted} in Archon's config.toml and restart archond", item.pipeline));
            }
            let action = actions.join("; ");
            return Ok((
                format!("Team needs {wanted} agents together, but its effective cap is {cap}"),
                action,
            ));
        }
        Ok((
            "Waiting for agent/run capacity, including reservations for earlier work and teams"
                .into(),
            "archon queue list --state running".into(),
        ))
    }
}

fn higher_budget(spent: f64) -> f64 {
    // Keep the suggested two-decimal amount strictly above the observed spend.
    spent.ceil() + 1.0
}

fn shell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn blocked_code(store: &Store, item: &QueueItem) -> Result<Code, ArchonError> {
    if item.state.is_model_pin_block() {
        return Ok(Code::Blocked);
    }
    let mut waiting = false;
    for id in &item.dependencies {
        match store.item(id)?.state {
            QueueState::Failed { .. } | QueueState::Cancelled { .. } => {
                return Ok(Code::DependencyFailed);
            }
            QueueState::Succeeded { .. } => {}
            _ => waiting = true,
        }
    }
    Ok(if waiting {
        Code::DependencyWaiting
    } else {
        Code::Blocked
    })
}

fn dependency(
    store: &Store,
    item: &QueueItem,
    code: Code,
) -> Result<(String, String), ArchonError> {
    for id in &item.dependencies {
        let dep = store.item(id)?;
        let broken = matches!(
            dep.state,
            QueueState::Failed { .. } | QueueState::Cancelled { .. }
        );
        let matches = if code == Code::DependencyFailed {
            broken
        } else {
            !matches!(dep.state, QueueState::Succeeded { .. })
        };
        if matches {
            let message = format!("Dependency {} ({}) {}", dep.id, dep.title, dep.state.kind());
            let action = if broken {
                format!("archon queue retry {}", dep.id)
            } else {
                format!("archon queue show {}", dep.id)
            };
            return Ok((message, action));
        }
    }
    Err(ArchonError::Unavailable(
        "dependencies changed while reading dispatch reasons; retry the view".into(),
    ))
}

/// Each item whose pending stage spawn a model hold stops, with why.
fn model_held_stages(
    snapshot: &DispatchSnapshot,
) -> BTreeMap<QueueItemId, (ModelHoldKind, String)> {
    snapshot
        .pending_spawns
        .iter()
        .filter_map(|spawn| {
            let hold = spawn.model_hold.as_ref()?;
            Some((spawn.item.clone(), (hold.kind, hold.reason.clone())))
        })
        .collect()
}

/// Reserved stage creations are checked like pending ones (`check_frozen_request`),
/// though the run table no longer lists them: each is held by an outage or by
/// a frozen model the ready catalog refuses.
fn held_reserved(
    store: &Store,
    catalog: &ModelCatalog,
    held: &mut BTreeMap<QueueItemId, (ModelHoldKind, String)>,
) -> Result<(), ArchonError> {
    for stage in store.live_stage_runs()? {
        if stage.state != StageState::Spawning {
            continue;
        }
        let run = store.run(&stage.run)?;
        // A finished run's unsent creation is cancelled, not held.
        if run.state.is_terminal() {
            continue;
        }
        let item = store.item(&run.item)?;
        let Some(hold) = reserved_hold(store, catalog, &item, &stage)? else {
            continue;
        };
        assert!(!hold.1.is_empty(), "a model hold says why");
        held.entry(item.id).or_insert(hold);
    }
    Ok(())
}

/// Why the catalog stops the reserved creation of `stage`, if it does: an
/// unsettled pin the operator re-plans, or a choice with no replacement yet.
fn reserved_hold(
    store: &Store,
    catalog: &ModelCatalog,
    item: &QueueItem,
    stage: &StageRun,
) -> Result<Option<(ModelHoldKind, String)>, ArchonError> {
    assert_eq!(stage.state, StageState::Spawning, "a reserved creation");
    if let Some(reason) = catalog.not_ready_reason() {
        return Ok(Some((ModelHoldKind::CatalogUnavailable, reason.to_owned())));
    }
    let models = match executor::reserved_models(store, &stage.id) {
        Ok(models) => models,
        Err(error) if error.is_store() => return Err(error),
        // Its claim fails a stage whose request cannot be read; no model holds it.
        Err(_) => return Ok(None),
    };
    let Some(refused) = models.iter().find_map(|m| catalog.check(m).err()) else {
        return Ok(None);
    };
    let when = archon_core::plan_origin::TierMove::InRun;
    Ok(Some(match dispatcher::invalid_pin(item, catalog, when) {
        Some(pin) => (ModelHoldKind::InvalidPin, pin),
        None => (ModelHoldKind::NoReplacement, refused.to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_enqueued_after_the_snapshot_has_no_reason_yet() {
        let store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let view = WaitingView {
            snapshot: store.dispatch_snapshot(now).unwrap(),
            reasons: BTreeMap::new(),
            held_stages: BTreeMap::new(),
            connected: true,
        };
        let item = QueueItem {
            intent: None,
            onto: None,
            id: QueueItemId::new(),
            repo: None,
            pipeline: "ask".into(),
            title: "new arrival".into(),
            brief: "brief".into(),
            priority: Default::default(),
            dependencies: vec![],
            tags: vec![],
            model_plan: Default::default(),
            model_reasons: None,
            budget_usd: None,
            autonomy: Default::default(),
            origin: None,
            stage_timeouts: Default::default(),
            fresh_retries: 0,
            routed_provider: None,
            provider_switch: None,
            initiative: None,
            risk: None,
            state: QueueState::Queued,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(view.reason(&store, &item, None).unwrap(), None);
    }
}
