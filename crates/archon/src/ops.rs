//! The operator's requests to archond, shared by the one-shot commands and
//! the chat's slash commands: each sends one request and checks the reply,
//! and the caller says what happened in its own words.

use archon_client::ArchonClient;
use archon_core::id::QueueItemId;
use archon_core::integration::Promotion;
use archon_core::merge::MergeRequest;
use archon_core::notify::Question;
use archon_core::protocol::{PauseTarget, Reply, RepoRef, Request};

use crate::connect::unexpected;

/// Send one line to the orchestrator.
pub async fn say(client: &ArchonClient, text: String) -> anyhow::Result<()> {
    assert!(!text.trim().is_empty(), "blank lines are never sent");
    match client.call(Request::Say { text }).await? {
        Ok(Reply::Ok) => Ok(()),
        Ok(other) => Err(unexpected(&other, "ok")),
        Err(refusal) => Err(archon_client::refusal_error(refusal).into()),
    }
}

/// Answer `question` (`chat` when typed in the chat); a bare option number picks that option; a number outside the options is refused. Returns the
/// answer as sent.
pub async fn answer(
    client: &ArchonClient,
    question: &Question,
    text: &str,
    chat: bool,
) -> anyhow::Result<String> {
    assert!(!text.trim().is_empty(), "callers refuse empty answers");
    let text = question.expand_option(text)?;
    // Options come from the asker, so an empty one is refused, not sent.
    if text.is_empty() {
        return Err(archon_core::ArchonError::Invalid(
            "that option is empty; answer in words".into(),
        )
        .into());
    }
    let request = Request::Answer {
        chat,
        question: question.id.clone(),
        text: text.clone(),
    };
    match client.request(request).await? {
        Reply::Ok | Reply::Question(_) => Ok(text),
        other => Err(unexpected(&other, "ok")),
    }
}

/// Pause `target`, or resume it when `pause` is false.
pub async fn pause(client: &ArchonClient, target: PauseTarget, pause: bool) -> anyhow::Result<()> {
    let request = match pause {
        true => Request::Pause(target),
        false => Request::Resume(target),
    };
    match client.request(request).await? {
        Reply::Ok => Ok(()),
        other => Err(unexpected(&other, "ok")),
    }
}

/// Have archond merge `item`'s pull request now; `chat` records that the
/// chat's `/merge` asked. The request comes back however it ended.
pub async fn promote(client: &ArchonClient, repo: RepoRef) -> anyhow::Result<Promotion> {
    match client.request(Request::Promote { repo }).await? {
        Reply::Promotion(promotion) => Ok(*promotion),
        other => Err(unexpected(&other, "promote")),
    }
}

pub async fn merge(
    client: &ArchonClient,
    item: QueueItemId,
    ignore_checks: bool,
    chat: bool,
) -> anyhow::Result<MergeRequest> {
    let request = Request::MergeItem {
        item,
        ignore_checks,
        chat,
    };
    match client.request(request).await? {
        Reply::Merge(merge) => Ok(*merge),
        other => Err(unexpected(&other, "merge")),
    }
}
