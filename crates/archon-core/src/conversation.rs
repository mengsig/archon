//! The operator conversation: the orchestrator's Aether transcript, read as
//! Aether's `get_conversation` folds it. archond, `archon chat` and the phone
//! show its important entries, the ones the Aether TUI's `/focus` shows.

use serde::{Deserialize, Serialize};

use crate::Timestamp;

/// Who said an entry, as Aether names it; a kind this Archon does not know
/// reads as `Other` and still shows its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// What the operator sent.
    Operator,
    /// The orchestrator's own text, including text between its tool calls.
    Assistant,
    /// A readable post a host appended, such as archond's notices.
    Post,
    /// The orchestrator asking the operator (`ask_user`).
    Question,
    #[serde(other)]
    Other,
}

/// One entry, identified and ordered by `(seq, part)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationEntry {
    /// The Aether event it came from.
    pub seq: u64,
    /// Its place among the entries of the same event.
    pub part: u32,
    pub at: Timestamp,
    pub kind: EntryKind,
    pub important: bool,
    /// Markdown.
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}

/// One page of entries. Page backwards with `before: from_seq` and forwards
/// with `after: to_seq`; both are 0 for a page that read no event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationPage {
    pub entries: Vec<ConversationEntry>,
    pub from_seq: u64,
    pub to_seq: u64,
    pub has_before: bool,
    pub has_after: bool,
    /// The newest conversation event of the agent.
    pub last_seq: u64,
}

impl ConversationPage {
    /// Whether the entries are in `(seq, part)` order inside the page's span.
    pub fn is_ordered(&self) -> bool {
        let in_span = self
            .entries
            .iter()
            .all(|e| self.from_seq <= e.seq && e.seq <= self.to_seq);
        let ascending = self
            .entries
            .windows(2)
            .all(|w| (w[0].seq, w[0].part) < (w[1].seq, w[1].part));
        in_span && ascending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, part: u32) -> ConversationEntry {
        ConversationEntry {
            seq,
            part,
            at: crate::now(),
            kind: EntryKind::Assistant,
            important: true,
            text: "hi".into(),
            links: Vec::new(),
        }
    }

    #[test]
    fn an_unknown_kind_reads_as_other() {
        let wire = serde_json::json!({
            "seq": 3, "part": 0, "at": "2026-11-13T10:00:00Z", "kind": "hologram",
            "important": true, "text": "x",
        });
        let entry: ConversationEntry = serde_json::from_value(wire).unwrap();
        assert_eq!(entry.kind, EntryKind::Other);
        assert!(entry.links.is_empty());
    }

    #[test]
    fn a_page_is_ordered_only_inside_its_span() {
        let mut page = ConversationPage {
            entries: vec![entry(2, 0), entry(2, 1), entry(4, 0)],
            from_seq: 2,
            to_seq: 4,
            ..ConversationPage::default()
        };
        assert!(page.is_ordered());
        page.entries.swap(0, 1);
        assert!(!page.is_ordered());
        page.entries.swap(0, 1);
        page.to_seq = 3;
        assert!(!page.is_ordered());
    }
}
