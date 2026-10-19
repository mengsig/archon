//! What a typed line does: talk to the orchestrator or run a slash command.
//! A failed request is reported in the chat and the chat carries on.

use archon_client::ArchonClient;
use archon_core::protocol::{ItemQuery, PauseTarget, Request};

use super::commands::{ChatInput, SlashCommand, help, parse_line};
use super::{Chat, Flow};
use crate::format;
use crate::ops;
use crate::questions::QuestionRef;
use crate::views;

const QUEUE_LIMIT: usize = 50;

pub(super) async fn handle_line(
    chat: &mut Chat,
    line: &str,
    client: Option<&ArchonClient>,
) -> anyhow::Result<Flow> {
    let command = match parse_line(line) {
        ChatInput::Empty => return Ok(Flow::Continue),
        ChatInput::Invalid(message) => {
            chat.error(&message)?;
            return Ok(Flow::Continue);
        }
        ChatInput::Say(text) => {
            let Some(client) = connected(chat, client)? else {
                return Ok(Flow::Continue);
            };
            let result = say(chat, client, text).await;
            report(chat, result)?;
            return Ok(Flow::Continue);
        }
        ChatInput::Command(command) => command,
    };
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
        SlashCommand::Help | SlashCommand::Quit | SlashCommand::ToggleProgress => {
            unreachable!("local commands are handled before reaching archond")
        }
    }
}

async fn say(chat: &mut Chat, client: &ArchonClient, text: String) -> anyhow::Result<()> {
    // archond records the line before delivering it, so the echo comes
    // even when delivery is refused.
    chat.expect_echo(&text);
    ops::say(client, text).await
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
    ops::pause(client, target, pause).await?;
    let verb = if pause { "paused" } else { "resumed" };
    chat.note(&format!("{verb} {what}"))
}
