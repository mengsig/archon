//! Subscribe before reading state, so a completion cannot fall between them.

use std::process::ExitCode;
use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::ArchonError;
use archon_core::age::Age;
use archon_core::id::QueueItemId;
use archon_core::protocol::{Reply, Request, WaitingReasonCode};
use archon_core::queue::QueueItem;
use tokio::time::{Instant, timeout_at};

use crate::cmd::emit;
use crate::connect::{connect_running, unexpected};
use crate::views;

pub async fn run(items: Vec<String>, timeout: &str, json: bool) -> anyhow::Result<ExitCode> {
    let duration = Age::parse(timeout)?.duration();
    if duration.is_zero() {
        return Err(ArchonError::Invalid("--timeout must be greater than zero".into()).into());
    }
    let deadline = Instant::now()
        .checked_add(duration)
        .ok_or_else(|| ArchonError::Invalid("--timeout is too long".into()))?;
    let mut rows = Vec::new();
    let mut blocked_dependencies = Vec::new();
    let timed_out = match timeout_at(
        deadline,
        observe(&items, &mut rows, &mut blocked_dependencies),
    )
    .await
    {
        Ok(result) => {
            result?;
            false
        }
        Err(_) => true,
    };
    let text = rows
        .iter()
        .map(|item: &QueueItem| format!("{} {}", item.id, item.state.kind()))
        .chain(blocked_dependencies.iter().cloned())
        .chain(timed_out.then(|| "Timed out waiting for queue items.".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");
    emit(
        json,
        || {
            Ok(
                serde_json::json!({"timed_out": timed_out, "items": rows, "blocked_dependencies": blocked_dependencies}),
            )
        },
        &text,
    )?;
    let code = if timed_out {
        4
    } else if !blocked_dependencies.is_empty() {
        5
    } else if rows.iter().all(|item| item.state.kind() == "succeeded") {
        0
    } else {
        3
    };
    Ok(ExitCode::from(code))
}

async fn observe(
    items: &[String],
    rows: &mut Vec<QueueItem>,
    blocked_dependencies: &mut Vec<String>,
) -> anyhow::Result<()> {
    // Dedicated autostart takes blocking process/file locks that cannot be
    // cancelled by the deadline. Waiting only observes an existing daemon.
    let client = connect_running().await?;
    let mut ids = Vec::with_capacity(items.len());
    for item in items {
        ids.push(views::resolve_item(&client, item).await?);
    }
    let mut feed = client.subscribe_feed(None, true).await?;
    // Reconcile periodically too: a feed notice is a wakeup, not the state itself.
    let period = Duration::from_secs(1);
    let mut reconcile = tokio::time::interval_at(Instant::now() + period, period);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        refresh(&client, &ids, rows, blocked_dependencies).await?;
        if !blocked_dependencies.is_empty() || rows.iter().all(|item| item.state.is_terminal()) {
            return Ok(());
        }
        tokio::select! {
            entry = feed.items.recv() => {
                if entry.is_none() {
                    anyhow::bail!("the feed stopped: {}", feed.close_reason());
                }
            }
            _ = reconcile.tick() => {}
        }
    }
}

async fn refresh(
    client: &ArchonClient,
    ids: &[QueueItemId],
    rows: &mut Vec<QueueItem>,
    blocked_dependencies: &mut Vec<String>,
) -> anyhow::Result<()> {
    blocked_dependencies.clear();
    for (index, id) in ids.iter().enumerate() {
        let reply = client
            .request(Request::GetItem { item: id.clone() })
            .await?;
        let Reply::ItemDetail(detail) = reply else {
            return Err(unexpected(&reply, "item_detail"));
        };
        assert_eq!(&detail.item.id, id, "the daemon answers for the item asked");
        if let Some(reason) = &detail.waiting_reason
            && reason.code == WaitingReasonCode::DependencyFailed
        {
            blocked_dependencies.push(format!(
                "{}: {}",
                detail.item.id,
                crate::wrap::sanitize_line(&reason.message)
            ));
        }
        if index < rows.len() {
            rows[index] = detail.item;
        } else {
            rows.push(detail.item);
        }
    }
    assert_eq!(rows.len(), ids.len(), "one observation per item");
    Ok(())
}
