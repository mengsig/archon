//! Presentation of dispatch reasons. Admission decisions come from the planner;
//! only dependency names and operator actions are resolved here.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{WaitingOn, WaitingReason, WaitingReasonCode as Code};
use archon_core::queue::{QueueItem, QueueState};
use archon_queue::{DispatchSnapshot, Store};

use crate::context::DaemonContext;
use crate::dispatcher;

pub(super) struct WaitingView {
    snapshot: DispatchSnapshot,
    connected: bool,
    reasons: BTreeMap<QueueItemId, (Code, usize)>,
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
        Ok(Self {
            snapshot,
            reasons,
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
                let repo = item
                    .repo
                    .as_ref()
                    .expect("repository pause has a repository");
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
            Code::LowDisk => (
                "Dispatch held by low disk space or a recent disk-full failure".into(),
                "archon gc".into(),
            ),
            Code::Capacity => self.capacity(store, item)?,
            Code::AetherDisconnected => (
                "Dispatch held: Aether disconnected".into(),
                "archon doctor".into(),
            ),
            Code::NextInLine => (
                "Next in line for the next dispatch pass".into(),
                format!("archon queue show {}", item.id),
            ),
            Code::Blocked => (
                "Dependencies recovered; inspect the recorded block before cancelling and retrying"
                    .into(),
                format!("archon queue show {} --full", item.id),
            ),
            Code::OperatorGate => {
                unreachable!("operator waits are handled before dispatch reasons")
            }
        })
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
                let repo = store.repo(&crate::resolve::repo_ref(
                    item.repo
                        .as_ref()
                        .expect("a repository cap has a repository"),
                ))?;
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
            connected: true,
        };
        let item = QueueItem {
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
            fresh_retries: 0,
            initiative: None,
            state: QueueState::Queued,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(view.reason(&store, &item, None).unwrap(), None);
    }
}
