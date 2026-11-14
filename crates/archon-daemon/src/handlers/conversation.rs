//! `conversation`: the important entries of the orchestrator's transcript,
//! read from Aether, so `archon chat` and the phone show what the Aether
//! TUI's `/focus` shows. With `wait` it is a long poll for what follows.

use std::time::Duration;

use archon_core::conversation::ConversationPage;
use archon_core::error::ArchonError;
use archon_core::protocol::Reply;
use tokio::time::Instant;

use crate::context::DaemonContext;
use crate::supervisor;

/// Longest a waiting read is held; well under a client's request timeout.
pub const LONG_POLL: Duration = Duration::from_secs(20);
/// Most events one page reads, as Aether caps it.
const MAX_LIMIT: u32 = 1000;

pub(super) async fn conversation(
    ctx: &DaemonContext,
    after: Option<u64>,
    before: Option<u64>,
    limit: Option<u32>,
    wait: bool,
) -> Result<Reply, ArchonError> {
    if limit.is_some_and(|limit| limit == 0 || limit > MAX_LIMIT) {
        return Err(ArchonError::Invalid(format!(
            "limit must be 1 to {MAX_LIMIT}"
        )));
    }
    if wait && (after.is_none() || before.is_some()) {
        return Err(ArchonError::Invalid(
            "wait needs `after` and no `before`: it waits for what follows".into(),
        ));
    }
    let page = match wait {
        true => wait_after(ctx, after.unwrap_or(0), limit).await?,
        false => read(ctx, after, before, limit).await?,
    };
    assert!(page.is_ordered(), "a page is ordered inside its span");
    Ok(Reply::Conversation(page))
}

/// The first page after `cursor` that holds an entry, or an empty one whose
/// `to_seq` is the cursor reached once [`LONG_POLL`] passes.
async fn wait_after(
    ctx: &DaemonContext,
    mut cursor: u64,
    limit: Option<u32>,
) -> Result<ConversationPage, ArchonError> {
    let deadline = Instant::now() + LONG_POLL;
    let mut grown = ctx.conversation.subscribe();
    loop {
        // Marked seen before the read, so a change during it wakes the wait.
        grown.borrow_and_update();
        let page = read(ctx, Some(cursor), None, limit).await?;
        assert!(
            page.entries.iter().all(|e| e.seq > cursor),
            "entries follow the cursor"
        );
        let advanced = page.to_seq > cursor;
        cursor = cursor.max(page.to_seq);
        if page.entries.is_empty() && page.has_after && advanced {
            continue;
        }
        if !page.entries.is_empty() || page.has_after {
            return Ok(ConversationPage {
                to_seq: cursor,
                ..page
            });
        }
        let woken = tokio::time::timeout_at(deadline, grown.changed()).await;
        if !matches!(woken, Ok(Ok(()))) {
            return Ok(ConversationPage {
                to_seq: cursor,
                ..page
            });
        }
    }
}

/// One page of the current orchestrator's conversation; empty before one exists.
async fn read(
    ctx: &DaemonContext,
    after: Option<u64>,
    before: Option<u64>,
    limit: Option<u32>,
) -> Result<ConversationPage, ArchonError> {
    let client = ctx.aether.client()?;
    let Some(orchestrator) = supervisor::current_orchestrator(ctx).await? else {
        return Ok(ConversationPage::default());
    };
    client
        .conversation(&orchestrator, after, before, limit)
        .await
}
