//! A confirmation carries its exact action, observed state and reply binding.

use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;
use archon_core::id::QuestionId;
use archon_core::notify::{AnswerVia, FeedEntry, Question};
use archon_core::protocol::{Reply, Request};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{actions, apply_audited};
use crate::{context::DaemonContext, feed, handlers};

const PREFIX: &str = "authority_confirmation:";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Confirmation {
    action: Request,
    before: Value,
    evidence: OperatorMessage,
    asked_at: archon_core::Timestamp,
    first_reply: Option<u64>,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Waiting,
    Executing,
    Applied,
    Declined,
}

fn key(id: &QuestionId) -> String {
    format!("{PREFIX}{id}")
}

async fn load(ctx: &DaemonContext, id: &QuestionId) -> Result<Option<Confirmation>, ArchonError> {
    ctx.store
        .lock()
        .await
        .kv_get(&key(id))?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}

pub(super) async fn is_confirmation(
    ctx: &DaemonContext,
    id: &QuestionId,
) -> Result<bool, ArchonError> {
    Ok(load(ctx, id).await?.is_some())
}

async fn save(
    ctx: &DaemonContext,
    id: &QuestionId,
    confirmation: &Confirmation,
) -> Result<(), ArchonError> {
    ctx.store
        .lock()
        .await
        .kv_set(&key(id), &serde_json::to_string(confirmation)?)
}

pub(super) async fn ask(
    ctx: &DaemonContext,
    evidence: &OperatorMessage,
    action: Request,
    before: Value,
) -> Result<Reply, ArchonError> {
    let now = archon_core::now();
    let text = format!(
        "Approve operator authority change?\nBefore: {}\nRequested change: {}\nReply yes or no, or /answer N yes. A bare reply applies only to your first message after this question, with exactly one confirmation open (same GitHub thread).",
        serde_json::to_string(&before)?,
        serde_json::to_string(&action)?
    );
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text,
        options: vec!["yes".into(), "no".into()],
        item: None,
        aether: None,
        asked_by: None,
        asked_at: now,
        answer: None,
        answered_at: None,
    };
    let confirmation = Confirmation {
        action,
        before,
        evidence: evidence.clone(),
        asked_at: now,
        first_reply: None,
        state: State::Waiting,
    };
    // Persist the restricted route first. A crash may leave an invisible
    // record; it cannot turn a confirmation into an orchestrator question.
    save(ctx, &question.id, &confirmation).await?;
    let question = ctx.store.lock().await.insert_question(&question)?;
    feed::append(ctx, FeedEntry::Question(question.clone())).await?;
    super::audit(
        ctx,
        evidence,
        "confirmation",
        "awaiting operator",
        confirmation.before,
        serde_json::to_value(&confirmation.action)?,
    )
    .await?;
    Ok(Reply::Question(question))
}

pub(super) async fn check_answer(
    ctx: &DaemonContext,
    id: &QuestionId,
    evidence: &OperatorMessage,
    answer: &str,
) -> Result<(), ArchonError> {
    let Some(pending) = load(ctx, id).await? else {
        return Ok(());
    };
    actions::check_channel(ctx, evidence, &pending.action).await?;
    use archon_core::authority::OperatorChannel;
    if !matches!(
        evidence.channel,
        OperatorChannel::Cli | OperatorChannel::ArchonChat
    ) && evidence.channel != pending.evidence.channel
    {
        return Err(ArchonError::Forbidden(
            "answer this confirmation in its original channel and GitHub thread".into(),
        ));
    }
    if evidence.channel.is_github()
        && !ctx.config.authority.github_may_loosen
        && yes_no(answer) == Some(true)
    {
        return Err(ArchonError::Forbidden(
            "GitHub loosening is disabled by authority.github_may_loosen".into(),
        ));
    }

    if evidence.at < pending.asked_at {
        return Err(ArchonError::Forbidden(
            "the operator answer predates the confirmation".into(),
        ));
    }
    Ok(())
}

pub(super) async fn decide(
    ctx: &DaemonContext,
    question: &Question,
    text: &str,
) -> Result<bool, ArchonError> {
    let _changes = ctx.authority_changes.lock().await;
    let Some(mut pending) = load(ctx, &question.id).await? else {
        return Ok(false);
    };
    let Some(yes) = yes_no(text) else {
        return Err(ArchonError::Invalid(
            "answer this authority confirmation with yes or no".into(),
        ));
    };
    if pending.state != State::Waiting && (yes || pending.state != State::Executing) {
        return Err(ArchonError::InvalidTransition("this authority confirmation already ended or began execution; inspect settings before requesting another".into()));
    }
    if !yes {
        let outcome = if pending.state == State::Executing {
            "closed after an interrupted operation; inspect settings, no effects were undone"
        } else {
            "declined"
        };
        pending.state = State::Declined;
        save(ctx, &question.id, &pending).await?;
        super::audit(
            ctx,
            &pending.evidence,
            "confirmation",
            outcome,
            pending.before,
            Value::Null,
        )
        .await?;
        return Ok(true);
    }
    actions::check_channel(ctx, &pending.evidence, &pending.action).await?;
    if pending.evidence.channel.is_github() && !ctx.config.authority.github_may_loosen {
        return Err(ArchonError::Forbidden(
            "GitHub loosening is disabled by authority.github_may_loosen".into(),
        ));
    }
    let current = actions::snapshot(ctx, &pending.action).await?;
    if current != pending.before {
        return Err(ArchonError::InvalidTransition(
            "settings changed since this question; reject it and request a fresh confirmation"
                .into(),
        ));
    }
    // Persist before effects: a crash or ambiguous external failure must not
    // replay an approved operation just because its question is still open.
    pending.state = State::Executing;
    save(ctx, &question.id, &pending).await?;
    apply_audited(
        ctx,
        &pending.evidence,
        pending.action.clone(),
        pending.before.clone(),
    )
    .await?;
    pending.state = State::Applied;
    save(ctx, &question.id, &pending).await?;
    Ok(true)
}

fn yes_no(text: &str) -> Option<bool> {
    match text
        .trim()
        .trim_end_matches(['.', '!'])
        .to_ascii_lowercase()
        .as_str()
    {
        "yes" | "y" | "approve" | "approved" | "go ahead" | "please go ahead" => Some(true),
        "no" | "n" | "reject" | "cancel" => Some(false),
        _ => None,
    }
}

pub(super) async fn bare_reply(
    ctx: &DaemonContext,
    evidence: &OperatorMessage,
) -> Result<bool, ArchonError> {
    let changes = ctx.authority_changes.lock().await;
    let open = ctx.store.lock().await.questions(true)?;
    let mut confirmations = Vec::new();
    let mut other_confirmations = 0;
    for question in open {
        if ctx
            .store
            .lock()
            .await
            .merge_by_question(&question.id)?
            .is_some()
        {
            other_confirmations += 1;
        }
        if let Some(pending) = load(ctx, &question.id).await?
            && pending.state == State::Waiting
        {
            confirmations.push((question, pending));
        }
    }
    let only_one = confirmations.len() + other_confirmations == 1;
    let mut target = None;
    for (question, mut pending) in confirmations {
        if pending.evidence.agent != evidence.agent || evidence.at < pending.asked_at {
            continue;
        }
        if pending.first_reply.is_none() {
            pending.first_reply = Some(evidence.seq);
            save(ctx, &question.id, &pending).await?;
            if only_one && pending.evidence.channel == evidence.channel {
                target = Some(question);
            }
        }
    }
    let Some(question) = target else {
        return Ok(false);
    };
    let Some(yes) = yes_no(crate::operator_commands::command_text(
        &evidence.text,
        evidence.channel.is_github(),
    )) else {
        return Ok(false);
    };
    drop(changes);
    check_answer(ctx, &question.id, evidence, if yes { "yes" } else { "no" }).await?;
    super::audit(
        ctx,
        evidence,
        "answer confirmation",
        "received",
        serde_json::to_value(&question)?,
        serde_json::json!({"answer": yes}),
    )
    .await?;
    handlers::answer_question(
        ctx,
        &question.id,
        if yes { "yes" } else { "no" },
        AnswerVia::ChatCommand,
    )
    .await?;
    Ok(true)
}
