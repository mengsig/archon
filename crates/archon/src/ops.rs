//! The operator's requests to archond, shared by the one-shot commands and
//! the chat's slash commands: each sends one request and checks the reply,
//! and the caller says what happened in its own words.

use archon_client::ArchonClient;
use archon_core::attachment::ImageAttachment;
use archon_core::id::QueueItemId;
use archon_core::integration::Promotion;
use archon_core::merge::MergeRequest;
use archon_core::notify::Question;
use archon_core::protocol::{PauseTarget, Reply, RepoRef, Request};

use crate::connect::unexpected;

/// Send one line to the orchestrator.
pub async fn say(
    client: &ArchonClient,
    text: String,
    images: Vec<ImageAttachment>,
) -> anyhow::Result<()> {
    assert!(
        !text.trim().is_empty() || !images.is_empty(),
        "a message without images is never blank"
    );
    match client.call(Request::Say { text, images }).await? {
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

/// Pause `target`, or resume it when `pause` is false. `false` comes back
/// when a resume found nothing paused.
pub async fn pause(
    client: &ArchonClient,
    target: PauseTarget,
    pause: bool,
) -> anyhow::Result<bool> {
    let was_paused = match pause {
        true => true,
        false => is_paused(client, &target).await?,
    };
    let request = match pause {
        true => Request::Pause(target),
        false => Request::Resume(target),
    };
    match client.request(request).await? {
        Reply::Ok => Ok(was_paused),
        other => Err(unexpected(&other, "ok")),
    }
}

/// Whether `target` is paused now. A hard resume also frees stage agents,
/// so it always counts as a change; an unknown repository is left for
/// archond to refuse.
async fn is_paused(client: &ArchonClient, target: &PauseTarget) -> anyhow::Result<bool> {
    let state = crate::views::fetch_status(client).await?.pause;
    match target {
        PauseTarget::Global { hard } => Ok(*hard || state.global),
        PauseTarget::Pipeline { name } => Ok(state.pipelines.contains(name)),
        PauseTarget::Repo { name } => {
            let repos = crate::views::fetch_repos(client).await?;
            let repo = repos
                .iter()
                .find(|r| r.name == name.0 || r.id.as_str() == name.0);
            Ok(repo.is_none_or(|r| state.repos.contains(&r.id)))
        }
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

pub async fn diff(
    client: &ArchonClient,
    item: QueueItemId,
    full: bool,
) -> anyhow::Result<archon_core::merge::LocalReview> {
    match client.request(Request::ItemDiff { item, full }).await? {
        Reply::LocalReview(review) => Ok(*review),
        other => Err(unexpected(&other, "diff")),
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
