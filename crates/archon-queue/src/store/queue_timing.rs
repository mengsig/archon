use std::collections::BTreeMap;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::WaitingReasonCode as Code;
use archon_core::queue::{QueueItem, QueueState};
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;
use super::codec::{SqlContext, ts, write_tx};

fn reason_key(code: Code) -> &'static str {
    match code {
        Code::DependencyFailed | Code::DependencyWaiting => "dependency",
        Code::Capacity => "cap",
        Code::TotalBudget | Code::DailyBudget => "budget",
        Code::GlobalPause | Code::RepoPause | Code::PipelinePause => "pause",
        Code::ProviderQuota => "quota",
        Code::NextInLine => "ready",
        Code::AetherDisconnected
        | Code::OperatorGate
        | Code::Blocked
        | Code::Draining
        | Code::LowDisk
        | Code::HostLoad
        | Code::ModelCatalogUnavailable => "other",
    }
}

impl Store {
    /// Keep the reason from the actual admission plan, ignoring items changed
    /// since the snapshot. Unchanged observations leave their interval open.
    pub fn observe_queue_reasons(
        &mut self,
        observed: &BTreeMap<QueueItemId, Code>,
        at: Timestamp,
    ) -> Result<(), ArchonError> {
        if observed.is_empty() {
            return Ok(());
        }
        let tx = write_tx(&mut self.conn)?;
        let at = ts(&at);
        for (item, code) in observed {
            observe(&tx, item, reason_key(*code), &at)?;
        }
        tx.commit().ctx("commit queue reason observations")
    }
}

fn observe(
    conn: &Connection,
    item: &QueueItemId,
    reason: &str,
    at: &str,
) -> Result<(), ArchonError> {
    assert!(!conn.is_autocommit(), "an observation is transactional");
    let row: Option<(Option<String>, Option<String>)> = conn
        .prepare_cached(
            "SELECT q.reason, q.started_at FROM items i
             LEFT JOIN queue_reason_intervals q ON q.item = i.id AND q.ended_at IS NULL
             WHERE i.id = ?1 AND i.state = 'queued' AND i.updated_at <= ?2",
        )
        .ctx("prepare queue observation lookup")?
        .query_row(params![item.as_str(), at], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()
        .ctx("read current queue interval")?;
    let Some((current, started_at)) = row else {
        return Ok(());
    };
    assert_eq!(
        current.is_some(),
        started_at.is_some(),
        "one interval or none"
    );
    if current.as_deref() == Some(reason) || started_at.as_deref().is_some_and(|start| start > at) {
        return Ok(());
    }
    close(conn, item, at)?;
    open(conn, item, reason, at)
}

/// Called inside the item transition so queue history follows the same
/// commit as the state, including cancellation and crash recovery.
pub(super) fn item_transition(
    conn: &Connection,
    item: &QueueItem,
    before: Option<&QueueState>,
) -> Result<(), ArchonError> {
    assert!(
        !conn.is_autocommit(),
        "queue intervals change with their item"
    );
    let at = ts(&item.updated_at);
    if before == Some(&QueueState::Queued) {
        close(conn, &item.id, &at)?;
    } else if item.state == QueueState::Queued {
        let has_unmet_dependency: bool = conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM item_deps d JOIN items dep ON dep.id = d.depends_on
                                WHERE d.item = ?1 AND dep.state <> 'succeeded')",
            )
            .ctx("prepare initial queue reason lookup")?
            .query_row([item.id.as_str()], |row| row.get(0))
            .ctx("check initial queue dependencies")?;
        let reason = if has_unmet_dependency {
            "dependency"
        } else {
            "other"
        };
        open(conn, &item.id, reason, &at)?;
    }
    Ok(())
}

fn open(conn: &Connection, item: &QueueItemId, reason: &str, at: &str) -> Result<(), ArchonError> {
    let inserted = conn
        .prepare_cached(
            "INSERT INTO queue_reason_intervals (item, reason, started_at)
             VALUES (?1, ?2, max(?3, coalesce(
                 (SELECT ended_at FROM queue_reason_intervals WHERE item = ?1
                  ORDER BY seq DESC LIMIT 1), ?3
             )))",
        )
        .ctx("prepare queue interval insert")?
        .execute(params![item.as_str(), reason, at])
        .ctx("start queue interval")?;
    assert_eq!(inserted, 1, "one interval started for the item");
    Ok(())
}

fn close(conn: &Connection, item: &QueueItemId, at: &str) -> Result<(), ArchonError> {
    let changed = conn
        .prepare_cached(
            "UPDATE queue_reason_intervals SET ended_at = max(started_at, ?2)
             WHERE item = ?1 AND ended_at IS NULL",
        )
        .ctx("prepare queue interval close")?
        .execute(params![item.as_str(), at])
        .ctx("finish queue interval")?;
    assert!(changed <= 1, "only the current interval can close");
    Ok(())
}
