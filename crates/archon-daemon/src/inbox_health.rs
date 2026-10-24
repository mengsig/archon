//! Inbox items an Archon-managed agent has not taken into a turn. One left
//! unread past `dispatcher.inbox_unread_after` by an agent of a running
//! stage is a stage health warning in `get_item` and `status`: a lead that
//! died while the agents it delegated to finished looks like this.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::StageWarning;
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use crate::context::DaemonContext;

const KEY_PREFIX: &str = "inbox_unread_";
/// The oldest items are what the warning needs; past this many an agent's
/// newest are only counted, between the oldest and newest of them.
const MAX_KEPT: usize = 64;

/// `kv`: what one agent has been delivered and not yet taken.
fn key(agent: &AetherAgentId) -> String {
    format!("{KEY_PREFIX}{agent}")
}

#[derive(Debug, Serialize, Deserialize)]
struct Unread {
    item: QueueItemId,
    /// Inbox sequence numbers and delivery times, oldest first.
    pending: Vec<(u64, Timestamp)>,
    /// Items delivered past `MAX_KEPT`, not yet known to be taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overflow: Option<Overflow>,
}

/// Unread items not kept one by one: their lowest and highest sequence
/// numbers with delivery times, and how many there are at most.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Overflow {
    first: (u64, Timestamp),
    last: (u64, Timestamp),
    count: u32,
}

impl Overflow {
    fn covers(&self, seq: u64) -> bool {
        (self.first.0..=self.last.0).contains(&seq)
    }

    fn add(&mut self, seq: u64, at: Timestamp) {
        assert!(!self.covers(seq), "a replay was filtered out");
        if seq < self.first.0 {
            self.first = (seq, at);
        } else {
            self.last = (seq, at);
        }
        self.count = self.count.saturating_add(1);
    }

    /// What is left unread once items up to `up_to` were taken. Past the
    /// oldest, its delivery time is unknown: the newest's is used, so a
    /// warning may come late but never early.
    fn after_taking(self, up_to: u64) -> Option<Self> {
        if self.last.0 <= up_to {
            return None;
        }
        if up_to < self.first.0 {
            return Some(self);
        }
        let most = u32::try_from(self.last.0 - up_to).unwrap_or(u32::MAX);
        Some(Self {
            first: self.last,
            last: self.last,
            count: self.count.min(most),
        })
    }
}

impl Unread {
    /// When the oldest unread item was delivered; `None` when none is.
    fn since(&self) -> Option<Timestamp> {
        let kept = self.pending.first().map(|&(_, at)| at);
        let overflowed = self.overflow.map(|o| o.first.1);
        kept.into_iter().chain(overflowed).min()
    }

    fn count(&self) -> u64 {
        let overflowed = self.overflow.map_or(0, |o| o.count);
        self.pending.len() as u64 + u64::from(overflowed)
    }
}

fn read(store: &Store, agent: &AetherAgentId) -> Result<Option<Unread>, ArchonError> {
    let key = key(agent);
    let Some(raw) = store.kv_get(&key)? else {
        return Ok(None);
    };
    let unread: Unread = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
    assert!(
        unread.pending.windows(2).all(|w| w[0].0 < w[1].0),
        "kv {key}: inbox sequence numbers ascend"
    );
    Ok(Some(unread))
}

fn write(store: &mut Store, agent: &AetherAgentId, unread: &Unread) -> Result<(), ArchonError> {
    if unread.since().is_none() {
        return store.kv_delete(&key(agent));
    }
    let raw = serde_json::to_string(unread).expect("plain data encodes");
    store.kv_set(&key(agent), &raw)
}

/// Item `seq` entered the inbox of `agent`, which works for `item`. A
/// replayed delivery is recorded once.
pub(crate) async fn delivered(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    item: QueueItemId,
    seq: u64,
    at: Timestamp,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut unread = read(&store, agent)?.unwrap_or(Unread {
        item,
        pending: Vec::new(),
        overflow: None,
    });
    let replayed = unread.pending.iter().any(|(kept, _)| *kept == seq)
        || unread.overflow.is_some_and(|o| o.covers(seq));
    if replayed {
        return Ok(());
    }
    if unread.pending.len() >= MAX_KEPT {
        match &mut unread.overflow {
            Some(overflow) => overflow.add(seq, at),
            None => {
                let first = (seq, at);
                unread.overflow = Some(Overflow {
                    first,
                    last: first,
                    count: 1,
                });
            }
        }
    } else {
        let at_seq = unread.pending.partition_point(|(kept, _)| *kept < seq);
        unread.pending.insert(at_seq, (seq, at));
    }
    debug_assert!(unread.pending.len() <= MAX_KEPT);
    write(&mut store, agent, &unread)
}

/// `agent` took its inbox items up to `up_to` into a turn.
pub(crate) async fn consumed(
    ctx: &DaemonContext,
    agent: &AetherAgentId,
    up_to: u64,
) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let Some(mut unread) = read(&store, agent)? else {
        return Ok(());
    };
    unread.pending.retain(|(seq, _)| *seq > up_to);
    unread.overflow = unread.overflow.and_then(|o| o.after_taking(up_to));
    assert!(
        unread.pending.iter().all(|(seq, _)| *seq > up_to),
        "taken items are gone"
    );
    write(&mut store, agent, &unread)
}

/// `agent` ended, so nothing it was sent will be read by it.
pub(crate) async fn ended(ctx: &DaemonContext, agent: &AetherAgentId) -> Result<(), ArchonError> {
    ctx.store.lock().await.kv_delete(&key(agent))
}

/// Every agent of a running stage, of `only` when given, that has left an
/// item unread since before `now - dispatcher.inbox_unread_after`, oldest first.
pub(crate) async fn warnings(
    ctx: &DaemonContext,
    now: Timestamp,
    only: Option<&QueueItemId>,
) -> Result<Vec<StageWarning>, ArchonError> {
    let cutoff = now - ctx.config.dispatcher.inbox_unread_after.chrono()?;
    assert!(cutoff < now, "the threshold is positive");
    let store = ctx.store.lock().await;
    let mut found = Vec::new();
    for (key, raw) in store.kv_prefix(KEY_PREFIX)? {
        let id = key.strip_prefix(KEY_PREFIX).expect("ranged by prefix");
        let agent = AetherAgentId::parse(id)
            .map_err(|e| ArchonError::Corrupt(format!("kv key {key}: {e}")))?;
        let unread: Unread = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
        let Some(since) = unread.since() else {
            continue;
        };
        if since > cutoff || only.is_some_and(|item| *item != unread.item) {
            continue;
        }
        let Some(stage) = running_stage_of(&store, &unread.item, &agent)? else {
            continue;
        };
        let count = unread.count();
        found.push(StageWarning {
            item: unread.item,
            stage: stage.id,
            stage_id: stage.stage_id,
            agent,
            unread: u32::try_from(count).unwrap_or(u32::MAX),
            since,
        });
    }
    found.sort_by_key(|warning| warning.since);
    Ok(found)
}

/// The running stage of `item` whose agent is `agent` or delegated to it.
fn running_stage_of(
    store: &Store,
    item: &QueueItemId,
    agent: &AetherAgentId,
) -> Result<Option<StageRun>, ArchonError> {
    let running = (store.stage_runs_for_item(item)?.into_iter())
        .filter(|stage| stage.state == StageState::Running);
    for stage in running {
        let Some(root) = &stage.aether_agent else {
            continue;
        };
        if root == agent || in_tree(store, root, agent)? {
            return Ok(Some(stage));
        }
    }
    Ok(None)
}

fn in_tree(
    store: &Store,
    root: &AetherAgentId,
    agent: &AetherAgentId,
) -> Result<bool, ArchonError> {
    let beneath = store.delegated_beneath(root, false)?;
    Ok(beneath.iter().any(|delegated| delegated.agent == *agent))
}

/// Forget agents whose item finished: none of them is a running stage's.
/// Returns how many were forgotten.
pub(crate) fn prune_finished(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, raw) in store.kv_prefix(KEY_PREFIX)? {
        let unread: Unread = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))?;
        let finished = match store.item(&unread.item) {
            Ok(item) => item.state.is_terminal(),
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if finished {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}
