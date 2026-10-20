//! The operator channel (ARCHITECTURE §5.2, §12): chat lines, notices,
//! questions and answers, feed history and reports.

use std::path::Path;

use archon_core::aether_ids::AetherAgentId;
use archon_core::attachment::{ImageAttachment, check_images};
use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QuestionId, QueueItemId};
use archon_core::notify::{AnswerVia, FeedEntry, Notification, NotificationKind, Question};
use archon_core::protocol::Reply;
use archon_queue::Store;

use crate::context::DaemonContext;
use crate::{feed, reports, supervisor};

/// Longest chat line, notice or question accepted.
const MAX_TEXT_CHARS: usize = 20_000;
/// Most items one `FeedHistory` returns.
const MAX_HISTORY: usize = 1_000;
const MAX_OPTIONS: usize = 10;

/// Echo the line to the feed, then hand it and its images to the
/// orchestrator. Images are checked as Aether checks them, before anything
/// is recorded; the feed keeps only what each image was.
pub(super) async fn say(
    ctx: &DaemonContext,
    text: &str,
    images: &[ImageAttachment],
) -> Result<Reply, ArchonError> {
    let refs = check_images(images).map_err(|e| ArchonError::Invalid(e.to_string()))?;
    let text = match refs.is_empty() {
        true => checked_text(text, "a chat line")?,
        false => checked_caption(text)?,
    };
    let entry = FeedEntry::Operator {
        text: text.clone(),
        images: refs,
    };
    let item = feed::append(ctx, entry).await?;
    assert!(item.seq > 0, "feed sequences start at 1");
    crate::learned::operator_message(ctx, &text).await?;
    supervisor::deliver_operator_message(ctx, &item.id, &text, images).await?;
    Ok(Reply::Ok)
}

/// The text beside at least one image: trimmed and bounded, and may be blank.
fn checked_caption(text: &str) -> Result<String, ArchonError> {
    match text.trim().is_empty() {
        true => Ok(String::new()),
        false => checked_text(text, "a chat line"),
    }
}

pub(super) async fn notify(
    ctx: &DaemonContext,
    kind: NotificationKind,
    text: &str,
    item: Option<QueueItemId>,
) -> Result<Reply, ArchonError> {
    let text = checked_text(text, "a notification")?;
    if let Some(id) = &item {
        ctx.store.lock().await.item(id)?;
    }
    let entry = FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind,
        text,
        item,
        run: None,
        at: archon_core::now(),
    });
    feed::append(ctx, entry).await?;
    Ok(Reply::Ok)
}

/// Record a non-blocking question; the answer comes back as a trigger.
/// `asked_by` is Aether's word for who called, kept as the question's asker.
pub(super) async fn ask(
    ctx: &DaemonContext,
    text: &str,
    item: Option<QueueItemId>,
    options: Vec<String>,
    asked_by: Option<AetherAgentId>,
) -> Result<Reply, ArchonError> {
    let text = checked_text(text, "a question")?;
    let options: Vec<String> = options
        .into_iter()
        .map(|o| o.trim().to_owned())
        .filter(|o| !o.is_empty())
        .collect();
    if options.len() > MAX_OPTIONS {
        return Err(ArchonError::Invalid(format!(
            "at most {MAX_OPTIONS} options"
        )));
    }
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text,
        options,
        item,
        aether: None,
        asked_by,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let question = {
        let mut store = ctx.store.lock().await;
        if let Some(id) = &question.item {
            store.item(id)?;
        }
        let recorded = store.insert_question(&question)?;
        assert_eq!(
            recorded.asked_by, question.asked_by,
            "the store keeps who asked"
        );
        recorded
    };
    feed::append(ctx, FeedEntry::Question(question.clone())).await?;
    crate::push::question(ctx, &question).await;
    Ok(Reply::Question(question))
}

/// The operator's answer, from `via`: route it to whoever asked, then
/// record and echo it. A failed delivery leaves the question open, so
/// answering again retries it. A "Reopen it?" answer is recorded while it
/// is routed (see `reports`).
pub(super) async fn answer(
    ctx: &DaemonContext,
    id: &QuestionId,
    text: &str,
    via: AnswerVia,
) -> Result<Question, ArchonError> {
    assert_ne!(
        via,
        AnswerVia::ChatReply,
        "a chat reply is recorded, not routed"
    );
    let text = checked_text(text, "an answer")?;
    let _answering = ctx.answering.lock().await;
    let asked = ctx.store.lock().await.question(id)?;
    assert_eq!(&asked.id, id, "the store returns the question asked for");
    if !asked.is_open() {
        return Err(reports::already_closed(&asked));
    }
    if let Some(recorded) = supervisor::route_answer(ctx, &asked, &text, via).await? {
        return Ok(recorded);
    }
    let question = ctx
        .store
        .lock()
        .await
        .answer_question(id, &text, archon_core::now())?;
    assert!(!question.is_open(), "an answered question is closed");
    let entry = FeedEntry::Answer {
        question: id.clone(),
        text,
        via,
    };
    feed::append(ctx, entry).await?;
    Ok(question)
}

/// The orchestrator's `record_chat_answer`: close a question this very agent
/// asked with `ask_operator` with what the operator replied in chat. Nothing
/// is routed, and a question anyone else asked or waits on is refused.
pub(super) async fn record_chat_answer(
    ctx: &DaemonContext,
    id: &QuestionId,
    text: &str,
    caller: Option<&AetherAgentId>,
) -> Result<Reply, ArchonError> {
    let text = checked_text(text, "an answer")?;
    let orchestrator = supervisor::current_orchestrator(ctx).await?;
    let Some(caller) = caller.filter(|caller| Some(*caller) == orchestrator.as_ref()) else {
        return Err(ArchonError::Forbidden(
            "record_chat_answer is the orchestrator's own tool and needs Aether caller metadata (send_caller_meta = true)".into(),
        ));
    };
    let _answering = ctx.answering.lock().await;
    let (question, echo) = {
        let mut store = ctx.store.lock().await;
        let asked = store.question(id)?;
        if !asked.is_open() {
            return Err(reports::already_closed(&asked));
        }
        if let Some(asker) = other_asker(&mut store, &asked)? {
            return Err(only_the_operator_answers(&asked, &format!("is {asker}")));
        }
        if asked.asked_by.as_ref() != Some(caller) {
            let asker = recorded_asker(&asked);
            return Err(only_the_operator_answers(&asked, &asker));
        }
        let now = archon_core::now();
        let question = store.answer_question(id, &text, now)?;
        let entry = FeedEntry::Answer {
            question: id.clone(),
            text,
            via: AnswerVia::ChatReply,
        };
        (question, store.append_feed(entry, now)?)
    };
    assert!(!question.is_open(), "a recorded answer closes its question");
    feed::publish(ctx, vec![echo]);
    Ok(Reply::Question(question))
}

/// The refusal of a question the caller did not ask; `whose` completes "Q3 …".
fn only_the_operator_answers(question: &Question, whose: &str) -> ArchonError {
    let number = question.number;
    assert!(number > 0, "a stored question is numbered");
    ArchonError::Forbidden(format!(
        "Q{number} {whose}, which only the operator answers (`/answer {number} …` in chat, or `archon answer`); record_chat_answer closes only a question you asked with ask_operator"
    ))
}

/// How a refusal names the asker of a question the caller does not own.
fn recorded_asker(question: &Question) -> String {
    match &question.asked_by {
        Some(agent) => format!("was asked by another agent ({agent})"),
        None => "has no recorded asker".to_owned(),
    }
}

/// Who waits on `question`, if it is not one the orchestrator asked with
/// `ask_operator`: the other cases `supervisor::route_answer` routes.
fn other_asker(
    store: &mut Store,
    question: &Question,
) -> Result<Option<&'static str>, ArchonError> {
    if question.aether.is_some() {
        return Ok(Some("an Aether `ask_user` question"));
    }
    if store.gate_by_question(&question.id)?.is_some() {
        return Ok(Some("an operator gate"));
    }
    if store.merge_by_question(&question.id)?.is_some() {
        return Ok(Some(
            "a merge confirmation, which only the operator answers",
        ));
    }
    if reports::asks_to_reopen(store, question)? {
        return Ok(Some("a report's \"Reopen it?\" question"));
    }
    Ok(None)
}

pub(super) async fn questions(ctx: &DaemonContext, open_only: bool) -> Result<Reply, ArchonError> {
    Ok(Reply::Questions(
        ctx.store.lock().await.questions(open_only)?,
    ))
}

pub(super) async fn history(
    ctx: &DaemonContext,
    before: Option<u64>,
    limit: usize,
    verbose: bool,
) -> Result<Reply, ArchonError> {
    if limit == 0 {
        return Err(ArchonError::Invalid("limit must be at least 1".into()));
    }
    let limit = limit.min(MAX_HISTORY);
    let items = ctx.store.lock().await.feed_before(before, limit, verbose)?;
    assert!(items.len() <= limit, "the store honours the limit");
    Ok(Reply::Feed(items))
}

pub(super) async fn publish_report(
    ctx: &DaemonContext,
    title: &str,
    path: &Path,
    item: Option<&QueueItemId>,
) -> Result<Reply, ArchonError> {
    let title = checked_text(title, "a report title")?;
    if !path.is_absolute() {
        return Err(ArchonError::Invalid(format!(
            "report path {} must be absolute",
            path.display()
        )));
    }
    if let Some(id) = item {
        ctx.store.lock().await.item(id)?;
    }
    check_report_scope(ctx, path).await?;
    Ok(Reply::Report(
        reports::publish(ctx, &title, path, item).await?,
    ))
}

/// The report's directory is snapshotted, so it must lie in a registered
/// repository or under the item workspaces. A missing file is left to
/// `reports::publish` to report as not found.
async fn check_report_scope(ctx: &DaemonContext, path: &Path) -> Result<(), ArchonError> {
    assert!(
        path.is_absolute(),
        "report paths are checked absolute first"
    );
    let Ok(resolved) = path.canonicalize() else {
        return Ok(());
    };
    let mut roots = vec![ctx.paths.worktrees.clone()];
    roots.extend(
        ctx.store
            .lock()
            .await
            .repos()?
            .into_iter()
            .map(|repo| repo.path),
    );
    let inside = roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| resolved.starts_with(root));
    if inside {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "report {} is outside every registered repository and item workspace; \
         write the page in the item's workspace or the repository",
        path.display()
    )))
}

/// Trimmed, non-empty and bounded.
fn checked_text(text: &str, what: &str) -> Result<String, ArchonError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ArchonError::Invalid(format!("{what} cannot be empty")));
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(ArchonError::Invalid(format!(
            "{what} is longer than {MAX_TEXT_CHARS} characters"
        )));
    }
    Ok(text.to_owned())
}
