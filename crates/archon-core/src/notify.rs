//! What the operator sees. The chat is a filtered feed; these are the only
//! item kinds it renders.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::{AetherAgentId, AetherQuestionRef};
use crate::id::{FeedItemId, NotificationId, PipelineRunId, QuestionId, QueueItemId, ReportId};

/// The orchestrator's `notify(kind=...)` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// Needs an operator answer; pairs with a `Question`.
    Question,
    /// A choice the orchestrator made that the operator should know about.
    Decision,
    /// Work cannot proceed without operator action.
    Blocker,
    /// A queue item or run finished without requiring operator action.
    /// Terminal failures requiring intervention use `Blocker` instead.
    Outcome,
    /// Hidden by default; shown when the operator asks for progress.
    Progress,
}

impl NotificationKind {
    /// Whether the default (quiet) chat renders this kind.
    pub fn shown_by_default(self) -> bool {
        !matches!(self, Self::Progress)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub kind: NotificationKind,
    pub text: String,
    pub item: Option<QueueItemId>,
    pub run: Option<PipelineRunId>,
    pub at: Timestamp,
}

/// A question the orchestrator asked without blocking (Archon MCP
/// `ask_operator`). Aether's own `ask_user` questions are mirrored here too
/// so one place holds everything awaiting an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: QuestionId,
    /// The operator's `Q<n>`, the same in every listing and chat. The store
    /// assigns it when recording the question and never reuses it; 0 before.
    #[serde(default)]
    pub number: u64,
    pub text: String,
    /// Suggested answers, when the asker offered some.
    pub options: Vec<String>,
    pub item: Option<QueueItemId>,
    /// Set when the question came from Aether's `ask_user`; the answer then
    /// goes back through `reply_to_question` instead of a trigger.
    pub aether: Option<AetherQuestionRef>,
    /// The agent whose `ask_operator` recorded this, from Aether's caller
    /// metadata. `None` when nobody vouched for the asker; never the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<AetherAgentId>,
    pub asked_at: Timestamp,
    pub answer: Option<String>,
    pub answered_at: Option<Timestamp>,
}

/// How the operator names a question: `2`, `Q2` or `q_01J…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionRef {
    Number(u64),
    Id(QuestionId),
}

impl QuestionRef {
    pub fn parse(text: &str) -> Result<Self, String> {
        let digits = text.strip_prefix(['Q', 'q']).unwrap_or(text);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return match digits.parse::<u64>() {
                Ok(n) if n > 0 => Ok(Self::Number(n)),
                Ok(_) => Err(format!(
                    "{text:?} is not a question number (they start at 1)"
                )),
                Err(_) => Err(format!("{text:?} is too large to be a question number")),
            };
        }
        QuestionId::parse(text)
            .map(Self::Id)
            .map_err(|_| format!("{text:?} is neither a question number nor a q_… id"))
    }
}

impl Question {
    pub fn is_open(&self) -> bool {
        self.answer.is_none()
    }

    /// The answer text to record: a bare number picks that option when the
    /// question offered options, and one outside `1..=N` is refused; anything
    /// else is taken as typed.
    pub fn expand_option(&self, text: &str) -> Result<String, crate::ArchonError> {
        let text = text.trim();
        let bare_number = !text.is_empty() && text.chars().all(|c| c.is_ascii_digit());
        if self.options.is_empty() || !bare_number {
            return Ok(text.to_owned());
        }
        let picked = text
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| self.options.get(i));
        picked.cloned().ok_or_else(|| {
            crate::ArchonError::Invalid(format!("options are 1..{}", self.options.len()))
        })
    }
}

/// The surface an answer came in through; the audit mark of a feed answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerVia {
    /// archond's own socket (`archon answer`, the line chat), Lavish, or a
    /// withdrawal by the daemon.
    #[default]
    Archon,
    /// The operator's `/answer` command in the orchestrator's Aether chat,
    /// applied by the daemon.
    ChatCommand,
    /// The operator's chat reply to an `ask_operator` question, recorded by
    /// the orchestrator that asked it.
    ChatReply,
    /// An answer chosen in Aether's notification center, applied by the
    /// daemon as an `/answer` command is.
    NotificationCenter,
}

impl AnswerVia {
    pub fn is_archon(&self) -> bool {
        *self == Self::Archon
    }

    /// What a listing appends to such an answer; `None` for the usual way.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Archon => None,
            Self::ChatCommand => Some("answered from operator chat"),
            Self::ChatReply => Some("answered in chat, recorded by the orchestrator"),
            Self::NotificationCenter => Some("answered in the notification center"),
        }
    }
}

/// One line of the operator chat. Persisted so `archon chat` can replay.
/// Tagged `entry` because the wrapped `Notification` already has a `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum FeedEntry {
    /// The daemon observed an authenticated operator command.
    Authority {
        evidence: crate::authority::OperatorMessage,
        action: String,
        result: String,
        before: serde_json::Value,
        after: serde_json::Value,
    },
    Operator {
        text: String,
        /// What the images sent with the line were; Aether keeps the bytes.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::attachment::ImageRef>,
    },
    /// Final text of an orchestrator turn that answered the operator.
    Orchestrator {
        text: String,
    },
    Notification(Notification),
    Question(Question),
    Answer {
        question: QuestionId,
        text: String,
        #[serde(default, skip_serializing_if = "AnswerVia::is_archon")]
        via: AnswerVia,
    },
    /// A report published for review (Lavish URL or plain file path).
    Report {
        report: ReportId,
        title: String,
        location: String,
        item: Option<QueueItemId>,
    },
    /// Daemon-level notices: start/stop, orchestrator restarted, caps hit.
    System {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedItem {
    pub id: FeedItemId,
    /// Gapless per-store sequence for `SubscribeFeed { after }`.
    pub seq: u64,
    pub at: Timestamp,
    pub entry: FeedEntry,
}

impl FeedItem {
    /// The quiet-chat filter. `verbose` also shows progress notifications.
    pub fn visible(&self, verbose: bool) -> bool {
        match &self.entry {
            FeedEntry::Notification(n) => verbose || n.kind.shown_by_default(),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_numbers_q_numbers_and_ids() {
        assert_eq!(QuestionRef::parse("3"), Ok(QuestionRef::Number(3)));
        assert_eq!(QuestionRef::parse("Q12"), Ok(QuestionRef::Number(12)));
        let id = QuestionId::new();
        assert_eq!(QuestionRef::parse(id.as_str()), Ok(QuestionRef::Id(id)));
        assert!(QuestionRef::parse("0").is_err());
        assert!(QuestionRef::parse("qi_01").is_err());
    }

    #[test]
    fn a_number_too_large_to_parse_says_so() {
        let too_large = QuestionRef::parse("99999999999999999999").unwrap_err();
        assert!(too_large.contains("too large"), "{too_large}");
        let zero = QuestionRef::parse("0").unwrap_err();
        assert!(zero.contains("start at 1"), "{zero}");
    }

    fn progress_item() -> FeedItem {
        FeedItem {
            id: FeedItemId::new(),
            seq: 1,
            at: crate::now(),
            entry: FeedEntry::Notification(Notification {
                id: NotificationId::new(),
                kind: NotificationKind::Progress,
                text: "50%".into(),
                item: None,
                run: None,
                at: crate::now(),
            }),
        }
    }

    #[test]
    fn progress_is_hidden_unless_verbose() {
        let item = progress_item();
        assert!(!item.visible(false));
        assert!(item.visible(true));
    }

    #[test]
    fn notification_entry_round_trips_through_json() {
        let item = progress_item();
        let json = serde_json::to_string(&item).expect("serialize feed item");
        assert!(json.contains(r#""entry":"notification""#), "{json}");
        let back: FeedItem = serde_json::from_str(&json).expect("deserialize feed item");
        assert_eq!(back, item);
    }

    fn question(options: &[&str]) -> Question {
        Question {
            id: QuestionId::new(),
            number: 1,
            text: "which?".into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            item: None,
            aether: None,
            asked_by: None,
            asked_at: crate::now(),
            answer: None,
            answered_at: None,
        }
    }

    #[test]
    fn a_bare_number_picks_an_option() {
        let q = question(&["rebase", "merge"]);
        assert_eq!(q.expand_option("2").unwrap(), "merge");
        assert_eq!(q.expand_option(" 1 ").unwrap(), "rebase");
        assert_eq!(q.expand_option("merge please").unwrap(), "merge please");
        assert_eq!(question(&[]).expand_option("2").unwrap(), "2");
    }

    #[test]
    fn a_number_outside_the_options_is_refused() {
        let q = question(&["a", "b", "c"]);
        for text in ["7", "0", " 4 ", "99999999999999999999999"] {
            let error = q.expand_option(text).unwrap_err().to_string();
            assert!(error.contains("options are 1..3"), "{text:?}: {error}");
        }
    }

    /// An answer through archond's own socket keeps its old wire form, so an
    /// older client reads it; only another surface adds the mark.
    #[test]
    fn an_answer_names_its_surface_only_when_it_is_not_archons_own() {
        let answer = |via| FeedEntry::Answer {
            question: QuestionId::new(),
            text: "yes".into(),
            via,
        };
        let plain = serde_json::to_value(answer(AnswerVia::Archon)).unwrap();
        assert!(plain.get("via").is_none(), "{plain}");
        let back: FeedEntry = serde_json::from_value(plain).unwrap();
        assert!(matches!(
            back,
            FeedEntry::Answer {
                via: AnswerVia::Archon,
                ..
            }
        ));
        for via in [AnswerVia::ChatCommand, AnswerVia::ChatReply] {
            let entry = answer(via);
            let json = serde_json::to_value(&entry).unwrap();
            assert!(json["via"].is_string(), "{json}");
            assert_eq!(serde_json::from_value::<FeedEntry>(json).unwrap(), entry);
            assert!(via.note().is_some_and(|note| note.contains("chat")));
        }
        assert_eq!(AnswerVia::Archon.note(), None);
    }
}
