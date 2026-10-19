//! An answer the operator gave in the notification center, applied as the
//! same `/answer <question> <choice>` would be.

use archon_aether::notifications::{Notification, NotificationAnswer};
use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::id::QuestionId;
use archon_core::notify::{AnswerVia, Question};

use crate::context::DaemonContext;
use crate::{feed, handlers};

/// Apply the answer `notification` carries to the open question its key
/// names. A question closed already, or a key that names none, is left be.
/// Only a store failure is returned; a refusal is told in the chat.
pub(super) async fn apply(
    ctx: &DaemonContext,
    notification: &Notification,
) -> Result<(), ArchonError> {
    let answer = notification
        .answer
        .as_ref()
        .expect("only an answered notification is applied");
    let Some(id) = notification
        .key
        .as_deref()
        .and_then(|k| QuestionId::parse(k).ok())
    else {
        return Ok(());
    };
    let question = match ctx.store.lock().await.question(&id) {
        Ok(question) => question,
        Err(ArchonError::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    assert_eq!(question.id, id, "the store returns the question asked for");
    if !question.is_open() {
        return Ok(());
    }
    match answer_with(ctx, &question, answer).await {
        Ok(()) => Ok(()),
        Err(error) if error.is_store() => Err(error),
        Err(error) => not_applied(ctx, &question, &error).await,
    }
}

async fn answer_with(
    ctx: &DaemonContext,
    question: &Question,
    answer: &NotificationAnswer,
) -> Result<(), ArchonError> {
    permitted(ctx, question, answer).await?;
    let text = answer_text(question, answer)?;
    let via = AnswerVia::NotificationCenter;
    let answered = handlers::answer_question(ctx, &question.id, &text, via).await?;
    assert!(!answered.is_open(), "an applied answer closes its question");
    Ok(())
}

/// The rules of an `/answer` chat command: a person answered, in an Aether
/// dedicated to Archon, and not an authority confirmation, which is
/// answered only in the channel it was asked in.
async fn permitted(
    ctx: &DaemonContext,
    question: &Question,
    answer: &NotificationAnswer,
) -> Result<(), ArchonError> {
    if !answer.by_operator() {
        return Err(ArchonError::Forbidden(
            "only a person answers in the notification center".into(),
        ));
    }
    if ctx.config.aether.mode != AetherMode::Dedicated {
        return Err(ArchonError::Forbidden(
            "answers in the notification center are off while Archon shares the operator's Aether instance".into(),
        ));
    }
    if crate::authority::is_confirmation(ctx, &question.id).await? {
        return Err(ArchonError::Forbidden(
            "an authority confirmation is answered in the channel it was asked in".into(),
        ));
    }
    Ok(())
}

/// The answer text `/answer` would record: the chosen option, the operator's
/// own words (a bare number picks an option), or the option followed by them.
fn answer_text(question: &Question, answer: &NotificationAnswer) -> Result<String, ArchonError> {
    let chosen = answer
        .action
        .as_deref()
        .map(|action| question.expand_option(action))
        .transpose()?;
    let typed = answer
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let text = match (chosen, typed) {
        (Some(choice), Some(text)) => format!("{choice} {text}"),
        (Some(choice), None) => choice,
        (None, Some(text)) => question.expand_option(text)?,
        (None, None) => {
            return Err(ArchonError::Invalid(
                "the answer chose no option and said nothing".into(),
            ));
        }
    };
    assert!(!text.trim().is_empty(), "an applied answer says something");
    Ok(text)
}

/// Tell the operator the answer did not take, and how to give it again; a
/// lost connection is only logged, as the next connect retries it.
async fn not_applied(
    ctx: &DaemonContext,
    question: &Question,
    error: &ArchonError,
) -> Result<(), ArchonError> {
    let number = question.number;
    assert!(number > 0, "a stored question is numbered");
    if matches!(error, ArchonError::Unavailable(_)) {
        tracing::warn!(question = %question.id, %error, "notification center answer deferred");
        return Ok(());
    }
    tracing::warn!(question = %question.id, %error, "notification center answer refused");
    let text = format!(
        "Q{number} was answered in the notification center, but the answer was not applied: {error}. Answer it with `archon answer {number} <answer>`."
    );
    feed::system(ctx, text).await.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(options: &[&str]) -> Question {
        Question {
            id: QuestionId::new(),
            number: 3,
            text: "Ship it?".into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            item: None,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        }
    }

    fn answer(action: Option<&str>, text: Option<&str>) -> NotificationAnswer {
        NotificationAnswer {
            action: action.map(str::to_owned),
            text: text.map(str::to_owned),
            by: serde_json::json!({"actor": "user"}),
        }
    }

    #[test]
    fn an_action_picks_its_option_and_words_follow_it() {
        let q = question(&["yes", "reject"]);
        assert_eq!(answer_text(&q, &answer(Some("1"), None)).unwrap(), "yes");
        let both = answer(Some("2"), Some(" too risky "));
        assert_eq!(answer_text(&q, &both).unwrap(), "reject too risky");
        let typed = answer(None, Some("ship after the release"));
        assert_eq!(answer_text(&q, &typed).unwrap(), "ship after the release");
        assert_eq!(
            answer_text(&q, &answer(None, Some(" 2 "))).unwrap(),
            "reject"
        );
        assert!(answer_text(&q, &answer(Some("3"), None)).is_err());
        assert!(answer_text(&q, &answer(None, Some("  "))).is_err());
    }
}
