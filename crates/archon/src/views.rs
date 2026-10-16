//! Fetch-and-format helpers shared by the CLI commands and the chat's
//! slash commands, so `/status` and `archon status` read the same.

use archon_client::ArchonClient;
use archon_core::id::{QueueItemId, queue_item_prefix};
use archon_core::protocol::{DaemonStatus, ItemQuery, ItemSummary, Reply, Request};
use archon_core::queue::QueueState;
use archon_core::repo::Repo;

use crate::connect::unexpected;
use crate::format;

/// Lower-case and check state filters, so a typo is an error rather than
/// an empty listing.
pub fn parse_states(states: &[String]) -> anyhow::Result<Vec<String>> {
    let parsed: Vec<String> = states
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    if let Some(bad) = parsed
        .iter()
        .find(|s| !QueueState::KINDS.contains(&s.as_str()))
    {
        return Err(archon_core::ArchonError::Invalid(format!(
            "unknown queue state {bad:?}; use one of {}",
            QueueState::KINDS.join(", ")
        ))
        .into());
    }
    assert_eq!(parsed.len(), states.len(), "every filter is kept");
    Ok(parsed)
}

/// The item `text` names: a whole id, or the start of one, either with or
/// without `qi_` and in any case. archond resolves a start in one request.
pub async fn resolve_item(client: &ArchonClient, text: &str) -> anyhow::Result<QueueItemId> {
    let prefix = queue_item_prefix(text)?;
    if let Ok(id) = QueueItemId::parse(&prefix) {
        assert_eq!(id.as_str(), prefix, "a whole id is kept as stored");
        return Ok(id);
    }
    let item = match client
        .request(Request::ResolveItem {
            prefix: prefix.clone(),
        })
        .await?
    {
        Reply::Item(item) => item,
        other => return Err(unexpected(&other, "item")),
    };
    assert!(
        item.id.as_str().starts_with(&prefix),
        "archond resolved {prefix} to {}",
        item.id
    );
    Ok(item.id)
}

pub async fn fetch_status(client: &ArchonClient) -> anyhow::Result<DaemonStatus> {
    match client.request(Request::Status(Default::default())).await? {
        Reply::Status(status) => Ok(*status),
        other => Err(unexpected(&other, "status")),
    }
}

pub async fn fetch_repos(client: &ArchonClient) -> anyhow::Result<Vec<Repo>> {
    match client.request(Request::RepoList).await? {
        Reply::Repos(repos) => Ok(repos),
        other => Err(unexpected(&other, "repos")),
    }
}

pub async fn fetch_summaries(
    client: &ArchonClient,
    query: ItemQuery,
) -> anyhow::Result<Vec<ItemSummary>> {
    match client.request(Request::ListItemSummaries(query)).await? {
        Reply::ItemSummaries(rows) => Ok(rows),
        other => Err(unexpected(&other, "item_summaries")),
    }
}

pub async fn status_text(client: &ArchonClient) -> anyhow::Result<String> {
    let mut status = fetch_status(client).await?;
    // Older daemons have no waiting_items field; preserve their approval view.
    if status.waiting_items.is_empty() && (status.open_questions > 0 || status.open_gates > 0) {
        status.waiting_items = fetch_summaries(
            client,
            ItemQuery {
                states: vec!["running".into()],
                limit: Some(usize::MAX),
                ..ItemQuery::default()
            },
        )
        .await?
        .into_iter()
        .filter(|row| row.waiting_on.is_some())
        .collect();
    }
    let names = format::repo_names(&fetch_repos(client).await?);
    Ok(format::status(&status, &names))
}

pub async fn queue_text(client: &ArchonClient, query: ItemQuery) -> anyhow::Result<String> {
    let rows = fetch_summaries(client, query).await?;
    let names = format::repo_names(&fetch_repos(client).await?);
    Ok(format::items(&rows, &names, archon_core::now()))
}

/// `full` shows every stage's whole summary instead of its first lines.
pub async fn item_text(
    client: &ArchonClient,
    item: QueueItemId,
    full: bool,
) -> anyhow::Result<String> {
    let detail = match client
        .request(Request::GetItem { item: item.clone() })
        .await?
    {
        Reply::ItemDetail(detail) => detail,
        other => return Err(unexpected(&other, "item_detail")),
    };
    assert_eq!(detail.item.id, item, "archond described the item asked for");
    let names = format::repo_names(&fetch_repos(client).await?);
    Ok(format::item_detail(&detail, &names, full))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_are_checked_and_normalised() {
        let parsed = parse_states(&["Running".to_owned(), " failed".to_owned()]).unwrap();
        assert_eq!(parsed, ["running", "failed"]);
        let error = parse_states(&["runing".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("\"runing\""));
        assert!(parse_states(&[]).unwrap().is_empty());
    }
}
