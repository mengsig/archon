//! Expiry removes only Archon's owned copies. Source pages and store history stay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::protocol::{GcReport, Reclaimed, ReportState};
use archon_queue::Store;

use super::{files, records};
use crate::gc::cache::disk_usage;

/// Caller holds report_files and the store throughout the safety checks and
/// deletion, excluding publication, reopen, item retries and gate changes.
pub(crate) fn expire_files(
    store: &mut Store,
    root: &Path,
    cutoff: Timestamp,
) -> Result<GcReport, ArchonError> {
    let records = records::all(store)?;
    let gates = store.open_gates()?;
    let questions = store.questions(true)?;
    let mut referenced = BTreeMap::<_, BTreeSet<_>>::new();
    for (id, record) in &records {
        for path in std::iter::once(&record.html).chain(record.source.iter()) {
            if let Ok(relative) = path.strip_prefix(root)
                && let Some(std::path::Component::Normal(name)) = relative.components().next()
            {
                referenced.entry(root.join(name)).or_default().insert(id);
            }
        }
    }
    let mut result = GcReport::default();
    for (id, record) in &records {
        if record.updated_at >= cutoff
            || record.state == ReportState::Open
            || record.reopen_question.is_some()
        {
            continue;
        }
        let Some(item_id) = &record.item else {
            continue;
        };
        let item = match store.item(item_id) {
            Ok(item) => item,
            Err(ArchonError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        if !item.state.is_terminal()
            || item.updated_at >= cutoff
            || store.has_pending_stack_fix(item_id)?
            || store
                .runs_for_item(item_id)?
                .iter()
                .any(|run| run.finished_at.is_none_or(|at| at >= cutoff))
            || gates
                .iter()
                .any(|gate| &gate.item == item_id || gate.report.as_ref() == Some(id))
            || questions
                .iter()
                .any(|question| question.item.as_ref() == Some(item_id))
        {
            continue;
        }
        let dir = root.join(id.as_str());
        if !record.html.starts_with(&dir)
            || !record
                .source
                .as_ref()
                .is_some_and(|source| source != &record.html)
            || referenced
                .get(&dir)
                .is_some_and(|ids| ids.iter().any(|other| *other != id))
        {
            continue;
        }
        let removal = || -> Result<Option<u64>, ArchonError> {
            match std::fs::symlink_metadata(&dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(ArchonError::io("reading report snapshot", error)),
                Ok(_) => {}
            }
            let bytes = disk_usage(&dir)?;
            files::remove(root, id)?;
            Ok(Some(bytes))
        };
        match removal() {
            Ok(Some(bytes)) => {
                result.bytes_freed = result.bytes_freed.saturating_add(bytes);
                result.reports.push(Reclaimed { path: dir, bytes });
            }
            Ok(None) => {}
            Err(error) => result.failed.push((dir, error.to_string())),
        }
    }
    Ok(result)
}
