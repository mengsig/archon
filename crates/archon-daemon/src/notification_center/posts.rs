//! What Archon posts to the notification center: an open question with
//! its options as actions, or a blocker or outcome without any.

use archon_aether::notifications::{
    MAX_ACTIONS, NotificationAction, NotificationKind, NotificationPost,
};
use archon_core::config::AetherMode;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::{self, Question};

use crate::context::DaemonContext;

/// The post of an open question, or `None` for an Aether `ask_user`, which
/// Aether shows the operator itself.
pub(super) async fn question(
    ctx: &DaemonContext,
    question: &Question,
) -> Result<Option<NotificationPost>, ArchonError> {
    assert!(question.number > 0, "a recorded question is numbered");
    assert!(question.is_open(), "only an open question is posted");
    if question.aether.is_some() {
        return Ok(None);
    }
    let confirmation = crate::authority::is_confirmation(ctx, &question.id).await?;
    let answerable = ctx.config.aether.mode == AetherMode::Dedicated && !confirmation;
    let number = question.number;
    let mut lines = Vec::new();
    if let Some(item) = &question.item {
        lines.push(item_line(ctx, item).await?);
    }
    lines.push(reason(ctx, question, confirmation).await?);
    lines.push(match answerable {
        true => format!("Choose an answer here, or reply `/answer {number} <answer>` in chat."),
        false if confirmation => "Answer it in the channel it was asked in.".to_owned(),
        false => format!(
            "Answer with `archon answer {number} <answer>`: answers here are off while Archon shares this Aether."
        ),
    });
    lines.push(String::new());
    lines.push(question.text.clone());
    let title = format!("Q{number}: {}", question.text);
    let mut post = NotificationPost::new(
        NotificationKind::Question,
        question.id.as_str(),
        &title,
        &lines.join("\n"),
    );
    post.link = question.asked_by.as_ref().map(ToString::to_string);
    if answerable {
        post.actions = actions(&question.options);
        post.free_text = true;
    }
    Ok(Some(post))
}

/// Each suggested answer as an action, numbered as `/answer` numbers them;
/// past Aether's limit they remain answerable as text.
fn actions(options: &[String]) -> Vec<NotificationAction> {
    let actions: Vec<NotificationAction> = options
        .iter()
        .take(MAX_ACTIONS)
        .enumerate()
        .map(|(i, option)| NotificationAction::option(i + 1, option))
        .collect();
    assert!(
        actions.len() <= MAX_ACTIONS,
        "Aether takes at most 8 actions"
    );
    actions
}

/// One line on why the question is asked.
async fn reason(
    ctx: &DaemonContext,
    question: &Question,
    confirmation: bool,
) -> Result<String, ArchonError> {
    if confirmation {
        return Ok("An authority change waits for your confirmation.".to_owned());
    }
    let gate = ctx.store.lock().await.gate_by_question(&question.id)?;
    if let Some(gate) = gate {
        return Ok(format!(
            "Gate: the {} waits for your decision.",
            gate.subject.noun()
        ));
    }
    Ok(match &question.asked_by {
        Some(agent) => format!("Asked by agent {agent}."),
        None => "Archon asks.".to_owned(),
    })
}

/// The post of a blocker or outcome, keyed by its feed notification; `None`
/// for any other kind.
pub(super) async fn notice(
    ctx: &DaemonContext,
    notification: &notify::Notification,
) -> Result<Option<NotificationPost>, ArchonError> {
    assert!(
        !notification.text.trim().is_empty(),
        "a notice says something"
    );
    let kind = match notification.kind {
        notify::NotificationKind::Blocker => NotificationKind::Blocker,
        notify::NotificationKind::Outcome => NotificationKind::Outcome,
        _ => return Ok(None),
    };
    let mut body = Vec::new();
    if let Some(item) = &notification.item {
        body.push(item_line(ctx, item).await?);
    }
    body.push(notification.text.clone());
    let key = notification.id.as_str();
    let post = NotificationPost::new(kind, key, &notification.text, &body.join("\n\n"));
    assert_eq!(post.key, key, "a notice is keyed by its feed notification");
    Ok(Some(post))
}

/// `Item `qi_…`: <title>`, or the id alone when the item is gone.
async fn item_line(ctx: &DaemonContext, item: &QueueItemId) -> Result<String, ArchonError> {
    match ctx.store.lock().await.item(item) {
        Ok(found) => Ok(format!("Item `{item}`: {}", found.title)),
        Err(ArchonError::NotFound(_)) => Ok(format!("Item `{item}`")),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_become_numbered_actions_up_to_aethers_limit() {
        let options: Vec<String> = (1..=10).map(|n| format!("option {n}")).collect();
        let actions = actions(&options);
        assert_eq!(actions.len(), MAX_ACTIONS);
        assert_eq!(actions[0].id, "1");
        assert_eq!(actions[0].label, "option 1");
        assert_eq!(actions[7].id, "8");
        assert!(super::actions(&[]).is_empty());
    }
}
