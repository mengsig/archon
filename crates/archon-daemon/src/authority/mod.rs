//! Operator authority is a daemon decision backed by an observed message.

mod actions;
pub(crate) mod commands;
mod confirmations;
pub(crate) mod evidence;
mod history;
pub(crate) mod item_grants;
mod settings;

use archon_core::aether_ids::AetherAgentId;
use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, Question};
use archon_core::protocol::{Reply, Request};
use serde_json::Value;

use crate::{context::DaemonContext, feed, supervisor};

pub(crate) async fn request(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
    seq: u64,
    quote: &str,
    reason: &str,
    action: Request,
) -> Result<Reply, ArchonError> {
    let current = supervisor::current_orchestrator(ctx).await?;
    let caller = caller
        .filter(|id| Some(*id) == current.as_ref())
        .ok_or_else(|| {
            ArchonError::Forbidden(
                "request_authority needs the current orchestrator's Aether caller metadata".into(),
            )
        })?;
    if reason.trim().is_empty() || reason.chars().count() > 500 {
        return Err(ArchonError::Invalid(
            "reason must contain 1–500 characters".into(),
        ));
    }
    let message = evidence::cite(ctx, caller, seq, quote).await?;
    if let Request::Answer { question, text } = &action {
        return answer_gate(ctx, &message, question, text).await;
    }
    let _changes = ctx.authority_changes.lock().await;
    perform(ctx, &message, action, false).await
}

pub(crate) async fn command(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    text: &str,
) -> Option<Result<Reply, ArchonError>> {
    // Parsing a partial settings command reads its scope's current overlay.
    // Keep that read and the replacement together, avoiding lost updates.
    let _changes = ctx.authority_changes.lock().await;
    let action = commands::parse(ctx, text).await?;
    Some(match action {
        Ok(action) => perform(ctx, message, action, true).await,
        Err(error) => Err(error),
    })
}

/// Called with authority_changes held by the authenticated operator socket.
pub(crate) async fn socket_settings(
    ctx: &DaemonContext,
    action: Request,
) -> Result<Reply, ArchonError> {
    let text = serde_json::to_string(&action)?;
    let entry = feed::append(ctx, FeedEntry::Operator { text: text.clone() }).await?;
    let evidence = OperatorMessage {
        agent: None,
        seq: entry.seq,
        channel: archon_core::authority::OperatorChannel::Cli,
        text,
        at: entry.at,
    };
    let before = actions::snapshot(ctx, &action).await?;
    apply_audited(ctx, &evidence, action, before).await
}

async fn perform(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    action: Request,
    explicit: bool,
) -> Result<Reply, ArchonError> {
    let action = history::resolve(ctx, action).await?;
    actions::check_channel(ctx, message, &action)?;
    let before = actions::snapshot(ctx, &action).await?;
    if !explicit && actions::needs_confirmation(ctx, &action, &before).await? {
        if message.channel.is_github() && !ctx.config.authority.github_may_loosen {
            return Err(ArchonError::Forbidden(
                "GitHub loosening is disabled by authority.github_may_loosen".into(),
            ));
        }
        return confirmations::ask(ctx, message, action, before).await;
    }
    if explicit
        && message.channel.is_github()
        && !ctx.config.authority.github_may_loosen
        && actions::needs_confirmation(ctx, &action, &before).await?
    {
        return Err(ArchonError::Forbidden(
            "GitHub loosening is disabled by authority.github_may_loosen".into(),
        ));
    }
    apply_audited(ctx, message, action, before).await
}

async fn answer_gate(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    id: &archon_core::id::QuestionId,
    text: &str,
) -> Result<Reply, ArchonError> {
    let question = {
        let store = ctx.store.lock().await;
        if store.gate_by_question(id)?.is_none() {
            return Err(ArchonError::Forbidden("plain-language authority answers only proposal/report gates; confirmations need the operator's own reply".into()));
        }
        store.question(id)?
    };
    if message.at < question.asked_at {
        return Err(ArchonError::Forbidden(
            "the operator message predates this gate; ask again".into(),
        ));
    }
    let before = serde_json::to_value(&question)?;
    audit(
        ctx,
        message,
        "answer gate",
        "requested",
        before.clone(),
        serde_json::json!({"answer":text}),
    )
    .await?;
    let answered = crate::handlers::answer_question(
        ctx,
        id,
        text,
        archon_core::notify::AnswerVia::ChatCommand,
    )
    .await?;
    audit(
        ctx,
        message,
        "answer gate",
        "applied",
        before,
        serde_json::to_value(&answered)?,
    )
    .await?;
    Ok(Reply::Question(answered))
}

pub(crate) async fn apply_audited(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    action: Request,
    before: Value,
) -> Result<Reply, ArchonError> {
    let label = crate::handlers::command_name(&action);
    audit(
        ctx,
        message,
        &label,
        "requested",
        before.clone(),
        serde_json::to_value(&action)?,
    )
    .await?;
    match actions::apply(ctx, action.clone()).await {
        Ok(reply) => {
            let after = actions::snapshot(ctx, &action).await?;
            history::record(ctx, message, &action, before, after).await?;
            Ok(reply)
        }
        Err(error) => {
            if !error.is_store() {
                audit(
                    ctx,
                    message,
                    &label,
                    &format!("refused: {error}"),
                    before,
                    Value::Null,
                )
                .await?;
            }
            Err(error)
        }
    }
}

pub(crate) async fn audit(
    ctx: &DaemonContext,
    message: &OperatorMessage,
    action: &str,
    result: &str,
    before: Value,
    after: Value,
) -> Result<(), ArchonError> {
    feed::append(
        ctx,
        FeedEntry::Authority {
            evidence: message.clone(),
            action: action.into(),
            result: result.into(),
            before,
            after,
        },
    )
    .await
    .map(drop)
}

pub(crate) async fn decide(
    ctx: &DaemonContext,
    question: &Question,
    text: &str,
) -> Result<bool, ArchonError> {
    confirmations::decide(ctx, question, text).await
}

pub(crate) async fn check_answer(
    ctx: &DaemonContext,
    id: &archon_core::id::QuestionId,
    message: &OperatorMessage,
    answer: &str,
) -> Result<(), ArchonError> {
    confirmations::check_answer(ctx, id, message, answer).await
}

pub(crate) async fn bare_reply(
    ctx: &DaemonContext,
    message: &OperatorMessage,
) -> Result<bool, ArchonError> {
    confirmations::bare_reply(ctx, message).await
}

pub(crate) fn guide(topic: &str) -> Result<Reply, ArchonError> {
    let text = match topic {
        "authority" => include_str!("../../prompts/topics/authority.md"),
        "settings" => include_str!("../../prompts/topics/settings.md"),
        "github" => include_str!("../../prompts/topics/github.md"),
        "models" => include_str!("../../prompts/topics/models.md"),
        _ => {
            return Err(ArchonError::Invalid(
                "unknown guide topic; choose authority, settings, github or models".into(),
            ));
        }
    };
    Ok(Reply::Guide { text: text.into() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn topics_stay_within_their_individual_budget() {
        for topic in ["authority", "settings", "github", "models"] {
            let archon_core::protocol::Reply::Guide { text } = super::guide(topic).unwrap() else {
                panic!("guide reply")
            };
            assert!(text.len() <= 4096, "{topic}: {} bytes", text.len());
        }
        assert!(super::guide("../../config.toml").is_err());
    }

    /// The orchestrator's prompt carries only GitHub's standing rules and
    /// points here for the workflow of each request.
    #[test]
    fn the_github_topic_holds_the_per_request_workflow() {
        let archon_core::protocol::Reply::Guide { text } = super::guide("github").unwrap() else {
            panic!("guide reply")
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for said in [
            "reply on its originating thread with `github_reply`",
            "Match the `list_repos` path by its `git remote get-url origin`.",
            "pass the thread as `origin`, reply with the item id",
            "`item_finished` carries the same origin: post its outcome to that thread.",
        ] {
            assert!(text.contains(said), "{said:?}");
        }
        assert!(!crate::supervisor::github_policy().contains("git remote get-url origin"));
    }

    #[test]
    fn the_models_topic_trades_tiers_against_effort_without_naming_models() {
        let archon_core::protocol::Reply::Guide { text } = super::guide("models").unwrap() else {
            panic!("guide reply")
        };
        assert_eq!(archon_core::testing::model_name_in(&text), None);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for said in [
            "Effort buys deliberation, not capability",
            "one stronger-tier judge at low effort",
            "another provider at the same tier",
        ] {
            assert!(text.contains(said), "{said:?}");
        }
    }
}
