//! Report records in `kv`, one entry per report under [`KEY_PREFIX`], so a
//! change rewrites one record and settled ones can be pruned. The single
//! [`LEGACY_KEY`] blob of earlier versions is split on first use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QuestionId, QueueItemId, ReportId};
use archon_core::protocol::{ReportState, ReportSummary};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

pub const KEY_PREFIX: &str = "report:";
/// Every report in one JSON map, as versions before per-report keys kept it.
pub const LEGACY_KEY: &str = "reports";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ReportRecord {
    pub html: PathBuf,
    /// Original file, used to refresh the durable copy on republish.
    /// None for reports published before snapshots were introduced.
    #[serde(default)]
    pub source: Option<PathBuf>,
    pub title: String,
    pub item: Option<QueueItemId>,
    pub run: Option<PipelineRunId>,
    pub state: ReportState,
    /// The open "Reopen it?" question of a `Disconnected` report that an
    /// open gate waits on.
    #[serde(default)]
    pub reopen_question: Option<QuestionId>,
    /// The Lavish URL or file path it was last shown at; `None` in records
    /// written before it was kept.
    #[serde(default)]
    pub location: Option<String>,
    /// When the record last changed; a record split from the legacy blob,
    /// which has none, counts from the split.
    #[serde(default = "archon_core::now")]
    pub updated_at: Timestamp,
}

impl ReportRecord {
    pub fn summary(&self, report: &ReportId) -> ReportSummary {
        ReportSummary {
            report: report.clone(),
            title: self.title.clone(),
            path: self.html.clone(),
            location: self.location.clone(),
            state: self.state,
            item: self.item.clone(),
            reopen_question: self.reopen_question.clone(),
            updated_at: self.updated_at,
        }
    }

    /// Nothing changes the record any more: it is not polled and no
    /// question about it waits for an answer.
    fn is_settled(&self) -> bool {
        match self.state {
            ReportState::Open => false,
            ReportState::Disconnected => self.reopen_question.is_none(),
            ReportState::Unpublished | ReportState::Ended | ReportState::Stopped => true,
        }
    }

    /// `archon reports reopen` can still show it: its Lavish session ended
    /// or lost its window, and its file is still there.
    fn is_resumable(&self) -> bool {
        matches!(self.state, ReportState::Disconnected | ReportState::Ended) && self.html.is_file()
    }
}

fn key(report: &ReportId) -> String {
    format!("{KEY_PREFIX}{report}")
}

/// Every report, by id.
pub(super) fn all(store: &mut Store) -> Result<BTreeMap<ReportId, ReportRecord>, ArchonError> {
    split_legacy(store)?;
    let mut reports = BTreeMap::new();
    for (key, raw) in store.kv_prefix(KEY_PREFIX)? {
        let report = id_of_key(&key)?;
        reports.insert(report, decode(&key, &raw)?);
    }
    Ok(reports)
}

/// One report; `None` when it was never recorded or has been pruned.
pub(super) fn get(
    store: &mut Store,
    report: &ReportId,
) -> Result<Option<ReportRecord>, ArchonError> {
    split_legacy(store)?;
    let key = key(report);
    store
        .kv_get(&key)?
        .map(|raw| decode(&key, &raw))
        .transpose()
}

/// Whether `report`'s Lavish session is open and polled, so feedback on it
/// may still need action; `false` once it settled or was pruned.
pub(crate) fn is_open(store: &mut Store, report: &ReportId) -> Result<bool, ArchonError> {
    Ok(get(store, report)?.is_some_and(|record| record.state == ReportState::Open))
}

/// Write `record` under `report`, stamped with `now` as its last change.
pub(super) fn put(
    store: &mut Store,
    report: &ReportId,
    mut record: ReportRecord,
    now: Timestamp,
) -> Result<(), ArchonError> {
    assert!(record.html.is_absolute(), "report paths are validated");
    assert!(
        record.reopen_question.is_none() || record.state == ReportState::Disconnected,
        "only a disconnected report asks to reopen"
    );
    record.updated_at = now;
    store.kv_set(&key(report), &serde_json::to_string(&record)?)
}

/// Delete settled reports last changed before `cutoff`; returns how many
/// went. Open reports, pending questions and resumable reports whose file
/// exists are kept, and until a report is pruned a republished file reuses
/// its id.
pub fn prune(store: &mut Store, root: &Path, cutoff: Timestamp) -> Result<usize, ArchonError> {
    let mut pruned = 0;
    for (report, record) in all(store)? {
        if let Some(id) = &record.item {
            // Item-owned snapshots expire through filesystem GC, which checks
            // live work, gates, questions and the configurable retention first.
            let protected = match store.item(id) {
                Ok(item) => !item.state.is_terminal() || item.updated_at >= cutoff,
                Err(ArchonError::NotFound(_)) => true,
                Err(error) => return Err(error),
            };
            let owned_file = record
                .source
                .as_ref()
                .is_some_and(|source| source != &record.html)
                && record.html.starts_with(root.join(report.as_str()))
                && record
                    .html
                    .try_exists()
                    .map_err(|error| ArchonError::io("checking archived report", error))?;
            if protected || owned_file || store.open_gate_by_report(&report)?.is_some() {
                continue;
            }
        }
        // The file check last: only an old, settled record needs it.
        if record.is_settled() && record.updated_at < cutoff && !record.is_resumable() {
            if record
                .source
                .as_ref()
                .is_some_and(|source| source != &record.html)
                && record.html.starts_with(root.join(report.as_str()))
            {
                super::files::remove(root, &report)?;
            }
            store.kv_delete(&key(&report))?;
            pruned += 1;
        }
    }
    let referenced = all(store)?
        .into_values()
        .flat_map(|record| std::iter::once(record.html).chain(record.source))
        .filter_map(|path| {
            let relative = path.strip_prefix(root).ok()?;
            match relative.components().next()? {
                std::path::Component::Normal(name) => Some(root.join(name)),
                _ => None,
            }
        })
        .collect();
    super::files::prune_orphans(root, &referenced, cutoff)?;
    Ok(pruned)
}

/// Move the records of a legacy blob to their own keys, then drop it. A
/// crash midway repeats the split, which writes the same records again.
fn split_legacy(store: &mut Store) -> Result<(), ArchonError> {
    let Some(raw) = store.kv_get(LEGACY_KEY)? else {
        return Ok(());
    };
    let legacy: BTreeMap<ReportId, ReportRecord> = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("kv {LEGACY_KEY} does not decode: {e}")))?;
    for (report, record) in &legacy {
        store.kv_set(&key(report), &serde_json::to_string(record)?)?;
    }
    store.kv_delete(LEGACY_KEY)?;
    tracing::info!(count = legacy.len(), "split the legacy reports blob");
    Ok(())
}

fn id_of_key(key: &str) -> Result<ReportId, ArchonError> {
    let raw = key
        .strip_prefix(KEY_PREFIX)
        .ok_or_else(|| ArchonError::Store(format!("kv key {key} is not a report")))?;
    ReportId::parse(raw).map_err(|e| ArchonError::Store(format!("report key {key}: {e}")))
}

fn decode(key: &str, raw: &str) -> Result<ReportRecord, ArchonError> {
    serde_json::from_str(raw)
        .map_err(|e| ArchonError::Store(format!("kv {key} does not decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: ReportState) -> ReportRecord {
        ReportRecord {
            html: "/r/index.html".into(),
            source: None,
            title: "Plan".into(),
            item: None,
            run: None,
            state,
            reopen_question: None,
            location: None,
            updated_at: archon_core::now(),
        }
    }

    #[test]
    fn records_round_trip_through_their_own_keys() {
        let mut store = Store::open_in_memory().unwrap();
        let report = ReportId::new();
        let mut asked = record(ReportState::Disconnected);
        asked.reopen_question = Some(QuestionId::new());
        let now = archon_core::now() + chrono::Duration::seconds(5);
        put(&mut store, &report, asked.clone(), now).unwrap();
        asked.updated_at = now;
        let raw = store.kv_get(&key(&report)).unwrap().unwrap();
        assert!(raw.contains(r#""state":"disconnected""#), "{raw}");
        assert_eq!(get(&mut store, &report).unwrap(), Some(asked));
        assert_eq!(get(&mut store, &ReportId::new()).unwrap(), None);
        store.kv_set(&key(&report), "{").unwrap();
        assert!(matches!(
            get(&mut store, &report),
            Err(ArchonError::Store(_))
        ));
    }

    #[test]
    fn only_an_open_report_counts_as_open() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        for state in [
            ReportState::Unpublished,
            ReportState::Open,
            ReportState::Disconnected,
            ReportState::Ended,
            ReportState::Stopped,
        ] {
            let report = ReportId::new();
            put(&mut store, &report, record(state), now).unwrap();
            let open = is_open(&mut store, &report).unwrap();
            assert_eq!(open, state == ReportState::Open, "{state:?}");
        }
        assert!(!is_open(&mut store, &ReportId::new()).unwrap(), "pruned");
    }

    #[test]
    fn a_legacy_blob_is_split_into_per_report_keys() {
        let mut store = Store::open_in_memory().unwrap();
        let (open, ended) = (ReportId::new(), ReportId::new());
        let legacy = format!(
            r#"{{"{open}":{{"html":"/r/a.html","title":"A","item":null,"run":null,"state":"open"}},
                "{ended}":{{"html":"/r/b.html","title":"B","item":null,"run":null,"state":"ended"}}}}"#
        );
        store.kv_set(LEGACY_KEY, &legacy).unwrap();
        let before = archon_core::now();
        let reports = all(&mut store).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[&open].state, ReportState::Open);
        assert!(reports[&ended].updated_at >= before, "aged from the split");
        assert_eq!(store.kv_get(LEGACY_KEY).unwrap(), None);
        assert_eq!(store.kv_prefix(KEY_PREFIX).unwrap().len(), 2);
    }

    #[test]
    fn only_settled_reports_past_retention_are_pruned() {
        let mut store = Store::open_in_memory().unwrap();
        let then = archon_core::now();
        let mut asked = record(ReportState::Disconnected);
        asked.reopen_question = Some(QuestionId::new());
        let states = [
            (record(ReportState::Open), false),
            (asked, false),
            (record(ReportState::Disconnected), true),
            (record(ReportState::Unpublished), true),
            (record(ReportState::Ended), true),
            (record(ReportState::Stopped), true),
        ];
        let mut ids = Vec::new();
        for (record, settled) in states {
            let report = ReportId::new();
            put(&mut store, &report, record, then).unwrap();
            ids.push((report, settled));
        }
        let minute = chrono::Duration::minutes(1);
        assert_eq!(
            prune(&mut store, Path::new("/unused"), then - minute).unwrap(),
            0,
            "none is old"
        );
        assert_eq!(
            prune(&mut store, Path::new("/unused"), then + minute).unwrap(),
            4
        );
        for (report, settled) in ids {
            assert_eq!(get(&mut store, &report).unwrap().is_none(), settled);
        }
    }

    /// A disconnected or ended report stays resumable while its file
    /// exists, however old; once the file is gone the retention applies.
    #[test]
    fn resumable_reports_are_kept_while_their_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("index.html");
        std::fs::write(&html, "<p>plan</p>").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let then = archon_core::now();
        let states = [
            (ReportState::Disconnected, true),
            (ReportState::Ended, true),
            (ReportState::Unpublished, false),
            (ReportState::Stopped, false),
        ];
        let mut ids = Vec::new();
        for (state, resumable) in states {
            let report = ReportId::new();
            let kept = ReportRecord {
                html: html.clone(),
                ..record(state)
            };
            put(&mut store, &report, kept, then).unwrap();
            ids.push((report, resumable));
        }
        let later = then + chrono::Duration::days(365);
        assert_eq!(
            prune(&mut store, Path::new("/unused"), later).unwrap(),
            2,
            "only the others"
        );
        for (report, resumable) in &ids {
            assert_eq!(get(&mut store, report).unwrap().is_some(), *resumable);
        }

        std::fs::remove_file(&html).unwrap();
        assert_eq!(
            prune(&mut store, Path::new("/unused"), then).unwrap(),
            0,
            "still in retention"
        );
        assert_eq!(
            prune(&mut store, Path::new("/unused"), later).unwrap(),
            2,
            "the file is gone"
        );
        assert!(all(&mut store).unwrap().is_empty());
    }
}
