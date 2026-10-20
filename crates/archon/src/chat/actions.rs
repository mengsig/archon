//! What a typed line does: talk to the orchestrator or run a slash command.
//! A failed request is reported in the chat and the chat carries on.

use std::path::Path;

use archon_client::ArchonClient;
use archon_core::protocol::{ItemQuery, PauseTarget, Request};

use super::commands::{ChatInput, SlashCommand, help, parse_line};
use super::draft::without_chips;
use super::{Chat, Flow};
use crate::format;
use crate::images::{expand_home, pasted_image_path, read_image_file};
use crate::ops;
use crate::questions::QuestionRef;
use crate::views;

const QUEUE_LIMIT: usize = 50;

pub(super) async fn handle_line(
    chat: &mut Chat,
    line: &str,
    client: Option<&ArchonClient>,
) -> anyhow::Result<Flow> {
    // The chips are the draft's; what the line asks for is what is left.
    let bare = without_chips(line);
    let home = dirs::home_dir();
    if let Some(path) = pasted_image_path(&bare, home.as_deref()) {
        attach_file(chat, line, &path)?;
        return Ok(Flow::Continue);
    }
    let command = match parse_line(&bare) {
        ChatInput::Empty | ChatInput::Say(_) => {
            say_line(chat, client, line).await?;
            return Ok(Flow::Continue);
        }
        ChatInput::Command(SlashCommand::Image(path)) => {
            attach_named(chat, line, &path, home.as_deref())?;
            return Ok(Flow::Continue);
        }
        ChatInput::Invalid(message) => {
            keep_chips(chat, line);
            chat.error(&message)?;
            return Ok(Flow::Continue);
        }
        ChatInput::Command(command) => command,
    };
    keep_chips(chat, line);
    match command {
        SlashCommand::Quit => return Ok(Flow::Quit),
        SlashCommand::Help => chat.note(&help())?,
        SlashCommand::ToggleProgress => {
            chat.verbose = !chat.verbose;
            let state = if chat.verbose { "shown" } else { "hidden" };
            chat.note(&format!("progress notes are {state}"))?;
        }
        remote => {
            if let Some(client) = connected(chat, client)? {
                let result = run_remote(chat, client, remote).await;
                report(chat, result)?;
            }
        }
    }
    Ok(Flow::Continue)
}

/// The client, or `None` after telling the operator nothing was sent.
fn connected<'a>(
    chat: &mut Chat,
    client: Option<&'a ArchonClient>,
) -> anyhow::Result<Option<&'a ArchonClient>> {
    if client.is_none() {
        chat.error("not connected to archond (reconnecting); nothing was sent")?;
    }
    Ok(client)
}

/// Show a failed request in the chat; only a broken terminal ends the chat.
fn report(chat: &mut Chat, result: anyhow::Result<()>) -> anyhow::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => chat.error(&format!("{error:#}")),
    }
}

/// Commands that need the daemon.
async fn run_remote(
    chat: &mut Chat,
    client: &ArchonClient,
    command: SlashCommand,
) -> anyhow::Result<()> {
    match command {
        SlashCommand::Authority(text) => {
            client.request(Request::OperatorCommand { text }).await?;
            Ok(())
        }
        SlashCommand::Answer { question, text } => answer(chat, client, question, &text).await,
        SlashCommand::Questions => {
            chat.sync_questions(client).await?;
            let text = format::questions(chat.book.open());
            chat.print(&text)
        }
        SlashCommand::Status => {
            let text = views::status_text(client).await?;
            chat.print(&text)
        }
        SlashCommand::Queue { states } => {
            let query = ItemQuery {
                states: views::parse_states(&states)?,
                limit: Some(QUEUE_LIMIT),
                ..ItemQuery::default()
            };
            let text = views::queue_text(client, query).await?;
            chat.print(&text)
        }
        SlashCommand::Item(id) => {
            let item = views::resolve_item(client, &id).await?;
            let text = views::item_text(client, item, false).await?;
            chat.print(&text)
        }
        SlashCommand::Merge {
            item,
            ignore_checks,
        } => {
            let item = views::resolve_item(client, &item).await?;
            let merge = ops::merge(client, item, ignore_checks, true).await?;
            chat.print(&format::merge(&merge))
        }
        SlashCommand::Pause(target) => pause(chat, client, target, true).await,
        SlashCommand::Resume(target) => pause(chat, client, target, false).await,
        SlashCommand::Help
        | SlashCommand::Quit
        | SlashCommand::ToggleProgress
        | SlashCommand::Image(_) => {
            unreachable!("local commands are handled before reaching archond")
        }
    }
}

/// Send `line` with the images whose chips it holds; a line of nothing but
/// stale chips sends nothing.
async fn say_line(
    chat: &mut Chat,
    client: Option<&ArchonClient>,
    line: &str,
) -> anyhow::Result<()> {
    let Some(client) = connected(chat, client)? else {
        keep_chips(chat, line);
        return Ok(());
    };
    let (text, images) = chat.lock_draft().take(line);
    if text.is_empty() && images.is_empty() {
        return Ok(());
    }
    // archond records the line before delivering it, so the echo comes
    // even when delivery is refused.
    chat.expect_echo(&text);
    let result = ops::say(client, text, images).await;
    report(chat, result)
}

/// `/image <path>`: the path as typed, or quoted or escaped as a paste has it.
fn attach_named(
    chat: &mut Chat,
    line: &str,
    path: &str,
    home: Option<&Path>,
) -> anyhow::Result<()> {
    assert!(!path.trim().is_empty(), "/image is parsed with a path");
    let found = pasted_image_path(path, home).or_else(|| expand_home(path, home));
    match found {
        Some(path) => attach_file(chat, line, &path),
        None => {
            keep_chips(chat, line);
            chat.error("no home directory to find ~ in")
        }
    }
}

/// Attach the image at `path` to the draft `line` holds, and start the next
/// line with the chips still attached.
fn attach_file(chat: &mut Chat, line: &str, path: &Path) -> anyhow::Result<()> {
    let image = match read_image_file(path) {
        Ok(image) => image,
        Err(refused) => {
            keep_chips(chat, line);
            return chat.error(&refused);
        }
    };
    let summary = image.reference().summary();
    let attached = chat.lock_draft().attach(image, line);
    prefill_chips(chat);
    match attached {
        Ok(chip) => {
            debug_assert!(chat.prefill.as_ref().is_some_and(|p| p.contains(&chip)));
            chat.note(&format!("attached {chip}: {summary}"))
        }
        Err(refused) => chat.error(&refused),
    }
}

/// A line that sends nothing keeps the images whose chips it holds, and the
/// next line starts with those chips again.
fn keep_chips(chat: &mut Chat, line: &str) {
    chat.lock_draft().keep(line);
    prefill_chips(chat);
}

fn prefill_chips(chat: &mut Chat) {
    let chips = chat.lock_draft().chips();
    chat.prefill = (!chips.is_empty()).then(|| format!("{chips} "));
}

async fn answer(
    chat: &mut Chat,
    client: &ArchonClient,
    question: QuestionRef,
    text: &str,
) -> anyhow::Result<()> {
    let Some(found) = chat.book.find(&question) else {
        return chat.error("no such open question; /questions lists them");
    };
    let number = found.number;
    assert!(number > 0, "the chat only tracks numbered questions");
    ops::answer(client, found, text, true).await?;
    chat.note(&format!("answered Q{number}"))
}

async fn pause(
    chat: &mut Chat,
    client: &ArchonClient,
    target: PauseTarget,
    pause: bool,
) -> anyhow::Result<()> {
    let what = format::pause_target(&target);
    if !ops::pause(client, target, pause).await? {
        return chat.note(&format!("nothing was paused: {what} was running"));
    }
    let verb = if pause { "paused" } else { "resumed" };
    chat.note(&format!("{verb} {what}"))
}
