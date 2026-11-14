//! Which orchestrator text answers the operator, decided as archond 1.5.0's
//! `ReplyTracker` did before it wrote each answer to the feed: the last
//! non-empty text of a turn that consumed an operator message, and of each
//! turn that continues it after a wait that ends by itself. Trigger-woken
//! turns and text written before the message was read never count.

use archon_aether::AetherEvent;
use archon_aether::events::EventKind;
use archon_core::Timestamp;

/// One answer, as 1.5.0 would have posted it to the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub at: Timestamp,
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReplyTracker {
    /// Inbox seqs of operator messages no turn has consumed yet, ascending.
    pending: Vec<u64>,
    /// A consumed operator message awaits this or a continuing turn's text.
    answering: bool,
    /// The last non-empty text written since the message was consumed.
    text: Option<String>,
    /// The running turn ends in a wait that ends by itself.
    resumes: bool,
    /// Operator messages the current answer covers.
    batch: u32,
    /// Earlier texts of a batched answer, before `text`.
    earlier: Vec<String>,
}

impl ReplyTracker {
    /// Apply one of the orchestrator's events, in seq order; the answer it
    /// finishes, if any.
    pub fn apply(&mut self, event: &AetherEvent) -> Option<Answer> {
        let text = match &event.kind {
            EventKind::InboxDelivered {
                seq, from_operator, ..
            } => {
                self.delivered(*seq, *from_operator);
                None
            }
            EventKind::InboxConsumed { up_to } => self.consumed(*up_to),
            EventKind::AgentMessage {
                assistant: true,
                text,
            } => {
                self.assistant_text(text);
                None
            }
            EventKind::AgentWaiting { idle } => {
                self.resumes = !idle;
                None
            }
            EventKind::AgentTurnCompleted { next, .. } => self.turn_completed(next.as_deref()),
            EventKind::AgentFailed { .. }
            | EventKind::AgentCancelled { .. }
            | EventKind::AgentCompleted => {
                self.interrupted();
                None
            }
            _ => None,
        };
        assert!(
            text.as_ref().is_none_or(|t| !t.trim().is_empty()),
            "an answer says something"
        );
        text.map(|text| Answer { at: event.at, text })
    }

    fn delivered(&mut self, seq: u64, from_operator: bool) {
        assert!(seq > 0, "inbox seqs start at 1");
        if from_operator && self.pending.last().is_none_or(|last| *last < seq) {
            self.pending.push(seq);
        }
    }

    /// Messages read together, or before the answer has any text, share one
    /// answer; one read after text was written finishes the answer so far.
    fn consumed(&mut self, up_to: u64) -> Option<String> {
        let before = self.pending.len();
        self.pending.retain(|seq| *seq > up_to);
        let count = u32::try_from(before - self.pending.len()).expect("a short inbox");
        if count == 0 {
            return None;
        }
        if self.answering && self.text.is_none() {
            self.batch += count;
            return None;
        }
        let finished = if self.answering {
            self.take_reply()
        } else {
            None
        };
        self.answering = true;
        self.batch = count;
        finished
    }

    fn assistant_text(&mut self, text: &str) {
        if !self.answering || text.trim().is_empty() {
            return;
        }
        if self.batch > 1 {
            self.earlier.extend(self.text.take());
        }
        self.text = Some(text.to_owned());
    }

    fn turn_completed(&mut self, next: Option<&str>) -> Option<String> {
        let resumes = std::mem::take(&mut self.resumes) && next == Some("waiting");
        if !self.answering {
            return None;
        }
        if matches!(next, Some("failed" | "cancelled")) {
            self.interrupted();
            return None;
        }
        self.answering = resumes;
        let reply = self.take_reply();
        if !resumes {
            self.batch = 0;
        }
        reply
    }

    fn interrupted(&mut self) {
        self.text = None;
        self.earlier.clear();
    }

    fn take_reply(&mut self) -> Option<String> {
        let last = self.text.take()?;
        let mut parts = std::mem::take(&mut self.earlier);
        parts.push(last);
        let body = parts.join("\n\n");
        if self.batch > 1 {
            return Some(format!(
                "Replying to your {} messages:\n\n{body}",
                self.batch
            ));
        }
        Some(body)
    }
}
