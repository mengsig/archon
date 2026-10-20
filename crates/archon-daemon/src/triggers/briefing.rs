//! Bounded continuity from durable state, frozen before the first delivery.

use archon_aether::AetherClient;
use archon_core::aether_ids::AetherAgentId;
use archon_core::attachment::with_image_labels;
use archon_core::error::ArchonError;
use archon_core::notify::FeedEntry;
use archon_core::protocol::ItemQuery;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::RunState;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

use super::{PAYLOAD_VERSION, TriggerEvent, TriggerPayload};
use crate::context::DaemonContext;

const FROZEN_KEY: &str = "orchestrator_briefing";
const MAX_BYTES: usize = 4096;
const CHAT_LINES: usize = 8;

#[derive(Serialize, Deserialize)]
struct Frozen {
    generation: u64,
    payload: TriggerPayload,
}

/// Called under the supervisor lifecycle lock, before the new agent is adopted.
pub(crate) async fn deliver(
    ctx: &DaemonContext,
    client: &AetherClient,
    agent: &AetherAgentId,
    generation: u64,
) -> Result<(), ArchonError> {
    assert!(
        generation > 0,
        "only a created generation receives a briefing"
    );
    let payload = {
        let mut store = ctx.store.lock().await;
        frozen(&mut store, generation)?
    };
    assert!(matches!(
        payload.events.as_slice(),
        [TriggerEvent::OrchestratorBriefing { .. }]
    ));
    let key = format!("archon-briefing-{agent}-{generation}");
    match client
        .trigger(agent, serde_json::to_value(payload)?, &key)
        .await
    {
        Err(ArchonError::Invalid(reason)) => Err(ArchonError::Unavailable(format!(
            "briefing for {agent} was refused: {reason}"
        ))),
        result => result,
    }
}

fn frozen(store: &mut Store, generation: u64) -> Result<TriggerPayload, ArchonError> {
    assert!(generation > 0, "briefings belong to an existing generation");
    if let Some(raw) = store.kv_get(FROZEN_KEY)? {
        let saved: Frozen = serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Store(format!("kv {FROZEN_KEY}: {e}")))?;
        if saved.generation == generation {
            return Ok(saved.payload);
        }
        if saved.generation > generation {
            return Err(ArchonError::Store(format!(
                "kv {FROZEN_KEY} belongs to generation {}, ahead of current generation {generation}",
                saved.generation
            )));
        }
    }
    let directives = store.directives(archon_core::now())?;
    let payload = TriggerPayload {
        archon: PAYLOAD_VERSION,
        events: vec![build(store)?],
        directives_in_force: u32::try_from(directives.len()).expect("directives are bounded"),
        directives: Some(directives),
        directive_changes: None,
    };
    let saved = Frozen {
        generation,
        payload,
    };
    store.kv_set(FROZEN_KEY, &serde_json::to_string(&saved)?)?;
    Ok(saved.payload)
}

fn build(store: &Store) -> Result<TriggerEvent, ArchonError> {
    let (active, finished) = queue_lines(store)?;
    let mut text = String::new();
    section(&mut text, "Non-terminal items", &active, 1600)?;
    section(&mut text, "Finished since last batch", &finished, 500)?;
    section(
        &mut text,
        "Open questions and gates",
        &question_lines(store)?,
        800,
    )?;
    let mut initiatives = store.initiatives()?;
    initiatives.sort_by(|a, b| a.id.cmp(&b.id));
    let initiatives: Vec<_> = initiatives
        .into_iter()
        .filter(|i| !i.state.is_terminal())
        .map(|i| format!("{} {} {}", i.id, i.state.as_str(), clip(&i.title, 120)))
        .collect();
    section(&mut text, "Open initiatives", &initiatives, 350)?;
    section(
        &mut text,
        "Recent chat (newest first)",
        &chat_lines(store)?,
        600,
    )?;
    let event = TriggerEvent::OrchestratorBriefing { text };
    assert!(
        serde_json::to_vec(&event)?.len() <= MAX_BYTES,
        "briefing event is bounded on the wire"
    );
    Ok(event)
}

fn queue_lines(store: &Store) -> Result<(Vec<String>, Vec<String>), ArchonError> {
    let mut items = store.items(&ItemQuery {
        limit: Some(i64::MAX as usize),
        ..Default::default()
    })?;
    items.sort_by(|a, b| (!a.state.is_active(), &a.id).cmp(&(!b.state.is_active(), &b.id)));
    let since = super::read_outbox(store)?.last_batch_at;
    let mut active = Vec::new();
    let mut finished = Vec::new();
    for item in items {
        if !item.state.is_terminal() {
            active.push(item_line(store, &item)?);
        } else if since.is_none_or(|at| item.updated_at >= at) {
            let line = format!(
                "{} {} {}",
                item.id,
                item.state.kind(),
                clip(&item.title, 100)
            );
            finished.push((std::cmp::Reverse(item.updated_at), item.id, line));
        }
    }
    finished.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    Ok((
        active,
        finished.into_iter().map(|(_, _, line)| line).collect(),
    ))
}

fn item_line(store: &Store, item: &QueueItem) -> Result<String, ArchonError> {
    assert!(
        !item.state.is_terminal(),
        "only unfinished work needs a stage and wait reason"
    );
    let mut details = Vec::new();
    if let QueueState::Running { run } = &item.state {
        let run = store.run(run)?;
        assert_eq!(run.item, item.id, "the active run belongs to this item");
        match run.state {
            RunState::Running { stage_id, .. } => details.push(format!("stage={stage_id}")),
            RunState::AwaitingOperator { stage_id, .. } => {
                details.push(format!("stage={stage_id} waits=operator"))
            }
            _ => details.push("stage=settling".into()),
        }
        let stages = store.stage_runs(&run.id)?;
        if let Some(url) = archon_core::run::pr_url(stages.iter()) {
            details.push(format!("PR={url}"));
        }
    }
    if let QueueState::Blocked { reason } = &item.state {
        details.push(format!("waits={}", clip(reason, 120)));
    }
    for dep in &item.dependencies {
        if !matches!(store.item(dep)?.state, QueueState::Succeeded { .. }) {
            details.push(format!("depends={dep}"));
        }
    }
    if matches!(item.state, QueueState::Queued) && details.is_empty() {
        details.push("waits=dispatch capacity/pause".into());
    }
    if let Some(onto) = &item.onto {
        details.push(format!("onto={}", onto.pr.url()));
    }
    Ok(format!(
        "{} {} {} | {}",
        item.id,
        item.state.kind(),
        details.join(" "),
        clip(&item.title, 100)
    ))
}

fn question_lines(store: &Store) -> Result<Vec<String>, ArchonError> {
    let mut questions = store.questions(true)?;
    questions.sort_by_key(|q| q.number);
    let mut gates = store.open_gates()?;
    gates.sort_by(|a, b| (&a.item, &a.stage_id, a.attempt).cmp(&(&b.item, &b.stage_id, b.attempt)));
    let by_question: std::collections::BTreeMap<_, _> = gates
        .iter()
        .filter_map(|gate| gate.question.as_ref().map(|id| (id, gate)))
        .collect();
    let mut lines = Vec::new();
    for q in questions {
        assert!(
            q.is_open() && q.number > 0,
            "stored open question has a number"
        );
        let gate = by_question.get(&q.id).copied();
        let item = q.item.as_ref().or_else(|| gate.map(|g| &g.item));
        let label = gate.map_or_else(String::new, |g| {
            format!("gate={} stage={} ", g.subject.noun(), g.stage_id)
        });
        lines.push(format!(
            "Q{} {} {}{}",
            q.number,
            item.map_or("global", |id| id.as_str()),
            label,
            clip(&q.text, 160)
        ));
    }
    for gate in gates.iter().filter(|g| g.question.is_none()) {
        assert!(gate.is_open(), "only open gates are listed");
        lines.push(format!(
            "{} gate={} stage={} question=pending",
            gate.item,
            gate.subject.noun(),
            gate.stage_id
        ));
    }
    Ok(lines)
}

fn chat_lines(store: &Store) -> Result<Vec<String>, ArchonError> {
    let entries = store.feed_before(None, CHAT_LINES, false)?;
    assert!(entries.len() <= CHAT_LINES, "feed query respects its limit");
    let mut lines = Vec::new();
    for entry in entries.into_iter().rev() {
        let text = match entry.entry {
            FeedEntry::Operator { text, images } => {
                let said = with_image_labels(&text, &images).replace('\n', " ");
                format!("operator: {said}")
            }
            FeedEntry::Orchestrator { text } => format!("orchestrator: {text}"),
            FeedEntry::Notification(n) => format!("{:?}: {}", n.kind, n.text),
            FeedEntry::Question(q) => format!("Q{}: {}", q.number, q.text),
            FeedEntry::Answer { text, .. } => format!("answer: {text}"),
            FeedEntry::Report {
                title, location, ..
            } => format!("report: {title} {location}"),
            FeedEntry::System { text } => format!("system: {text}"),
            FeedEntry::Authority { action, result, .. } => {
                format!("operator action: {action} {result}")
            }
        };
        lines.push(clip(&text, 200));
    }
    assert!(lines.len() <= CHAT_LINES, "one line per chat entry");
    Ok(lines)
}

fn clip(text: &str, bytes: usize) -> String {
    assert!(bytes >= 3, "space for an ellipsis");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() <= bytes {
        return text;
    }
    let end = text.floor_char_boundary(bytes - 3);
    assert!(
        end < text.len(),
        "clipping shortens text at a character boundary"
    );
    format!("{}…", &text[..end])
}

fn section(
    text: &mut String,
    title: &str,
    rows: &[String],
    budget: usize,
) -> Result<(), ArchonError> {
    let mut part = format!("{title}:\n");
    let mut kept = 0;
    for row in rows {
        let line = format!("{}\n", clip(row, 400));
        if serde_json::to_string(&part)?.len() + serde_json::to_string(&line)?.len() + 40 > budget {
            break;
        }
        part.push_str(&line);
        kept += 1;
    }
    if kept < rows.len() {
        part.push_str(&format!("+{} more\n", rows.len() - kept));
    } else if rows.is_empty() {
        part.push_str("none\n");
    }
    assert!(kept <= rows.len(), "each row is kept or counted");
    assert!(
        serde_json::to_string(&part)?.len() <= budget,
        "section fits its wire budget"
    );
    text.push_str(&part);
    Ok(())
}

#[cfg(test)]
#[path = "briefing_tests.rs"]
mod tests;
