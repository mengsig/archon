//! A note Archon records on a stage run as it spawns, shown by `get_item`:
//! such as a lead whose planned workers the model policy left it none of.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::run::StageRun;
use archon_queue::Store;

const KEY_PREFIX: &str = "stage_note:";

pub(crate) fn key(stage: &StageRunId) -> String {
    format!("{KEY_PREFIX}{stage}")
}

/// The note of each of `stages` that has one.
pub(crate) fn read(
    store: &Store,
    stages: &[StageRun],
) -> Result<BTreeMap<StageRunId, String>, ArchonError> {
    let mut notes = BTreeMap::new();
    for stage in stages {
        if let Some(note) = store.kv_get(&key(&stage.id))? {
            assert!(!note.is_empty(), "an empty note is never recorded");
            notes.insert(stage.id.clone(), note);
        }
    }
    assert!(notes.len() <= stages.len());
    Ok(notes)
}

/// Delete the notes of stage runs the store no longer holds; returns how many.
pub(crate) fn prune_orphans(store: &mut Store) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (key, _) in store.kv_prefix(KEY_PREFIX)? {
        let raw = key.strip_prefix(KEY_PREFIX).expect("ranged by prefix");
        let id = StageRunId::parse(raw)
            .map_err(|e| ArchonError::Corrupt(format!("stage note key {key}: {e}")))?;
        match store.stage_run(&id) {
            Ok(_) => continue,
            Err(ArchonError::NotFound(_)) => {
                store.kv_delete(&key)?;
                pruned += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(pruned)
}
