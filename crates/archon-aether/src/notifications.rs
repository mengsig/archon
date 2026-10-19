//! Aether's notification center (feature `notifications`): Archon posts its
//! operator questions, blockers and outcomes there under the source
//! `archon`, and reads the operator's answers back from the
//! `notification.answered` events Aether records.

use archon_core::error::ArchonError;
use serde::{Deserialize, Serialize};

use crate::client::AetherClient;
use crate::events::{AetherEvent, EventKind};
use crate::wire::{CLIENT_NAME, Command, NOTIFICATIONS_FEATURE};

/// The service name every Archon notification is posted under.
pub const SOURCE: &str = CLIENT_NAME;
/// Aether's limits on a post (`aether_core::notification`).
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_BODY_BYTES: usize = 4096;
pub const MAX_KEY_BYTES: usize = 200;
pub const MAX_ACTIONS: usize = 8;
pub const MAX_ACTION_LABEL_CHARS: usize = 64;
/// The most one `list_notifications` returns.
pub const MAX_LIST: usize = 1000;
/// The `custom` event an operator's answer is recorded under.
pub const ANSWERED_EVENT: &str = "notification.answered";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Question,
    Blocker,
    Decision,
    Outcome,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationState {
    Unread,
    Read,
    Resolved,
}

/// A choice the operator can make; its `id` is what comes back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    pub id: String,
    pub label: String,
}

impl NotificationAction {
    /// Option `number` (from 1) of a question, its label cut to one line;
    /// the id is the number, as `/answer <question> <number>` takes it.
    pub fn option(number: usize, label: &str) -> Self {
        assert!(number >= 1, "options are numbered from 1");
        let label = one_line(label, MAX_ACTION_LABEL_CHARS);
        let id = number.to_string();
        Self {
            label: if label.is_empty() { id.clone() } else { label },
            id,
        }
    }
}

/// What Archon posts. The same `key` posted again updates that notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotificationPost {
    pub kind: NotificationKind,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<NotificationAction>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub free_text: bool,
}

impl NotificationPost {
    /// A post without actions: the title cut to one line and the body to
    /// Aether's limits, so a long question never makes the post refused.
    pub fn new(kind: NotificationKind, key: &str, title: &str, body: &str) -> Self {
        assert!(!key.is_empty(), "every Archon notification is keyed");
        assert!(key.len() <= MAX_KEY_BYTES, "keys are Archon ids, not text");
        let title = one_line(title, MAX_TITLE_CHARS);
        let post = Self {
            kind,
            title: if title.is_empty() {
                key.to_owned()
            } else {
                title
            },
            body: clip_bytes(body.trim(), MAX_BODY_BYTES),
            key: key.to_owned(),
            link: None,
            actions: Vec::new(),
            free_text: false,
        };
        assert!(post.body.len() <= MAX_BODY_BYTES, "the body fits Aether");
        post
    }
}

/// The operator's answer: a chosen action, their own words, or both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NotificationAnswer {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// Aether's actor that answered, such as `{"actor": "user"}`.
    #[serde(default)]
    pub by: serde_json::Value,
}

impl NotificationAnswer {
    /// Whether a person answered (Aether's `user` actor), not an agent or a rule.
    pub fn by_operator(&self) -> bool {
        self.by.get("actor").and_then(|a| a.as_str()) == Some("user")
    }
}

/// A notification as Aether stores it, narrowed to what Archon reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Notification {
    pub id: String,
    pub source: String,
    /// Kept as text, so a kind added by a newer Aether still decodes.
    pub kind: String,
    #[serde(default)]
    pub key: Option<String>,
    pub state: NotificationState,
    #[serde(default)]
    pub answer: Option<NotificationAnswer>,
}

impl Notification {
    pub fn is_open(&self) -> bool {
        self.state != NotificationState::Resolved
    }
}

impl AetherClient {
    /// Whether the daemon serves the notification center; an older one
    /// would refuse every call below as an unknown command.
    pub fn serves_notifications(&self) -> bool {
        self.welcome()
            .features
            .iter()
            .any(|feature| feature == NOTIFICATIONS_FEATURE)
    }

    /// Post as `archon`, or update the notification `post.key` names.
    pub async fn post_notification(
        &self,
        post: NotificationPost,
    ) -> Result<Notification, ArchonError> {
        self.require_notifications()?;
        assert!(post.actions.len() <= MAX_ACTIONS, "Aether takes 8 actions");
        let key = post.key.clone();
        let command = Command::PostNotification {
            source: SOURCE.to_owned(),
            notification: post,
        };
        let posted: Notification = self.typed(command, "notification").await?;
        if posted.source != SOURCE || posted.key.as_deref() != Some(key.as_str()) {
            return Err(ArchonError::Protocol(format!(
                "post_notification of {key} answered about {:?} of {}",
                posted.key, posted.source
            )));
        }
        Ok(posted)
    }

    /// Archon's own notifications in every state, newest first.
    pub async fn archon_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<Notification>, ArchonError> {
        self.require_notifications()?;
        assert!(
            (1..=MAX_LIST).contains(&limit),
            "Aether caps a list at 1000"
        );
        let command = Command::ListNotifications {
            states: Vec::new(),
            source: Some(SOURCE.to_owned()),
            limit: Some(limit),
        };
        let listed: Vec<Notification> = self.typed(command, "notifications").await?;
        if listed.len() > limit || listed.iter().any(|n| n.source != SOURCE) {
            return Err(ArchonError::Protocol(format!(
                "list_notifications is not at most {limit} notifications of {SOURCE}"
            )));
        }
        Ok(listed)
    }

    /// Resolve the notification posted under `key`; `false` when Aether
    /// holds none, which is fine: nothing is left to resolve.
    pub async fn resolve_notification(&self, key: &str) -> Result<bool, ArchonError> {
        self.require_notifications()?;
        assert!(!key.is_empty(), "every Archon notification is keyed");
        let command = Command::ResolveNotification {
            source: SOURCE.to_owned(),
            key: key.to_owned(),
        };
        match self.typed::<Notification>(command, "notification").await {
            Ok(resolved) if resolved.is_open() => Err(ArchonError::Protocol(format!(
                "resolve_notification left {key} {:?}",
                resolved.state
            ))),
            Ok(_) => Ok(true),
            Err(ArchonError::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn require_notifications(&self) -> Result<(), ArchonError> {
        if self.serves_notifications() {
            return Ok(());
        }
        let welcome = self.welcome();
        Err(ArchonError::Unavailable(format!(
            "aetherd {} does not serve the notification center",
            welcome.daemon_version
        )))
    }
}

/// The record of an answer to one of Archon's notifications, when `event`
/// is one; `Ok(None)` for every other event.
pub fn answered(event: &AetherEvent) -> Result<Option<Notification>, ArchonError> {
    let EventKind::Custom { name, payload } = &event.kind else {
        return Ok(None);
    };
    if name != ANSWERED_EVENT || payload.get("source").and_then(|s| s.as_str()) != Some(SOURCE) {
        return Ok(None);
    }
    let notification: Notification = serde_json::from_value(payload.clone()).map_err(|e| {
        ArchonError::Protocol(format!(
            "{ANSWERED_EVENT} {} did not decode: {e}",
            event.seq
        ))
    })?;
    if notification.answer.is_none() || notification.key.is_none() {
        return Err(ArchonError::Protocol(format!(
            "{ANSWERED_EVENT} {} carries no keyed answer",
            event.seq
        )));
    }
    Ok(Some(notification))
}

/// The first line of `text`, trimmed and cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// `text` cut on a character boundary to at most `max` bytes.
fn clip_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let ellipsis = '…';
    let mut end = max - ellipsis.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ellipsis}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_post_fits_aethers_limits_whatever_the_question() {
        let long = format!("{}\nsecond line", "é".repeat(300));
        let post =
            NotificationPost::new(NotificationKind::Question, "q_1", &long, &"ü".repeat(5000));
        assert_eq!(post.title.chars().count(), MAX_TITLE_CHARS);
        assert!(!post.title.contains('\n'));
        assert!(post.body.len() <= MAX_BODY_BYTES, "{}", post.body.len());
        let blank = NotificationPost::new(NotificationKind::Outcome, "ntf_1", "  ", "");
        assert_eq!(blank.title, "ntf_1", "a title is never empty");
        let wire = serde_json::to_value(&blank).unwrap();
        assert_eq!(
            wire,
            json!({"kind": "outcome", "title": "ntf_1", "key": "ntf_1"})
        );
    }

    fn custom(name: &str, payload: serde_json::Value) -> AetherEvent {
        let raw = json!({"seq": 7, "at": "2026-10-19T00:00:00Z",
                         "kind": {"type": "custom", "name": name, "payload": payload}});
        crate::events::parse_event(raw).unwrap()
    }

    #[test]
    fn only_an_answer_to_an_archon_notification_is_read_back() {
        let record = |source: &str| {
            json!({"id": "ntf_1", "source": source, "kind": "question", "title": "Q1",
                   "key": "q_1", "state": "resolved", "created_at": "2026-10-19T00:00:00Z",
                   "answer": {"action": "2", "by": {"kind": "user"}, "at": "2026-10-19T00:00:00Z"}})
        };
        let answered_event = custom(ANSWERED_EVENT, record(SOURCE));
        let notification = answered(&answered_event).unwrap().expect("archon's answer");
        assert_eq!(notification.key.as_deref(), Some("q_1"));
        assert_eq!(notification.answer.unwrap().action.as_deref(), Some("2"));
        assert_eq!(
            answered(&custom(ANSWERED_EVENT, record("other"))).unwrap(),
            None
        );
        assert_eq!(
            answered(&custom("notification.posted", record(SOURCE))).unwrap(),
            None
        );
        let unanswered = json!({"id": "ntf_1", "source": SOURCE, "kind": "question",
                                "key": "q_1", "state": "resolved"});
        assert!(answered(&custom(ANSWERED_EVENT, unanswered)).is_err());
    }
}
