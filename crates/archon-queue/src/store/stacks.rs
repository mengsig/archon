//! Durable ancestry and merge progress for stacked item branches.

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::Store;
use super::codec::{SqlContext, from_json, to_json, ts, write_tx};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackParent {
    pub item: QueueItemId,
    pub parent_item: QueueItemId,
    pub parent_branch: String,
    pub parent_sha: String,
    pub position: usize,
    #[serde(default)]
    pub repository_base: Option<String>,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub conflict: Option<String>,
}

impl Store {
    /// The first unmerged parent's branch and last incorporated SHA.
    pub fn stack_base(&self, item: &QueueItemId) -> Result<Option<(String, String)>, ArchonError> {
        Ok(self
            .stack_parents(item)?
            .into_iter()
            .find(|p| !p.merged)
            .map(|p| (p.parent_branch, p.parent_sha)))
    }

    pub fn stack_parents(&self, item: &QueueItemId) -> Result<Vec<StackParent>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM stacks WHERE item=?1 ORDER BY position")
            .ctx("read stack")?;
        let rows = stmt
            .query_map([item.as_str()], |row| row.get::<_, String>(0))
            .ctx("read stack")?;
        let parents: Vec<StackParent> = rows
            .map(|row| from_json(&row.ctx("read stack parent")?, "stack parent"))
            .collect::<Result<_, _>>()?;
        assert!(parents.iter().all(|p| &p.item == item));
        assert!(
            parents
                .windows(2)
                .all(|pair| pair[0].position < pair[1].position)
        );
        Ok(parents)
    }

    pub fn stacked_items(&self) -> Result<Vec<QueueItemId>, ArchonError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT DISTINCT item FROM stacks ORDER BY item")
            .ctx("list stacks")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .ctx("list stacks")?;
        rows.map(|row| QueueItemId::parse(&row.ctx("read stacked item")?))
            .collect()
    }

    pub fn save_stack_parent(&mut self, parent: &StackParent) -> Result<(), ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        validate_parent(&tx, parent)?;
        write_parent(&tx, parent)?;
        tx.commit().ctx("commit stack parent")
    }

    pub fn replace_stack_parents(
        &mut self,
        item: &QueueItemId,
        parents: &[StackParent],
    ) -> Result<(), ArchonError> {
        let tx = write_tx(&mut self.conn)?;
        for (position, parent) in parents.iter().enumerate() {
            assert_eq!(&parent.item, item);
            assert_eq!(parent.position, position);
            validate_parent(&tx, parent)?;
        }
        tx.execute("DELETE FROM stacks WHERE item=?1", [item.as_str()])
            .ctx("replace stack")?;
        for parent in parents {
            write_parent(&tx, parent)?;
        }
        tx.commit().ctx("commit stack ancestry")
    }
}

/// An item that works on another item's branch: a restack fix, or a refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFix {
    pub owner: QueueItemId,
    pub branch: String,
    pub team: bool,
    /// Set for a refresh: the base commit it must merge before it ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<Refresh>,
}

/// The fetched head of the base branch a refresh merges into the owner's branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refresh {
    pub base: String,
    pub sha: String,
}

impl Store {
    pub fn has_pending_stack_fix(&self, owner: &QueueItemId) -> Result<bool, ArchonError> {
        for (_, id) in self.kv_prefix(&format!("restack_fix:{owner}:"))? {
            if !self.item(&QueueItemId::parse(&id)?)?.state.is_terminal() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn stack_fix(&self, item: &QueueItemId) -> Result<Option<StackFix>, ArchonError> {
        self.kv_get(&format!("restack_target:{item}"))?
            .map(|raw| from_json(&raw, "restack target"))
            .transpose()
    }

    pub fn enqueue_stack_fix(
        &mut self,
        child: &archon_core::queue::QueueItem,
        run: &archon_core::run::PipelineRun,
        parent: &StackParent,
        sha: &str,
        conflict: &str,
    ) -> Result<QueueItemId, ArchonError> {
        use super::{items, settings};
        use archon_core::queue::QueueState;
        assert_eq!(run.item, child.id);
        assert_eq!(parent.item, child.id);
        let key = format!("restack_fix:{}:{}:{sha}", child.id, parent.parent_item);
        if let Some(id) = self.kv_get(&key)? {
            return QueueItemId::parse(&id);
        }
        let mut fix = child.clone();
        fix.id = QueueItemId::new();
        fix.title = format!("Restack {}", child.id);
        fix.brief = format!(
            "Merge {sha}, the head of `{}`, into `{}` with `git merge {sha}`: Archon's own \
             merge of it conflicted, so nothing of it is on the branch yet. Conflicting \
             paths:\n{conflict}\nResolve them, commit the merge on the existing branch and \
             validate the resolution. It ships as this run's pipeline ships, into the pull \
             request the branch already has: never open a second one, and never rebase or \
             force-push.",
            parent.parent_branch, run.branch
        );
        fix.dependencies.clear();
        fix.initiative = None;
        fix.origin = None;
        fix.state = QueueState::Queued;
        fix.created_at = archon_core::now();
        fix.updated_at = fix.created_at;
        let target = StackFix {
            owner: child.id.clone(),
            branch: run.branch.clone(),
            team: run.team_root.is_some(),
            refresh: None,
        };
        let tx = write_tx(&mut self.conn)?;
        items::insert_in(&tx, &fix, None, &Default::default())?;
        settings::kv_set(&tx, &key, fix.id.as_str())?;
        settings::kv_set(
            &tx,
            &format!("restack_target:{}", fix.id),
            &to_json(&target, "stack fix")?,
        )?;
        tx.commit().ctx("enqueue restack fix")?;
        Ok(fix.id)
    }
}

impl Store {
    /// Queue a refresh of `owner`, whose last run is `run`: an item on its branch
    /// that merges `refresh.sha` and ships into the same pull request. The one
    /// still to finish for that commit is returned again; another unfinished
    /// restack fix or refresh of `owner` refuses it. The refresh of an `onto`
    /// owner merges its pull request's head: that becomes the refresh's frozen head.
    pub fn enqueue_refresh(
        &mut self,
        owner: &archon_core::queue::QueueItem,
        run: &archon_core::run::PipelineRun,
        refresh: &Refresh,
    ) -> Result<archon_core::queue::QueueItem, ArchonError> {
        use super::{items, settings};
        use archon_core::queue::QueueState;
        assert_eq!(run.item, owner.id, "the owner's own run");
        assert!(!run.branch.is_empty(), "a refresh works on a branch");
        let key = format!("restack_fix:{}:refresh:{}", owner.id, refresh.sha);
        if let Some(id) = self.kv_get(&key)? {
            let earlier = self.item(&QueueItemId::parse(&id)?)?;
            if !earlier.state.is_terminal() {
                return Ok(earlier);
            }
        }
        if self.has_pending_stack_fix(&owner.id)? {
            return Err(ArchonError::InvalidTransition(format!(
                "{} has a restack fix or refresh still to finish, which works on its branch",
                owner.id
            )));
        }
        let mut fix = owner.clone();
        fix.id = QueueItemId::new();
        fix.title = format!("Refresh {} onto {}", owner.id, refresh.base);
        fix.brief = refresh_brief(owner, &run.branch, refresh);
        if let Some(onto) = fix.onto.as_mut() {
            assert_eq!(
                onto.head_ref, refresh.base,
                "an onto refresh merges its head"
            );
            onto.head_sha.clone_from(&refresh.sha);
        }
        fix.dependencies.clear();
        fix.initiative = None;
        fix.origin = None;
        fix.fresh_retries = 0;
        fix.state = QueueState::Queued;
        fix.created_at = archon_core::now();
        fix.updated_at = fix.created_at;
        let target = StackFix {
            owner: owner.id.clone(),
            branch: run.branch.clone(),
            team: run.team_root.is_some(),
            refresh: Some(refresh.clone()),
        };
        let tx = write_tx(&mut self.conn)?;
        let queued = items::insert_in(&tx, &fix, None, &Default::default())?;
        settings::kv_set(&tx, &key, fix.id.as_str())?;
        settings::kv_set(
            &tx,
            &format!("restack_target:{}", fix.id),
            &to_json(&target, "stack fix")?,
        )?;
        tx.commit().ctx("enqueue refresh")?;
        assert_eq!(queued.id, fix.id, "a new item, no replay");
        Ok(queued)
    }
}

/// What a refresh's agent is asked: merge the base in, resolve, check, and
/// have the change reviewed from where it now leaves the base.
fn refresh_brief(owner: &archon_core::queue::QueueItem, branch: &str, refresh: &Refresh) -> String {
    let (base, sha) = (&refresh.base, &refresh.sha);
    format!(
        "Refresh `{branch}` onto `{base}`: merge {sha}, the head of `{base}` Archon just \
         fetched, into the branch with `git merge {sha}`. Resolve any \
         conflicts, commit the merge on the existing branch and run the checks again. It ships \
         as this run's pipeline ships, into the pull request the branch already has: never \
         open a second one, and never rebase or force-push. Archon delivers it only once the \
         branch holds {sha}. The review reads the change from its new merge base \
         (`git merge-base {sha} HEAD`) to the head, conflict resolutions included.\n\n\
         The item refreshed, {}: {}\n\n{}",
        owner.id, owner.title, owner.brief
    )
}

fn validate_parent(conn: &rusqlite::Connection, parent: &StackParent) -> Result<(), ArchonError> {
    let child = super::items::load(conn, &parent.item)?;
    let ancestor = super::items::load(conn, &parent.parent_item)?;
    if child.repo.is_none()
        || child.repo != ancestor.repo
        || !child.dependencies.contains(&ancestor.id)
    {
        return Err(ArchonError::Invalid(
            "a stack parent must be a same-repository dependency".into(),
        ));
    }
    if parent.parent_branch.is_empty()
        || parent.parent_branch.starts_with('-')
        || parent.parent_branch.chars().any(char::is_control)
        || !matches!(parent.parent_sha.len(), 40 | 64)
        || !parent.parent_sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ArchonError::Invalid(
            "invalid stack parent branch or commit".into(),
        ));
    }
    assert_ne!(parent.item, parent.parent_item);
    assert_eq!(child.repo, ancestor.repo);
    if parent.position >= child.dependencies.len() {
        return Err(ArchonError::Invalid(
            "stack position exceeds dependency count".into(),
        ));
    }
    Ok(())
}

fn write_parent(conn: &rusqlite::Connection, parent: &StackParent) -> Result<(), ArchonError> {
    assert!(!conn.is_autocommit());
    assert!(matches!(parent.parent_sha.len(), 40 | 64));
    let state = if parent.merged {
        "merged"
    } else if parent.conflict.is_some() {
        "conflict"
    } else {
        "open"
    };
    conn.execute("INSERT INTO stacks (item,parent_item,position,parent_branch,parent_sha,state,updated_at,data)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(item,parent_item) DO UPDATE SET
            position=excluded.position,parent_branch=excluded.parent_branch,parent_sha=excluded.parent_sha,
            state=excluded.state,updated_at=excluded.updated_at,data=excluded.data",
            params![parent.item.as_str(), parent.parent_item.as_str(), parent.position as i64,
                parent.parent_branch, parent.parent_sha, state, ts(&archon_core::now()), to_json(parent,"stack parent")?])
            .ctx("save stack parent")?;
    Ok(())
}
