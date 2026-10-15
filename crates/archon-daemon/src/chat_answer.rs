//! The operator's `/answer` chat command (ARCHITECTURE §5.2). A message the
//! operator sends the orchestrator in Aether that is exactly
//! `/answer <question> <answer>` is applied here, through the answer path
//! `archon answer` uses, and its outcome is sent to the orchestrator as
//! `chat_answer` news to restate in the same chat.
//!
//! The pump calls [`handle`] only for a message Aether authenticated as the
//! `user` actor. In a shared Aether instance that actor includes `[listen]`
//! token clients, so the command is applied in dedicated mode only.

use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::notify::{AnswerVia, Question, QuestionRef};

use crate::context::DaemonContext;
use crate::executor::clip;
use crate::triggers::{self, TriggerEvent};
use crate::{feed, handlers, reports};

const COMMAND: &str = "/answer";
const USAGE: &str = "usage: `/answer <question number or q_… id> <answer>`, alone in the message";
const SHARED_REFUSAL: &str = "chat answers are off while Archon shares the operator's Aether instance (`[aether] mode = \"shared\"`), where a `[listen]` token client is the same `user` actor as the operator; answer with `archon answer <number> <answer>`";
/// Longest answer or reason one `chat_answer` event carries.
const MAX_NEWS_CHARS: usize = 500;

/// A well-formed `/answer` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    question: QuestionRef,
    text: String,
}

/// What became of one command; the body of its `chat_answer` event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    number: Option<u64>,
    applied: bool,
    text: String,
}

impl Outcome {
    fn refused(number: Option<u64>, why: impl Into<String>) -> Self {
        let text = why.into();
        assert!(!text.trim().is_empty(), "a refusal gives its reason");
        Self {
            number,
            applied: false,
            text,
        }
    }
}

/// `None` unless the whole message is an `/answer` command, so one quoted
/// or embedded in other text is never applied. `Some(Err)` says what a
/// malformed command lacks.
pub(crate) fn parse(message: &str) -> Option<Result<Command, String>> {
    let message = message.trim();
    let name = message.get(..COMMAND.len())?;
    if !name.eq_ignore_ascii_case(COMMAND) {
        return None;
    }
    let rest = &message[COMMAND.len()..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let (reference, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if reference.is_empty() {
        return Some(Err(format!("no question was named; {USAGE}")));
    }
    let question = match QuestionRef::parse(reference) {
        Ok(question) => question,
        Err(why) => return Some(Err(format!("{why}; {USAGE}"))),
    };
    let text = text.trim();
    if text.is_empty() {
        return Some(Err(format!("no answer was given; {USAGE}")));
    }
    Some(Ok(Command {
        question,
        text: text.to_owned(),
    }))
}

/// Apply `message` if it is an `/answer` command and tell the operator what
/// happened. Only a store failure is returned: the pump shows a message
/// here once, so any other refusal is reported, never retried.
pub(crate) async fn handle(
    ctx: &DaemonContext,
    message: &str,
    evidence: Option<&archon_core::authority::OperatorMessage>,
) -> Result<(), ArchonError> {
    let Some(parsed) = parse(message) else {
        return Ok(());
    };
    let outcome = match parsed {
        Ok(command) => apply(ctx, &command, evidence).await?,
        Err(usage) => Outcome::refused(None, usage),
    };
    report(ctx, outcome).await
}

async fn apply(
    ctx: &DaemonContext,
    command: &Command,
    evidence: Option<&archon_core::authority::OperatorMessage>,
) -> Result<Outcome, ArchonError> {
    if ctx.config.aether.mode != AetherMode::Dedicated
        && !evidence.is_some_and(|m| {
            !matches!(
                m.channel,
                archon_core::authority::OperatorChannel::AetherChat
            )
        })
    {
        return Ok(Outcome::refused(None, SHARED_REFUSAL));
    }
    let Some(question) = find(ctx, &command.question).await? else {
        return Ok(Outcome::refused(None, unknown(&command.question)));
    };
    let number = Some(question.number);
    if !question.is_open() {
        let why = reports::already_closed(&question).to_string();
        return Ok(Outcome::refused(number, why));
    }
    if let Some(evidence) = evidence
        && let Err(error) = crate::authority::check_answer(ctx, &question.id, evidence).await
    {
        if error.is_store() {
            return Err(error);
        }
        return Ok(Outcome::refused(number, error.to_string()));
    }
    let text = question.expand_option(&command.text);
    let via = AnswerVia::ChatCommand;
    match handlers::answer_question(ctx, &question.id, &text, via).await {
        Ok(answered) => {
            assert_eq!(answered.id, question.id, "the question named was answered");
            assert!(!answered.is_open(), "an applied answer closes its question");
            Ok(Outcome {
                number,
                applied: true,
                text,
            })
        }
        Err(error) if error.is_store() => Err(error),
        Err(error) => not_applied(ctx, &question, &error).await,
    }
}

/// The question `reference` names, open or closed; `None` when none is
/// recorded under it.
async fn find(
    ctx: &DaemonContext,
    reference: &QuestionRef,
) -> Result<Option<Question>, ArchonError> {
    let store = ctx.store.lock().await;
    match reference {
        QuestionRef::Id(id) => match store.question(id) {
            Ok(question) => Ok(Some(question)),
            Err(ArchonError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        },
        QuestionRef::Number(number) => {
            assert!(*number > 0, "question numbers start at 1");
            let recorded = store.questions(false)?;
            Ok(recorded.into_iter().find(|q| q.number == *number))
        }
    }
}

fn unknown(reference: &QuestionRef) -> String {
    let named = match reference {
        QuestionRef::Number(number) => format!("Q{number}"),
        QuestionRef::Id(id) => id.to_string(),
    };
    format!("no question {named} is recorded; `archon questions` lists the open ones")
}

/// The refusal of an answer the answer path turned down, saying whether the
/// question still waits.
async fn not_applied(
    ctx: &DaemonContext,
    question: &Question,
    error: &ArchonError,
) -> Result<Outcome, ArchonError> {
    assert!(
        !error.is_store(),
        "a store failure is never reported as a refusal"
    );
    let now = ctx.store.lock().await.question(&question.id)?;
    assert_eq!(now.id, question.id, "the store returns the question asked");
    let state = match now.is_open() {
        true => "is still open",
        false => "has closed meanwhile",
    };
    let why = format!("{error}; Q{} {state}", question.number);
    Ok(Outcome::refused(Some(question.number), why))
}

/// Tell the orchestrator, which restates it in the chat the command came
/// from; a refusal is also noted in Archon's own feed.
async fn report(ctx: &DaemonContext, outcome: Outcome) -> Result<(), ArchonError> {
    assert!(!outcome.text.trim().is_empty(), "an outcome says something");
    if !outcome.applied {
        let named = outcome
            .number
            .map(|n| format!(" for Q{n}"))
            .unwrap_or_default();
        let note = format!(
            "A `/answer`{named} sent in the orchestrator's Aether chat was not applied: {}",
            outcome.text
        );
        feed::system(ctx, clip(&note, 2 * MAX_NEWS_CHARS)).await?;
    }
    let news = TriggerEvent::ChatAnswer {
        number: outcome.number,
        applied: outcome.applied,
        text: clip(&outcome.text, MAX_NEWS_CHARS),
    };
    triggers::push(ctx, news).await
}

#[cfg(test)]
mod tests {
    use archon_core::id::QuestionId;

    use super::*;

    fn command(question: QuestionRef, text: &str) -> Option<Result<Command, String>> {
        Some(Ok(Command {
            question,
            text: text.to_owned(),
        }))
    }

    #[test]
    fn a_whole_message_command_names_its_question_and_answer() {
        let third = QuestionRef::Number(3);
        assert_eq!(
            parse("/answer 3 approve"),
            command(third.clone(), "approve")
        );
        assert_eq!(
            parse("  /answer Q3  yes, ship it \n"),
            command(third.clone(), "yes, ship it")
        );
        assert_eq!(parse("/ANSWER q3 reject"), command(third.clone(), "reject"));
        assert_eq!(
            parse("/answer 3 tighten the scope\nand drop the migration"),
            command(third, "tighten the scope\nand drop the migration")
        );
        let id = QuestionId::new();
        assert_eq!(
            parse(&format!("/answer {id} 2")),
            command(QuestionRef::Id(id), "2")
        );
    }

    #[test]
    fn text_that_is_not_exactly_a_command_is_left_alone() {
        for message in [
            "",
            "approve",
            "answer 3 approve",
            "please /answer 3 approve",
            "> /answer 3 approve",
            "`/answer 3 approve`",
            "the reviewer wrote:\n/answer 3 approve",
            "/answering 3 approve",
            "/answer3 approve",
            "/a 3 approve",
            "//answer 3 approve",
            "/ansẅer 3 approve",
        ] {
            assert_eq!(parse(message), None, "{message:?}");
        }
    }

    #[test]
    fn a_malformed_command_says_what_it_lacks() {
        let refusal = |message: &str| parse(message).expect("a command").expect_err("malformed");
        assert!(refusal("/answer").starts_with("no question was named"));
        assert!(refusal("/answer   ").starts_with("no question was named"));
        assert!(refusal("/answer 3").starts_with("no answer was given"));
        assert!(refusal("/answer 3   \n").starts_with("no answer was given"));
        assert!(refusal("/answer 0 yes").contains("not a question number"));
        assert!(refusal("/answer approve 3").contains("neither a question number"));
        assert!(refusal("/answer -3 yes").contains("neither a question number"));
        assert!(refusal("/answer 99999999999999999999999 yes").contains("not a question number"));
        for message in ["/answer", "/answer 3", "/answer x y"] {
            assert!(refusal(message).ends_with(USAGE), "{message:?}");
        }
    }

    #[test]
    fn the_outcome_of_a_refusal_is_never_marked_applied() {
        let outcome = Outcome::refused(Some(4), "no");
        assert!(!outcome.applied);
        assert_eq!(outcome.number, Some(4));
    }
}
