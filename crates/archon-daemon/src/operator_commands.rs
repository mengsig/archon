//! Whole-message commands from authenticated operator inbox deliveries.

use archon_aether::AetherEvent;
use archon_aether::events::InboxMessage;
use archon_core::authority::{OperatorChannel, OperatorMessage};
use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::merge::MergeAuthority;
use archon_core::notify::QuestionRef;

use crate::authority::evidence;
use crate::context::DaemonContext;
use crate::{chat_answer, chat_merge, feed};

pub(crate) async fn handle(
    ctx: &DaemonContext,
    event: &AetherEvent,
    seq: u64,
    message: &InboxMessage,
) -> Result<(), ArchonError> {
    let Some(channel) = &message.channel else {
        return Ok(());
    };
    let github = channel.is_github();
    if !github && ctx.config.aether.mode != AetherMode::Dedicated {
        // Preserve the existing refusal messages for shared-mode chat.
        chat_answer::handle(ctx, &message.text, false).await?;
        return chat_merge::handle(ctx, &message.text, MergeAuthority::AetherChat).await;
    }
    if github && ctx.aether.client()?.welcome().protocol_version < 17 {
        return feed::system(
            ctx,
            "GitHub operator commands need Aether protocol 17; upgrade Archon's aetherd.",
        )
        .await
        .map(drop);
    }
    let Some(agent) = &event.agent else {
        return Ok(());
    };
    let evidence = OperatorMessage {
        agent: agent.clone(),
        seq,
        channel: channel.clone(),
        text: message
            .text
            .chars()
            .take(evidence::MAX_MESSAGE_CHARS)
            .collect(),
        at: event.at,
    };
    evidence::record(ctx, &evidence).await?;
    let text = command_text(&message.text, github);
    let command = match gate_alias(ctx, text).await {
        Ok(Some(command)) => command,
        Ok(None) => text.to_owned(),
        Err(error) if error.is_store() => return Err(error),
        Err(error) => {
            return feed::system(ctx, format!("Operator command refused: {error}"))
                .await
                .map(drop);
        }
    };
    if chat_answer::parse(&command).is_some() || chat_merge::parse(&command).is_some() {
        feed::append(
            ctx,
            archon_core::notify::FeedEntry::Authority {
                evidence,
                action: command.clone(),
                result: "received; outcome follows".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::Null,
            },
        )
        .await?;
    }
    chat_answer::handle(ctx, &command, github).await?;
    let authority = match channel {
        OperatorChannel::AetherChat => MergeAuthority::AetherChat,
        OperatorChannel::Github {
            repository,
            number,
            login,
        } => MergeAuthority::Github {
            repository: repository.to_string(),
            number: *number,
            login: login.clone(),
        },
    };
    chat_merge::handle(ctx, &command, authority).await
}

/// GitHub's mention is accepted only at the start, never inside quoted text.
fn command_text(text: &str, github: bool) -> &str {
    let text = text.trim();
    if github
        && text
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("@archon"))
    {
        let rest = &text[7..];
        if rest.starts_with(char::is_whitespace) {
            return rest.trim_start();
        }
    }
    text
}

async fn gate_alias(ctx: &DaemonContext, text: &str) -> Result<Option<String>, ArchonError> {
    let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if name.eq_ignore_ascii_case("/a") {
        return Ok(Some(format!("/answer {}", rest.trim())));
    }
    let decision = match name.to_ascii_lowercase().as_str() {
        "/approve" => "approve",
        "/reject" => "reject",
        _ => return Ok(None),
    };
    let (reference, note) = rest
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((rest.trim(), ""));
    let store = ctx.store.lock().await;
    let mut gates = Vec::new();
    for question in store.questions(true)? {
        if store.gate_by_question(&question.id)?.is_some() {
            gates.push(question);
        }
    }
    let question = if reference.is_empty() {
        if gates.len() != 1 {
            return Err(ArchonError::Invalid(
                "name a question: /approve N or /reject N; there is not exactly one open gate"
                    .into(),
            ));
        }
        gates.remove(0)
    } else {
        let reference = QuestionRef::parse(reference).map_err(ArchonError::Invalid)?;
        gates
            .into_iter()
            .find(|q| match &reference {
                QuestionRef::Id(id) => &q.id == id,
                QuestionRef::Number(n) => q.number == *n,
            })
            .ok_or_else(|| {
                ArchonError::Invalid(
                    "the named question is not an open proposal/report gate".into(),
                )
            })?
    };
    let note = if note.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", note.trim())
    };
    Ok(Some(format!(
        "/answer {} {decision}{note}",
        question.number
    )))
}
