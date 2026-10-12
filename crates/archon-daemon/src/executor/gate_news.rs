//! The `gate_opened` news of an operator gate (ARCHITECTURE §5.4): what the
//! orchestrator needs to restate the gate in chat, so the operator can
//! answer it in words where the localhost Lavish page is out of reach.

use archon_core::config::AetherMode;
use archon_core::notify::Question;
use archon_core::queue::QueueItem;
use archon_core::run::{GateSubject, OperatorGate};

use super::clip;
use crate::triggers::TriggerEvent;

const MAX_TITLE_CHARS: usize = 200;
const MAX_SUMMARY_CHARS: usize = 600;
const MAX_PAGE_CHARS: usize = 300;

/// The news that `gate` now waits on `question`, bounded like any event.
pub(super) fn opened(
    mode: AetherMode,
    gate: &OperatorGate,
    item: &QueueItem,
    question: &Question,
) -> TriggerEvent {
    assert_eq!(
        gate.question.as_ref(),
        Some(&question.id),
        "the gate asks this question"
    );
    assert_eq!(item.id, gate.item, "the news names the gate's item");
    assert!(question.number > 0, "a recorded question has its number");
    let asks = match gate.subject {
        GateSubject::Proposal => "Approve the direction proposed for this item?",
        GateSubject::Report => "Approve the change report before the pull request is opened?",
    };
    TriggerEvent::GateOpened {
        question: question.id.clone(),
        number: question.number,
        item: item.id.clone(),
        title: clip(&item.title, MAX_TITLE_CHARS),
        subject: gate.subject,
        asks: asks.to_owned(),
        summary: clip(gate.summary.trim(), MAX_SUMMARY_CHARS),
        page: clip(
            gate.location.as_deref().unwrap_or("not published"),
            MAX_PAGE_CHARS,
        ),
        reply: reply(mode, question.number, gate.subject),
    }
}

/// How the operator answers gate `number` in words: the vocabulary of
/// `GateDecision::from_answer`, on the surface this Aether mode accepts.
fn reply(mode: AetherMode, number: u64, subject: GateSubject) -> String {
    assert!(number > 0, "question numbers start at 1");
    let command = match mode {
        AetherMode::Dedicated => format!("/answer {number}"),
        AetherMode::Shared => format!("archon answer {number}"),
    };
    let revised = match subject {
        GateSubject::Proposal => "have the proposal redone with it",
        GateSubject::Report => "send the work back to implementation with it",
    };
    let surface = match mode {
        AetherMode::Dedicated => {
            "Sent in this chat, the command must be the whole message; `archon answer` takes the same answers."
        }
        AetherMode::Shared => {
            "Run it in a terminal on Archon's machine: chat answers are off in a shared Aether instance."
        }
    };
    let text = format!(
        "`{command} approve` (a note may follow), `{command} reject` to cancel the item, or \
         `{command} <feedback>` to {revised}. A first word of yes, approve, ok or lgtm approves. \
         {surface}"
    );
    assert!(text.contains(&command), "the reply names its command");
    text
}

#[cfg(test)]
mod tests {
    use archon_core::run::GateDecision;

    use super::*;

    /// The answers the reply text spells out, read as an operator sends them.
    fn offered(reply: &str, command: &str) -> Vec<String> {
        reply
            .split('`')
            .filter_map(|quoted| quoted.strip_prefix(command))
            .map(|answer| answer.trim().to_owned())
            .filter(|answer| !answer.is_empty())
            .collect()
    }

    #[test]
    fn the_reply_offers_only_answers_the_gate_understands() {
        let reply = reply(AetherMode::Dedicated, 7, GateSubject::Proposal);
        let answers = offered(&reply, "/answer 7");
        assert_eq!(answers, ["approve", "reject", "<feedback>"]);
        let decide = |answer: &str| GateDecision::from_answer(answer).expect("a decision");
        assert!(matches!(
            decide("approve"),
            GateDecision::Approve { note: None }
        ));
        assert!(matches!(decide("reject"), GateDecision::Reject { .. }));
        assert!(matches!(
            decide("drop the migration"),
            GateDecision::Revise { .. }
        ));
        for word in ["yes", "approve", "ok", "lgtm"] {
            assert!(reply.contains(word), "{word} is named");
            let noted = format!("{word} with a note");
            assert!(matches!(
                decide(&noted),
                GateDecision::Approve { note: Some(_) }
            ));
        }
    }

    #[test]
    fn a_shared_instance_is_pointed_at_the_cli_never_at_chat() {
        let reply = reply(AetherMode::Shared, 3, GateSubject::Report);
        assert_eq!(
            offered(&reply, "archon answer 3"),
            ["approve", "reject", "<feedback>"]
        );
        assert!(!reply.contains("/answer"), "{reply}");
        assert!(reply.contains("chat answers are off"));
        assert!(reply.contains("back to implementation"));
    }
}
