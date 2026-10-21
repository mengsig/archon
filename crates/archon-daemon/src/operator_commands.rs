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
    let github = message.channel.as_ref().is_some_and(|c| c.is_github());
    if !github && ctx.config.aether.mode != AetherMode::Dedicated {
        return refuse_in_shared(ctx, message).await;
    }
    let Some(channel) = &message.channel else {
        return Ok(());
    };
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
        archon_core::notify::FeedEntry::Operator {
            text: text.into(),
            images: Vec::new(),
        },
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
    let text = unalias(text);
    let text = text.as_ref();
    if text.starts_with('/') && text.chars().count() > evidence::MAX_MESSAGE_CHARS {
        return feed::system(ctx, "Operator command exceeds the 4000-character limit.")
            .await
            .map(drop);
    }
    if let Some(result) = crate::authority::command(ctx, &evidence, text).await {
        let label = command_label(text);
        let (applied, text) = match result {
            Ok(archon_core::protocol::Reply::Question(asked)) => (
                false,
                format!(
                    "Awaiting your confirmation as Q{}; nothing changed yet.",
                    asked.number
                ),
            ),
            Ok(_) => (true, crate::operator_sentence::command_sentence(text)),
            Err(error) if error.is_store() => return Err(error),
            Err(error) => (false, error.to_string()),
        };
        feed::system(ctx, format!("Operator command: {text}")).await?;
        crate::command_notice::show(ctx, &label, applied, &text).await;
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

/// Host commands an Aether chat message may carry; shared mode refuses them
/// all, because its `user` actor includes `[listen]` token clients.
const SHARED_CHAT_COMMANDS: &[&str] = &[
    "/answer", "/approve", "/reject", "/merge", "/archon", "/upgrade", "/update", "/watch", "/a",
];

const SHARED_REFUSAL: &str = "Operator commands are disabled in shared-mode Aether chat; use Archon's dedicated Aether or the operator socket.";

/// Whether the whole message is led by one of the host command words.
fn names_host_command(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("");
    SHARED_CHAT_COMMANDS
        .iter()
        .any(|name| first.eq_ignore_ascii_case(name))
}

/// A shared-mode Aether chat message never runs a command: say so in the
/// feed, in the TUI and to the orchestrator. Other text is left alone.
async fn refuse_in_shared(ctx: &DaemonContext, message: &InboxMessage) -> Result<(), ArchonError> {
    assert!(
        ctx.config.aether.mode != AetherMode::Dedicated,
        "only a shared instance refuses chat commands"
    );
    let text = unalias(message.text.trim());
    if !names_host_command(&text) {
        return Ok(());
    }
    if retired_alias(&text) {
        return retired_alias_hint(ctx).await;
    }
    let first = text.split_whitespace().next().unwrap_or("");
    // The specific handlers word the refusal for the commands they parse.
    if !["/approve", "/reject", "/archon"]
        .iter()
        .any(|name| first.eq_ignore_ascii_case(name))
    {
        chat_answer::handle(ctx, &text, None).await?;
        crate::chat_ops::handle(ctx, &text).await?;
        return chat_merge::handle(ctx, &text, MergeAuthority::AetherChat).await;
    }
    feed::system(ctx, SHARED_REFUSAL).await?;
    crate::command_notice::show(ctx, &command_label(&text), false, SHARED_REFUSAL).await;
    let news = crate::triggers::TriggerEvent::ChatAnswer {
        number: None,
        applied: false,
        text: SHARED_REFUSAL.into(),
    };
    crate::triggers::push(ctx, news).await
}

/// The command word, plus its subcommand for `/archon`, for a result note.
fn command_label(text: &str) -> String {
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or("/archon");
    match (first, words.next()) {
        ("/archon", Some(sub)) => format!("{first} {sub}"),
        _ => first.to_owned(),
    }
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

/// `/archon answer|approve|reject|merge …` runs the same handler as the bare
/// command, so the sender checks and results cannot differ.
pub(crate) fn unalias(text: &str) -> std::borrow::Cow<'_, str> {
    let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if !name.eq_ignore_ascii_case("/archon") {
        return text.into();
    }
    let rest = rest.trim_start();
    let (verb, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let verb = verb.to_ascii_lowercase();
    match verb.as_str() {
        "answer" | "approve" | "reject" | "merge" => {
            format!("/{verb} {args}").trim_end().to_owned().into()
        }
        _ => text.into(),
    }
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

/// A first word that is shaped like a question reference (`3`, `Q3`, `q_…`),
/// even a bad one such as `0` or a truncated `q_…`, so it is refused rather than taken as a note.
fn names_a_question(word: &str) -> bool {
    let digits = word.strip_prefix(['Q', 'q']).unwrap_or(word);
    let numeric = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    numeric || word.starts_with("q_")
}

fn not_one_gate(gates: &[archon_core::notify::Question]) -> String {
    if gates.is_empty() {
        return "there is no open proposal/report gate to act on".into();
    }
    let listed: Vec<String> = gates
        .iter()
        .map(|q| format!("Q{}: {}", q.number, q.text))
        .collect();
    format!(
        "name a question: /approve N [note] or /reject N [note]; open gates: {}",
        listed.join("; ")
    )
}

async fn gate_alias(ctx: &DaemonContext, text: &str) -> Result<Option<String>, ArchonError> {
    let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let decision = match name.to_ascii_lowercase().as_str() {
        "/approve" => "approve",
        "/reject" => "reject",
        _ => return Ok(None),
    };
    let rest = rest.trim();
    let (first, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (reference, note) = if names_a_question(first) {
        (first, after)
    } else {
        ("", rest)
    };
    let store = ctx.store.lock().await;
    let mut gates = Vec::new();
    for question in store.questions(true)? {
        if store.gate_by_question(&question.id)?.is_some() {
            gates.push(question);
        }
    }
    let question = if reference.is_empty() {
        if gates.len() != 1 {
            return Err(ArchonError::Invalid(not_one_gate(&gates)));
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
    fn archon_verbs_become_the_bare_commands() {
        for (text, bare) in [
            ("/archon answer 73 yes", "/answer 73 yes"),
            ("/ARCHON  Approve", "/approve"),
            ("/archon reject 4 no\nwhy", "/reject 4 no\nwhy"),
            (
                "/archon merge qi_01 --ignore-checks",
                "/merge qi_01 --ignore-checks",
            ),
        ] {
            assert_eq!(unalias(text), bare);
        }
        for text in [
            "/archon set a=b",
            "/archon",
            "/answer 3 yes",
            "say /archon answer 3 x",
            "/archonanswer 3",
        ] {
            assert_eq!(unalias(text), text);
        }
    }

    #[test]
    fn only_a_question_shaped_word_is_a_reference() {
        for word in ["3", "Q3", "q12", "0", "99", "q_01M57Z"] {
            assert!(names_a_question(word), "{word:?}");
        }
        for word in ["looks", "not", "q", "3rd", "approved,"] {
            assert!(!names_a_question(word), "{word:?}");
        }
    }

    #[test]
    fn several_gates_are_listed_by_number_and_title() {
        assert!(not_one_gate(&[]).contains("no open"));
    }

    #[test]
    fn only_a_message_led_by_a_bare_slash_a_is_the_retired_alias() {
        for text in ["/a", "/a 3 yes", "/A yes", "  /a\t2"] {
            assert!(retired_alias(text), "{text:?}");
        }
        for text in ["/answer 3 yes", "/approve", "/a/b", "say /a 3 yes", ""] {
            assert!(!retired_alias(text), "{text:?}");
        }
    }

    #[test]
    fn only_a_message_led_by_a_host_command_is_refused_in_shared_chat() {
        for text in [
            "/answer 3 yes",
            "/APPROVE",
            "/archon set a=b",
            "/watch repo",
            "/a 1",
        ] {
            assert!(names_host_command(text), "{text:?}");
        }
        for text in ["say /answer 3 yes", "hello", "/status", "/answered", ""] {
            assert!(!names_host_command(text), "{text:?}");
        }
    }

    #[test]
    fn the_shared_list_names_only_real_operator_commands() {
        for name in SHARED_CHAT_COMMANDS.iter().filter(|n| **n != "/a") {
            let known = archon_core::operator_commands::OPERATOR_COMMANDS
                .iter()
                .any(|c| format!("/{}", c.name) == *name);
            assert!(known, "{name}");
        }
    }
}
