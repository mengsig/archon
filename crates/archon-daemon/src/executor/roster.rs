//! Team rosters (ARCHITECTURE §13.1): once every role of a team attempt has
//! an agent, each live role gets one message naming every role's agent,
//! worktree and branch. A role's replacement makes a new roster version,
//! sent again to the live roles.
//!
//! The version is derived from persisted stage runs, and each send is keyed
//! by recipient and version, so a retry after a crash or a lost reply is
//! delivered once. `kv` records the version each stage run last received.

use std::path::PathBuf;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::run::{PipelineRun, RunState, StageRun, StageState};
use archon_pipeline::team;
use archon_queue::Store;

use super::{FollowUp, settle};
use crate::context::DaemonContext;

/// `kv`: `team_roster:<stage run>` → the last roster version it received.
const SENT_KEY_PREFIX: &str = "team_roster:";

fn sent_key(stage: &StageRunId) -> String {
    format!("{SENT_KEY_PREFIX}{stage}")
}

/// Every role of one team attempt, each with its latest stage run's agent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Roster {
    pub stage_id: String,
    pub attempt: u32,
    /// 1 for the first agents, one more per replaced agent since.
    pub version: u32,
    /// In roles order.
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Member {
    pub role: String,
    pub stage_run: StageRunId,
    pub agent: AetherAgentId,
    /// Still working: only running roles are sent the roster.
    pub running: bool,
    pub worktree: PathBuf,
    pub branch: String,
}

impl Roster {
    /// The roster of `run`'s team attempt from its stage runs (`group`),
    /// or `None` while a role's latest stage run has no agent yet.
    pub(crate) fn of(
        run: &PipelineRun,
        stage_id: &str,
        attempt: u32,
        roles: &[String],
        group: &[StageRun],
    ) -> Option<Self> {
        assert!(!roles.is_empty(), "a team stage has roles");
        assert!(
            group
                .iter()
                .all(|s| s.run == run.id && s.stage_id == stage_id && s.attempt == attempt),
            "the group is one team attempt of this run"
        );
        let root = run.team_root.as_ref().expect("a team run has a team root");
        let mut members = Vec::with_capacity(roles.len());
        for (fan, role) in roles.iter().enumerate() {
            let latest = group
                .iter()
                .filter(|s| s.fan_index as usize == fan)
                .max_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)))?;
            members.push(Member {
                role: role.clone(),
                stage_run: latest.id.clone(),
                agent: latest.aether_agent.clone()?,
                running: latest.state == StageState::Running,
                worktree: root.join(team::worker_dir(stage_id, role)),
                branch: team::worker_branch(&run.branch, &run.id, role, attempt),
            });
        }
        let agents = group.iter().filter(|s| s.aether_agent.is_some()).count();
        assert!(agents >= roles.len(), "every role has an agent");
        let version = u32::try_from(agents - roles.len() + 1).expect("few agents per attempt");
        Some(Self {
            stage_id: stage_id.to_owned(),
            attempt,
            version,
            members,
        })
    }

    /// The message for `member`; the same bytes for every retry of a version.
    pub(crate) fn message_for(&self, member: &Member) -> String {
        assert!(
            self.members.contains(member),
            "a recipient is on the roster"
        );
        let mut text = format!(
            "Archon team roster v{} for team stage `{}` attempt {}. You are role `{}`.\n\n",
            self.version, self.stage_id, self.attempt, member.role
        );
        for peer in &self.members {
            text.push_str(&format!(
                "- `{}`: agent `{}`, worktree `{}`, branch `{}`\n",
                peer.role,
                peer.agent,
                peer.worktree.display(),
                peer.branch
            ));
        }
        text.push_str(
            "\nMessage a peer with Aether's `send_message` and its agent id. This roster \
             supersedes any earlier version.\n",
        );
        text
    }
}

/// One roster message still owed.
#[derive(Debug)]
struct Delivery {
    run: PipelineRun,
    stage_run: StageRunId,
    to: AetherAgentId,
    version: u32,
    content: String,
}

/// Send `run`'s current team roster to every running role that has not
/// received its version yet. Safe to repeat: sends are keyed.
pub(super) async fn deliver(
    ctx: &DaemonContext,
    run: &PipelineRunId,
) -> Result<FollowUp, ArchonError> {
    for delivery in owed(ctx, run).await? {
        send(ctx, delivery).await?;
    }
    Ok(FollowUp::default())
}

async fn owed(ctx: &DaemonContext, run_id: &PipelineRunId) -> Result<Vec<Delivery>, ArchonError> {
    let table = ctx.runs.lock().await;
    let store = ctx.store.lock().await;
    let run = store.run(run_id)?;
    let RunState::Running { stage_id, attempt } = &run.state else {
        return Ok(Vec::new());
    };
    let driver = table.drivers.get(run_id);
    let Some(roles) = driver.and_then(|d| d.team_roles(stage_id)) else {
        return Ok(Vec::new());
    };
    let group: Vec<StageRun> = store
        .stage_runs(run_id)?
        .into_iter()
        .filter(|s| s.stage_id == *stage_id && s.attempt == *attempt)
        .collect();
    let Some(roster) = Roster::of(&run, stage_id, *attempt, roles, &group) else {
        return Ok(Vec::new());
    };
    let mut owed = Vec::new();
    for member in roster.members.iter().filter(|m| m.running) {
        if sent_version(&store, &member.stage_run)? >= roster.version {
            continue;
        }
        owed.push(Delivery {
            run: run.clone(),
            stage_run: member.stage_run.clone(),
            to: member.agent.clone(),
            version: roster.version,
            content: roster.message_for(member),
        });
    }
    Ok(owed)
}

fn sent_version(store: &Store, stage: &StageRunId) -> Result<u32, ArchonError> {
    let Some(raw) = store.kv_get(&sent_key(stage))? else {
        return Ok(0);
    };
    raw.parse()
        .map_err(|e| ArchonError::Corrupt(format!("roster version of stage run {stage}: {e}")))
}

/// Send one roster message. A transient failure is left to the sweep; a
/// recipient Aether no longer knows or a refusal is not retried.
async fn send(ctx: &DaemonContext, delivery: Delivery) -> Result<(), ArchonError> {
    let key = format!("roster:{}:v{}", delivery.stage_run, delivery.version);
    let result = ctx
        .backend
        .send_message(&delivery.to, &delivery.content, &key)
        .await;
    match result {
        Ok(()) => {}
        Err(
            error @ (ArchonError::Unavailable(_)
            | ArchonError::Protocol(_)
            | ArchonError::Io { .. }),
        ) => {
            tracing::info!(to = %delivery.to, %error, "roster not sent; the next sweep retries");
            return Ok(());
        }
        Err(ArchonError::NotFound(why)) => {
            tracing::info!(to = %delivery.to, %why, "roster recipient is gone");
        }
        Err(refusal) => {
            let text = format!(
                "Aether refused team roster v{} for agent {} of run {}: {refusal}",
                delivery.version, delivery.to, delivery.run.id
            );
            settle::progress(ctx, &delivery.run, text).await?;
        }
    }
    mark_sent(
        &mut *ctx.store.lock().await,
        &delivery.stage_run,
        delivery.version,
    )
}

fn mark_sent(store: &mut Store, stage: &StageRunId, version: u32) -> Result<(), ArchonError> {
    assert!(version >= 1, "roster versions start at 1");
    let sent = sent_version(store, stage)?;
    // A concurrent delivery of the same version may have marked it already.
    store.kv_set(&sent_key(stage), &sent.max(version).to_string())
}

/// Delete the roster records of settled stage runs and of stage runs that
/// no longer exist. Returns how many went.
pub(crate) fn prune_settled_rosters(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(SENT_KEY_PREFIX)? {
        let raw = key.strip_prefix(SENT_KEY_PREFIX).expect("ranged by prefix");
        let id = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("roster key {key}: {e}")))?;
        let settled = match store.stage_run(&id) {
            Ok(stage) => stage.state.is_terminal(),
            Err(ArchonError::NotFound(_)) => true,
            Err(error) => return Err(error),
        };
        if settled {
            store.kv_delete(&key)?;
            pruned += 1;
        }
    }
    Ok(pruned)
}
