//! An item's steer history: who changed what, kept with the item and shown
//! by `archon queue show` and `get_item`.

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::steer::{MAX_STEER_RECORDS, SteerRecord};
use archon_queue::Store;

/// `kv`: `steers:<item>` → the JSON list of [`SteerRecord`], oldest first.
const KEY_PREFIX: &str = "steers:";

fn key(item: &QueueItemId) -> String {
    format!("{KEY_PREFIX}{item}")
}

/// Every steer recorded for `item`, oldest first.
pub(crate) fn records(store: &Store, item: &QueueItemId) -> Result<Vec<SteerRecord>, ArchonError> {
    let Some(raw) = store.kv_get(&key(item))? else {
        return Ok(Vec::new());
    };
    let records: Vec<SteerRecord> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("steer history of item {item}: {e}")))?;
    assert!(records.len() <= MAX_STEER_RECORDS, "the history is capped");
    assert!(
        records.windows(2).all(|pair| pair[0].at <= pair[1].at),
        "steers are recorded in order"
    );
    Ok(records)
}

/// Keep steer history bounded. Scope and effort changes carry no quality judgment;
/// explicit feedback records corrections separately.
pub(crate) fn record(
    store: &mut Store,
    item: &QueueItemId,
    run: Option<&PipelineRunId>,
    mut record: SteerRecord,
) -> Result<(), ArchonError> {
    assert!(!record.what.trim().is_empty(), "a steer changed something");
    if let Some(run) = run {
        assert_eq!(
            &store.run(run)?.item,
            item,
            "a steer belongs to its item's run"
        );
    }
    let mut all = records(store, item)?;
    // The wall clock may step back; the history stays in recorded order.
    record.at = all.last().map_or(record.at, |last| record.at.max(last.at));
    all.push(record);
    let excess = all.len().saturating_sub(MAX_STEER_RECORDS);
    all.drain(..excess);
    store.kv_set(&key(item), &serde_json::to_string(&all)?)
}

/// The notes steered into `item` after `since`, oldest first: what an agent
/// that was not working then has not been told.
pub(crate) fn notes_since(
    store: &Store,
    item: &QueueItemId,
    since: Timestamp,
) -> Result<Vec<String>, ArchonError> {
    let notes: Vec<String> = records(store, item)?
        .into_iter()
        .filter(|record| record.at > since)
        .filter_map(|record| record.note)
        .collect();
    assert!(notes.iter().all(|note| !note.trim().is_empty()));
    Ok(notes)
}
