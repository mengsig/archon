use std::collections::HashMap;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{ItemQuery, WorkUnitCounts};
use archon_core::queue::QueueItem;

use super::Store;

impl Store {
    /// Count connected supersession chains among all retained items.
    pub fn work_unit_counts(&self) -> Result<WorkUnitCounts, ArchonError> {
        let items = self.items(&ItemQuery {
            limit: Some(usize::MAX),
            ..Default::default()
        })?;
        Ok(count(&items))
    }
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

fn count(items: &[QueueItem]) -> WorkUnitCounts {
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
        *active |= !item.state.is_terminal();
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
