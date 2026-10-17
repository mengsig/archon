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
        if retired_alias(&message.text) {
            return retired_alias_hint(ctx).await;
        }
        if matches!(
            message.text.split_whitespace().next(),
            Some("/archon" | "/approve" | "/reject")
        ) {
            feed::system(ctx, "Operator commands are disabled in shared-mode Aether chat; use Archon's dedicated Aether or the operator socket.").await?;
        }
        // Preserve the existing refusal messages for shared-mode chat.
        chat_answer::handle(ctx, &message.text, None).await?;
        crate::chat_ops::handle(ctx, &message.text).await?;
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
        agent: Some(agent.clone()),
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
    execute(ctx, evidence, &message.text).await
}

/// The operator socket is already authenticated, including in shared mode.
/// Its feed sequence is audit evidence, never an Aether inbox citation.
pub(crate) async fn from_socket(
    ctx: &DaemonContext,
    text: &str,
) -> Result<archon_core::protocol::Reply, ArchonError> {
    if text.chars().count() > evidence::MAX_MESSAGE_CHARS {
        return Err(ArchonError::Invalid(
            "operator command exceeds 4000 characters".into(),
        ));
    }
    let item = feed::append(
        ctx,
        archon_core::notify::FeedEntry::Operator { text: text.into() },
    )
    .await?;
    let evidence = OperatorMessage {
        agent: None,
        seq: item.seq,
        channel: OperatorChannel::ArchonChat,
        text: text.into(),
        at: item.at,
    };
    execute(ctx, evidence, text).await?;
    Ok(archon_core::protocol::Reply::Ok)
}

async fn execute(
    ctx: &DaemonContext,
    evidence: OperatorMessage,
    text: &str,
) -> Result<(), ArchonError> {
    let channel = &evidence.channel;
    let github = channel.is_github();
    if crate::authority::bare_reply(ctx, &evidence).await? {
        return Ok(());
    }
    let text = command_text(text, github);
    if text.starts_with('/') && text.chars().count() > evidence::MAX_MESSAGE_CHARS {
        return feed::system(ctx, "Operator command exceeds the 4000-character limit.")
            .await
            .map(drop);
    }
    if let Some(result) = crate::authority::command(ctx, &evidence, text).await {
        let (applied, text) = match result {
            Ok(reply) => (true, serde_json::to_string(&reply)?),
            Err(error) if error.is_store() => return Err(error),
            Err(error) => (false, error.to_string()),
        };
        feed::system(ctx, format!("Operator command: {text}")).await?;
        return crate::triggers::push(
            ctx,
            crate::triggers::TriggerEvent::ChatAnswer {
                number: None,
                applied,
                text,
            },
        )
        .await;
    }
    if retired_alias(text) {
        return retired_alias_hint(ctx).await;
    }
    // Existing upgrade/watch commands remain restricted to authenticated TUI users.
    if !github {
        crate::chat_ops::handle(ctx, text).await?;
    }
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
                evidence: evidence.clone(),
                action: command.clone(),
                result: "received; outcome follows".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::Null,
            },
        )
        .await?;
    }
    chat_answer::handle(ctx, &command, Some(&evidence)).await?;
    let authority = match channel {
        OperatorChannel::AetherChat => MergeAuthority::AetherChat,
        OperatorChannel::ArchonChat => MergeAuthority::ArchonChat,
        OperatorChannel::Cli => MergeAuthority::Cli,
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
pub(crate) fn command_text(text: &str, github: bool) -> &str {
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

const RETIRED_ALIAS_HINT: &str = "/a is no longer a command: use /answer N <answer>.";

/// `/a` was once `/answer`'s alias; whoever still types it gets a pointer.
fn retired_alias(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("/a"))
}

/// Note the hint in Archon's feed and tell the orchestrator, which restates
/// it in the chat the alias was typed in.
async fn retired_alias_hint(ctx: &DaemonContext) -> Result<(), ArchonError> {
    feed::system(ctx, RETIRED_ALIAS_HINT).await?;
    let news = crate::triggers::TriggerEvent::ChatAnswer {
        number: None,
        applied: false,
        text: RETIRED_ALIAS_HINT.into(),
    };
    crate::triggers::push(ctx, news).await
}

async fn gate_alias(ctx: &DaemonContext, text: &str) -> Result<Option<String>, ArchonError> {
    let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_message_led_by_a_bare_slash_a_is_the_retired_alias() {
        for text in ["/a", "/a 3 yes", "/A yes", "  /a\t2"] {
            assert!(retired_alias(text), "{text:?}");
        }
        for text in ["/answer 3 yes", "/approve", "/a/b", "say /a 3 yes", ""] {
            assert!(!retired_alias(text), "{text:?}");
        }
    }
}
