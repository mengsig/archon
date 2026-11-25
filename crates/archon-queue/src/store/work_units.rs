use std::collections::HashMap;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::WorkUnitCounts;
use archon_core::queue::QueueState;

use super::Store;
use super::codec::SqlContext;

struct WorkItem {
    id: QueueItemId,
    supersedes: Option<QueueItemId>,
    active: bool,
}

impl Store {
    /// Count connected supersession chains among all retained items.
    pub fn work_unit_counts(&self) -> Result<WorkUnitCounts, ArchonError> {
        let items = work_items(self)?;
        Ok(count(&items))
    }
}

fn work_items(store: &Store) -> Result<Vec<WorkItem>, ArchonError> {
    let mut stmt = store
        .conn
        .prepare("SELECT id, state, json_extract(data, '$.supersedes'), json_extract(data, '$.state.state') FROM items")
        .ctx("prepare work-unit items")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .ctx("query work-unit items")?;
    let mut items = Vec::new();
    for row in rows {
        let (id, state, supersedes, data_state) = row.ctx("read work-unit item")?;
        if !QueueState::KINDS.contains(&state.as_str()) {
            return Err(ArchonError::Corrupt(format!(
                "unknown state {state:?} for item {id}"
            )));
        }
        if data_state.as_deref() != Some(state.as_str()) {
            return Err(ArchonError::Corrupt(format!(
                "state disagrees with item {id}"
            )));
        }
        items.push(WorkItem {
            id: QueueItemId::parse(&id)?,
            supersedes: supersedes.as_deref().map(QueueItemId::parse).transpose()?,
            active: !matches!(state.as_str(), "succeeded" | "failed" | "cancelled"),
        });
    }
    Ok(items)
}

fn root(parents: &mut [usize], mut index: usize) -> usize {
    assert!(
        index < parents.len(),
        "index belongs to the component forest"
    );
    while parents[index] != index {
        assert!(
            parents[index] < parents.len(),
            "parent belongs to the component forest"
        );
        parents[index] = parents[parents[index]];
        index = parents[index];
    }
    index
}

fn count(items: &[WorkItem]) -> WorkUnitCounts {
    let mut ids: HashMap<&QueueItemId, usize> = HashMap::new();
    for item in items {
        let next = ids.len();
        assert!(ids.insert(&item.id, next).is_none(), "item ids are unique");
    }
    let retained = ids.len();
    for target in items.iter().filter_map(|item| item.supersedes.as_ref()) {
        let next = ids.len();
        ids.entry(target).or_insert(next);
    }
    let mut parents: Vec<usize> = (0..ids.len()).collect();
    for item in items {
        if let Some(target) = &item.supersedes {
            let from = root(&mut parents, ids[&item.id]);
            let to = root(&mut parents, ids[target]);
            parents[from] = to;
        }
    }
    let mut components: HashMap<usize, bool> = HashMap::new();
    for item in items {
        let component = root(&mut parents, ids[&item.id]);
        let active = components.entry(component).or_insert(false);
        *active |= item.active;
    }
    let active = components.values().filter(|&&active| active).count();
    let finished = components.len() - active;
    assert!(
        components.len() <= retained,
        "each component has a retained item"
    );
    WorkUnitCounts {
        active: active as u64,
        finished: finished as u64,
        missing_targets: (ids.len() - retained) as u64,
    }
}
