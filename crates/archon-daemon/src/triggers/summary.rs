//! Display labels only; the event bodies and directives remain authoritative.

use archon_core::agent_message::MessageKind;
use serde::{Serialize, Serializer};

use super::{TriggerEvent, TriggerPayload};

const MAX_SUMMARY_CHARS: usize = 160;

impl Serialize for TriggerPayload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            archon: u32,
            events: &'a [TriggerEvent],
            directives: &'a [archon_core::autonomy::Directive],
            summary: String,
        }

        Envelope {
            archon: self.archon,
            events: &self.events,
            directives: &self.directives,
            summary: summary(&self.events),
        }
        .serialize(serializer)
    }
}

fn summary(events: &[TriggerEvent]) -> String {
    let text = match events {
        [] => "Archon update: no events".to_owned(),
        [event] => {
            let label = label(event);
            match event {
                TriggerEvent::ItemFinished { title, .. } => format!("{label} — {title}"),
                TriggerEvent::ItemBlocked { reason, .. } => format!("{label} — {reason}"),
                TriggerEvent::AgentMessage { message } => format!("{label}: {}", message.text),
                _ => label,
            }
        }
        _ => format!(
            "{} events: {}",
            events.len(),
            events.iter().map(label).collect::<Vec<_>>().join(", ")
        ),
    };
    let text: String = text
        .chars()
        .filter(|ch| (!ch.is_control() || ch.is_whitespace()) && !is_format_control(*ch))
        .collect();
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = line.chars();
    let mut result: String = chars.by_ref().take(MAX_SUMMARY_CHARS).collect();
    if chars.next().is_some() {
        result.pop();
        result.push('…');
    }
    result
}

fn label(event: &TriggerEvent) -> String {
    match event {
        TriggerEvent::ItemFinished { item, state, .. } => {
            format!("item finished: {item} {state}")
        }
        TriggerEvent::InitiativeFinished {
            initiative, state, ..
        } => format!("initiative finished: {initiative} {state}"),
        TriggerEvent::ItemBlocked { item, .. } => format!("item blocked: {item}"),
        TriggerEvent::AgentMessage { message } => {
            let kind = match message.kind {
                MessageKind::Relay => "relay",
                MessageKind::Escalation => "escalation",
                MessageKind::Info => "info",
            };
            let source = message.stage.as_deref().unwrap_or("run node");
            format!("stage {kind} from {source} ({})", message.item)
        }
        TriggerEvent::OperatorAnswer { question, .. } => format!("operator answered {question}"),
        TriggerEvent::GateOpened {
            number,
            item,
            subject,
            ..
        } => format!("gate opened: Q{number} {} ({item})", subject.noun()),
        TriggerEvent::ChatAnswer {
            number, applied, ..
        } => {
            let question = number.map_or_else(|| "/answer".to_owned(), |n| format!("Q{n}"));
            let verdict = if *applied { "applied" } else { "refused" };
            format!("chat answer {verdict}: {question}")
        }
        TriggerEvent::LavishFeedback { report, item, .. } => match item {
            Some(item) => format!("report feedback: {report} ({item})"),
            None => format!("report feedback: {report}"),
        },
        TriggerEvent::Budget { scope, .. } => format!("budget limit reached: {scope}"),
        TriggerEvent::PolicyViolation {
            agent,
            model,
            action,
        } => {
            format!("policy violation: {agent} ({model}) — {action}")
        }
        TriggerEvent::NewsDigest { dropped, .. } => {
            let count: u64 = dropped.values().map(|count| u64::from(*count)).sum();
            format!("news digest: {count} older events omitted")
        }
    }
}

// Unicode General_Category=Cf. Keep format controls out of display labels while
// retaining the original event body, including bidi/zero-width text, verbatim.
fn is_format_control(ch: char) -> bool {
    matches!(ch,
        '\u{00ad}' | '\u{0600}'..='\u{0605}' | '\u{061c}' | '\u{06dd}' | '\u{070f}' |
        '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}' |
        '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' |
        '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' |
        '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' |
        '\u{1d173}'..='\u{1d17a}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}
