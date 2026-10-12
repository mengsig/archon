//! Archon news for the orchestrator, delivered as Aether `trigger`s with
//! `source = "archon"` and coalesced (ARCHITECTURE §5.4).
//! Other components enqueue events through [`push`]; a long or stale
//! backlog is compacted as it is keyed (see `compact`).

mod compact;
mod summary;

use std::collections::BTreeMap;
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::Directive;
use archon_core::error::ArchonError;
use archon_core::github::GithubThread;
use archon_core::id::{QuestionId, QueueItemId, ReportId, StoreId};
use archon_core::run::GateSubject;
use archon_queue::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

pub use self::compact::MAX_PENDING;
use self::compact::{Facts, Held};
use crate::context::DaemonContext;
use crate::{feed, supervisor};

pub const TRIGGER_SOURCE: &str = "archon";
pub const PAYLOAD_VERSION: u32 = 1;

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Consecutive failures of one batch before the operator hears about it.
const QUIET_FAILURES: u32 = 5;
/// `kv`: how many trigger events have been given a key so far.
const TRIGGER_SEQ_KEY: &str = "trigger_seq";
/// `kv`: the [`Outbox`], so an `archond` restart neither loses held news
/// nor re-keys a batch Aether may already have.
const OUTBOX_KEY: &str = "trigger_outbox";
/// Most bytes of `lavish-axi poll` output a `lavish_feedback` event carries
/// once its DOM snapshot is dropped. The prompts table precedes `next_step`,
/// so a cut takes lavish-axi's instructions before any prompt row.
pub const FEEDBACK_EXCERPT_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerEvent {
    AgentMessage {
        #[serde(flatten)]
        message: Box<archon_core::agent_message::AgentMessage>,
    },
    ItemFinished {
        item: QueueItemId,
        title: String,
        /// `QueueState::kind()`.
        state: String,
        summary: String,
        pr_url: Option<String>,
        cost_usd: f64,
        /// The GitHub thread that asked for the item; the orchestrator
        /// reports the outcome there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<GithubThread>,
    },
    ItemBlocked {
        item: QueueItemId,
        reason: String,
    },
    OperatorAnswer {
        question: QuestionId,
        text: String,
    },
    /// A proposal or report gate opened and waits for the operator.
    GateOpened {
        question: QuestionId,
        /// The operator's `Q<n>` for the gate's question.
        number: u64,
        item: QueueItemId,
        title: String,
        subject: GateSubject,
        /// What the operator is asked to decide.
        asks: String,
        /// The gate stage's summary, clipped.
        summary: String,
        /// Where the page is: a Lavish URL on archond's machine, or a path.
        page: String,
        /// How the operator answers in words, on a surface that takes it.
        reply: String,
    },
    /// What the daemon did with an operator `/answer` chat command.
    ChatAnswer {
        /// The question's `Q<n>`, when the command named a recorded one.
        number: Option<u64>,
        applied: bool,
        /// The answer as recorded, or why nothing was; clipped.
        text: String,
    },
    LavishFeedback {
        report: ReportId,
        item: Option<QueueItemId>,
        /// The `lavish-axi poll` output without its DOM snapshot, bounded by
        /// [`FEEDBACK_EXCERPT_BYTES`], with a note of what was left out.
        text: String,
        /// Best-effort parse of the operator's prompts out of the whole output.
        #[serde(default)]
        prompts: Vec<String>,
    },
    Budget {
        scope: String,
        spent_usd: f64,
        limit_usd: f64,
    },
    PolicyViolation {
        agent: String,
        model: String,
        action: String,
    },
    /// Held news left out of this trigger when it was compacted.
    NewsDigest {
        /// How many events were left out, by `type`.
        dropped: BTreeMap<String, u32>,
        /// Where the orchestrator finds the detail.
        note: String,
    },
}

impl TriggerEvent {
    /// Feedback on `report`, carrying the poll output `raw` without the
    /// page's DOM snapshot (the page itself is the report's file).
    pub fn lavish_feedback(
        report: ReportId,
        item: Option<QueueItemId>,
        raw: &str,
        prompts: Vec<String>,
    ) -> Self {
        Self::LavishFeedback {
            report,
            item,
            text: excerpt(&without_dom_snapshot(raw), FEEDBACK_EXCERPT_BYTES),
            prompts,
        }
    }

    /// Its `type` in the payload.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AgentMessage { .. } => "agent_message",
            Self::ItemFinished { .. } => "item_finished",
            Self::ItemBlocked { .. } => "item_blocked",
            Self::OperatorAnswer { .. } => "operator_answer",
            Self::GateOpened { .. } => "gate_opened",
            Self::ChatAnswer { .. } => "chat_answer",
            Self::LavishFeedback { .. } => "lavish_feedback",
            Self::Budget { .. } => "budget",
            Self::PolicyViolation { .. } => "policy_violation",
            Self::NewsDigest { .. } => "news_digest",
        }
    }
}

/// `raw` without its top-level `dom_snapshot` field and the lines nested
/// under it, followed by a note of its size when there was one.
fn without_dom_snapshot(raw: &str) -> String {
    let mut kept = String::with_capacity(raw.len());
    let (mut snapshot, mut in_snapshot) = (0, false);
    for line in raw.split_inclusive('\n') {
        if !line.starts_with([' ', '\t']) {
            in_snapshot = is_dom_snapshot_field(line);
        }
        if in_snapshot {
            snapshot += line.len();
        } else {
            kept.push_str(line);
        }
    }
    assert_eq!(
        kept.len() + snapshot,
        raw.len(),
        "every byte is kept or cut"
    );
    if snapshot == 0 {
        return kept;
    }
    if !kept.is_empty() && !kept.ends_with('\n') {
        kept.push('\n');
    }
    kept.push_str(&format!(
        "[dom_snapshot left out: {snapshot} bytes; the page is the report file]"
    ));
    assert!(kept.len() < raw.len() + 80, "only a short note is added");
    kept
}

/// Whether `line` opens the top-level `dom_snapshot` field, in any TOON form.
fn is_dom_snapshot_field(line: &str) -> bool {
    line.strip_prefix("dom_snapshot")
        .is_some_and(|rest| rest.starts_with([':', '[', '{']))
}

/// `text` cut to at most `limit` bytes on a character boundary, followed by
/// a note of how many bytes were left out.
fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(limit);
    assert!(
        end <= limit && end < text.len(),
        "the cut shortens the text"
    );
    let cut = text.len() - end;
    format!(
        "{}\n[… {cut} more bytes of poll output left out]",
        &text[..end]
    )
}

/// Every trigger restates the operator directives in force (ARCHITECTURE
/// §18.5). Serialization adds a one-line `summary` for the TUI, derived
/// from `events` so restored batches and callers cannot carry stale labels.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TriggerPayload {
    pub archon: u32,
    pub events: Vec<TriggerEvent>,
    #[serde(default)]
    pub directives: Vec<Directive>,
}

/// Every event pushed and not yet delivered. Each change is a single
/// read-modify-write under the store lock, so none is lost or re-keyed.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Outbox {
    in_flight: Option<Batch>,
    /// Pushed since `in_flight` was keyed, oldest first.
    pending: Vec<Held>,
    /// A fresh orchestrator generation was created while `pending` news
    /// waited, so the next batch is compacted whatever its size.
    #[serde(default)]
    collapse: bool,
}

/// One trigger under the idempotency key it keeps across retries, with the
/// directives in force when it was keyed, so a retry sends the same payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Batch {
    key: String,
    events: Vec<TriggerEvent>,
    #[serde(default)]
    directives: Vec<Directive>,
}

/// Wakes the delivery task; the news itself waits in the outbox.
#[derive(Debug, Default)]
pub struct TriggerQueue {
    news: Notify,
}

/// Save `event` for the orchestrator, then wake delivery. Once this returns
/// the event survives a daemon stop or crash and is delivered after it. News
/// held past [`MAX_PENDING`] is compacted here, not only when keyed.
pub async fn push(ctx: &DaemonContext, event: TriggerEvent) -> Result<(), ArchonError> {
    hold(&mut *ctx.store.lock().await, event)?;
    // Stores a permit when delivery is busy, so no wake-up is lost.
    ctx.triggers.news.notify_one();
    Ok(())
}

/// Save `event` under the caller's store lock, beside the change it reports;
/// the caller then calls [`wake`].
pub(crate) fn hold(store: &mut Store, event: TriggerEvent) -> Result<(), ArchonError> {
    assert!(
        !matches!(event, TriggerEvent::NewsDigest { .. }),
        "a digest is made when news is keyed, never pushed"
    );
    let mut outbox = read_outbox(store)?;
    let before = outbox.pending.len();
    outbox.pending.push(Held {
        event,
        pushed_at: Some(archon_core::now()),
    });
    if outbox.pending.len() > MAX_PENDING {
        compact_pending(store, &mut outbox)?;
    }
    assert!(
        outbox.pending.len() <= before + 1,
        "holding one event adds at most one"
    );
    write_outbox(store, &outbox)
}

pub(crate) fn record_message(
    store: &mut Store,
    message: archon_core::agent_message::AgentMessage,
    updates: &[(&str, &str)],
) -> Result<archon_core::notify::FeedItem, ArchonError> {
    use archon_core::notify::{FeedEntry, Notification, NotificationKind};
    let at = archon_core::now();
    let entry = FeedEntry::Notification(Notification {
        id: archon_core::id::NotificationId::new(),
        kind: NotificationKind::Progress,
        text: serde_json::to_string(&message)?,
        item: Some(message.item.clone()),
        run: Some(message.run.clone()),
        at,
    });
    let mut outbox = read_outbox(store)?;
    outbox.pending.push(Held {
        event: TriggerEvent::AgentMessage {
            message: Box::new(message),
        },
        pushed_at: Some(at),
    });
    if outbox.pending.len() > MAX_PENDING {
        compact_pending(store, &mut outbox)?;
    }
    let raw = serde_json::to_string(&outbox)?;
    let mut updates = updates.to_vec();
    updates.push((OUTBOX_KEY, &raw));
    store.append_feed_with_kv(entry, at, &updates)
}

pub(crate) fn wake(ctx: &DaemonContext) {
    ctx.triggers.news.notify_one();
}

/// Compact the news held behind a stuck batch, as keying it would; what is
/// dropped is counted in a digest that a later compaction folds in.
fn compact_pending(store: &mut Store, outbox: &mut Outbox) -> Result<(), ArchonError> {
    let held = std::mem::take(&mut outbox.pending);
    let before = held.len();
    assert!(before > 0, "only held news is compacted");
    let facts = Facts::gather(store, &held)?;
    outbox.pending = compact::compact(held, &facts, archon_core::now());
    let after = outbox.pending.len();
    assert!(
        after > 0 && after <= before,
        "compaction keeps at least a digest and never grows the news"
    );
    tracing::info!(before, after, "compacted news held behind a batch");
    Ok(())
}

/// Deliver held news as triggers until the daemon stops; only a store
/// failure ends it. News pushed within `coalesce` of the first event after a
/// quiet spell shares one trigger. News pushed while a batch is delivered
/// has waited already and goes out right after it. A batch is keyed once and
/// retried under that key, across `archond` restarts too.
pub async fn deliver_loop(ctx: std::sync::Arc<DaemonContext>) -> Result<(), ArchonError> {
    let coalesce = Duration::from_millis(ctx.config.orchestrator.trigger_coalesce_ms);
    loop {
        let in_flight = read_outbox(&*ctx.store.lock().await)?.in_flight;
        match in_flight {
            Some(batch) => {
                deliver(&ctx, &batch).await?;
                forget_delivered(&ctx, &batch.key).await?;
            }
            None => wait_for_news(&ctx, coalesce).await?,
        }
        key_pending(&ctx).await?;
    }
}

/// Wait for news when none is held, then let more gather for `coalesce`.
/// The window is not extended by later events, so a steady stream still
/// yields a trigger per window.
async fn wait_for_news(ctx: &DaemonContext, coalesce: Duration) -> Result<(), ArchonError> {
    let held = read_outbox(&*ctx.store.lock().await)?;
    assert!(
        held.in_flight.is_none(),
        "news waits only when nothing is in flight"
    );
    if held.pending.is_empty() {
        ctx.triggers.news.notified().await;
    }
    tokio::time::sleep(coalesce).await;
    Ok(())
}

/// Move every pending event into one keyed batch, compacted when it is
/// due; a no-op when none waits. A batch already keyed is never touched.
async fn key_pending(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let store_id = supervisor::store_id(ctx).await?;
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    assert!(
        outbox.in_flight.is_none(),
        "one batch is in flight at a time"
    );
    if outbox.pending.is_empty() {
        return Ok(());
    }
    let held = std::mem::take(&mut outbox.pending);
    let collapse = std::mem::take(&mut outbox.collapse);
    let events = batch_events(&mut store, held, collapse)?;
    let key = batch_key(&mut store, &store_id, events.len())?;
    let directives = store.directives(archon_core::now())?;
    outbox.in_flight = Some(Batch {
        key,
        events,
        directives,
    });
    write_outbox(&mut store, &outbox)
}

/// `held` as a batch: compacted when a fresh orchestrator inherits it
/// (`collapse`) or it is too long or too old to replay in full.
fn batch_events(
    store: &mut Store,
    held: Vec<Held>,
    collapse: bool,
) -> Result<Vec<TriggerEvent>, ArchonError> {
    assert!(!held.is_empty(), "only held news makes a batch");
    let now = archon_core::now();
    if !collapse && !compact::needs_compaction(&held, now) {
        return Ok(held.into_iter().map(|news| news.event).collect());
    }
    let facts = Facts::gather(store, &held)?;
    let before = held.len();
    let events: Vec<TriggerEvent> = compact::compact(held, &facts, now)
        .into_iter()
        .map(|news| news.event)
        .collect();
    assert!(!events.is_empty(), "compaction keeps at least a digest");
    tracing::info!(
        before,
        after = events.len(),
        collapse,
        "compacted held news"
    );
    Ok(events)
}

/// A fresh orchestrator generation has none of its predecessor's context:
/// compact the news it inherits, pending now, when it is next keyed,
/// whatever its size. A batch already keyed goes out as it is, and news
/// pushed after this is news to the new generation, compacted only as usual.
pub async fn collapse_for_new_orchestrator(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    if outbox.pending.is_empty() {
        return Ok(());
    }
    outbox.collapse = true;
    write_outbox(&mut store, &outbox)
}

/// Drop the batch the orchestrator now has; news pushed meanwhile stays.
async fn forget_delivered(ctx: &DaemonContext, key: &str) -> Result<(), ArchonError> {
    let mut store = ctx.store.lock().await;
    let mut outbox = read_outbox(&store)?;
    let delivered = outbox.in_flight.take();
    assert!(
        delivered.is_some_and(|batch| batch.key == key),
        "only the batch in flight is delivered"
    );
    write_outbox(&mut store, &outbox)
}

fn read_outbox(store: &Store) -> Result<Outbox, ArchonError> {
    let Some(raw) = store.kv_get(OUTBOX_KEY)? else {
        return Ok(Outbox::default());
    };
    let outbox: Outbox = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Store(format!("kv {OUTBOX_KEY} = {raw:?}: {e}")))?;
    let keyed = outbox.in_flight.as_ref();
    assert!(
        keyed.is_none_or(|batch| !batch.events.is_empty() && !batch.key.is_empty()),
        "only a keyed, non-empty batch is ever in flight"
    );
    Ok(outbox)
}

fn write_outbox(store: &mut Store, outbox: &Outbox) -> Result<(), ArchonError> {
    let raw = serde_json::to_string(outbox)?;
    store.kv_set(OUTBOX_KEY, &raw)
}

/// News not yet delivered to the orchestrator, oldest first: the batch in
/// flight, then the events pushed since it was keyed.
pub async fn held(ctx: &DaemonContext) -> Result<Vec<TriggerEvent>, ArchonError> {
    let outbox = read_outbox(&*ctx.store.lock().await)?;
    let mut events = outbox.in_flight.map_or_else(Vec::new, |batch| batch.events);
    events.extend(outbox.pending.into_iter().map(|news| news.event));
    Ok(events)
}

/// How many news events wait for the orchestrator (see [`held`]).
pub async fn held_news(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    Ok(held(ctx).await?.len())
}

/// `trg_<store>_<first>_<last>` over a persisted count of trigger events:
/// unique across daemon restarts and across stores. Trigger events are not
/// feed items, hence not the feed seq ARCHITECTURE §5.4 names. The count is
/// saved before the batch, so a crash between them only skips numbers.
fn batch_key(store: &mut Store, store_id: &StoreId, len: usize) -> Result<String, ArchonError> {
    assert!(len > 0, "an empty batch is never delivered");
    let delivered: u64 = match store.kv_get(TRIGGER_SEQ_KEY)? {
        None => 0,
        Some(raw) => raw
            .parse()
            .map_err(|e| ArchonError::Store(format!("kv {TRIGGER_SEQ_KEY} = {raw:?}: {e}")))?,
    };
    let last = delivered + len as u64;
    store.kv_set(TRIGGER_SEQ_KEY, &last.to_string())?;
    Ok(format!("trg_{store_id}_{}_{last}", delivered + 1))
}

/// Retry `batch` under its fixed key until the orchestrator has it, at once
/// when the supervisor names a new or revived orchestrator. Only a store
/// failure (fatal) or a refusal of the payload itself ends retries.
async fn deliver(ctx: &DaemonContext, batch: &Batch) -> Result<(), ArchonError> {
    assert!(
        !batch.events.is_empty(),
        "an empty batch is never delivered"
    );
    let key = batch.key.as_str();
    let payload = serde_json::to_value(TriggerPayload {
        archon: PAYLOAD_VERSION,
        events: batch.events.clone(),
        directives: batch.directives.clone(),
    })?;
    let payload = &payload;
    let mut orchestrator = ctx.supervisor.watch();
    let mut backoff = RETRY_MIN;
    for failures in 1u32.. {
        let error = match attempt(ctx, key, payload).await {
            Ok(()) => return Ok(()),
            Err(error) if error.is_store() => return Err(error),
            Err(ArchonError::Invalid(refusal)) => {
                let text = format!(
                    "Aether refused trigger {key} ({refusal}); its news was not delivered: {payload}"
                );
                tracing::error!("{text}");
                feed::system(ctx, text).await?;
                return Ok(());
            }
            Err(error) => error,
        };
        tracing::warn!(key, failures, %error, "trigger delivery failed; retrying");
        if failures == QUIET_FAILURES {
            let text = format!(
                "Archon cannot reach the orchestrator ({error}); its news is held and retried."
            );
            feed::system(ctx, text).await?;
        }
        // Only a change after this attempt is news.
        orchestrator.borrow_and_update();
        wait_to_retry(&mut orchestrator, Instant::now() + backoff).await;
        backoff = (backoff * 2).min(RETRY_MAX);
    }
    unreachable!("the retry loop only ends by returning")
}

/// Sleep out a retry backoff, cut short when the orchestrator changes.
/// News pushed meanwhile is already saved as pending.
async fn wait_to_retry(orchestrator: &mut watch::Receiver<Option<AetherAgentId>>, wake: Instant) {
    tokio::select! {
        () = tokio::time::sleep_until(wake) => {}
        // The sender lives in `ctx`, so `changed` cannot fail here.
        _ = orchestrator.changed() => {}
    }
}

/// One delivery. Aether also refuses a trigger to an agent that stopped
/// since `ensure_orchestrator`; that is retried, not a refused payload.
async fn attempt(
    ctx: &DaemonContext,
    key: &str,
    payload: &serde_json::Value,
) -> Result<(), ArchonError> {
    let orchestrator = supervisor::ensure_orchestrator(ctx).await?;
    let client = ctx.aether.client()?;
    let refusal = match client.trigger(&orchestrator, payload.clone(), key).await {
        Err(ArchonError::Invalid(refusal)) => refusal,
        sent => return sent,
    };
    if supervisor::is_live(&client, &orchestrator).await? {
        return Err(ArchonError::Invalid(refusal));
    }
    Err(ArchonError::Unavailable(format!(
        "orchestrator {orchestrator} stopped before trigger {key} reached it ({refusal})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_matches_the_documented_shape() {
        let directive = archon_core::autonomy::Directive::new(
            "merge docs fixes yourself",
            archon_core::autonomy::DirectiveSource::Operator,
            None,
            archon_core::now(),
        )
        .unwrap();
        let payload = TriggerPayload {
            archon: PAYLOAD_VERSION,
            events: vec![TriggerEvent::ItemBlocked {
                item: QueueItemId::new(),
                reason: "dependency failed".into(),
            }],
            directives: vec![directive],
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["archon"], 1);
        assert_eq!(value["events"][0]["type"], "item_blocked");
        assert_eq!(value["directives"][0]["text"], "merge docs fixes yourself");
        assert_eq!(value["directives"][0]["source"], "operator");
    }

    #[test]
    fn every_kind_names_its_payload_type() {
        let events = [
            TriggerEvent::ItemBlocked {
                item: QueueItemId::new(),
                reason: "r".into(),
            },
            TriggerEvent::OperatorAnswer {
                question: QuestionId::new(),
                text: "t".into(),
            },
            TriggerEvent::GateOpened {
                question: QuestionId::new(),
                number: 4,
                item: QueueItemId::new(),
                title: "t".into(),
                subject: GateSubject::Proposal,
                asks: "a".into(),
                summary: "s".into(),
                page: "p".into(),
                reply: "r".into(),
            },
            TriggerEvent::ChatAnswer {
                number: Some(4),
                applied: true,
                text: "yes".into(),
            },
            TriggerEvent::lavish_feedback(ReportId::new(), None, "raw", vec![]),
            TriggerEvent::NewsDigest {
                dropped: BTreeMap::from([("budget".to_owned(), 2)]),
                note: "n".into(),
            },
        ];
        for event in events {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["type"], event.kind());
        }
    }

    fn feedback_text(raw: &str, prompts: &[&str]) -> String {
        let prompts = prompts.iter().map(|p| (*p).to_owned()).collect();
        match TriggerEvent::lavish_feedback(ReportId::new(), None, raw, prompts) {
            TriggerEvent::LavishFeedback { text, .. } => text,
            _ => unreachable!("lavish_feedback builds feedback"),
        }
    }

    #[test]
    fn feedback_text_is_cut_on_a_character_boundary() {
        let short = "session:\n  status: feedback\n";
        assert_eq!(excerpt(short, FEEDBACK_EXCERPT_BYTES), short);
        // `é` is two bytes: a cut at 3 would split the second one.
        let text = excerpt("éééé", 3);
        assert!(text.starts_with("é\n"), "{text}");
        assert!(text.contains("6 more bytes"), "{text}");
    }

    #[test]
    fn even_a_short_dom_snapshot_is_left_out() {
        let raw = concat!(
            "session:\n  status: feedback\n",
            "prompts[1]{uid,prompt}:\n  u1,Blue\n",
            "next_step: Apply\n",
            "dom_snapshot: \"<p>page</p>\"\n",
        );
        let text = feedback_text(raw, &["Blue"]);
        assert!(!text.contains("<p>page</p>"), "{text}");
        assert!(
            text.starts_with("session:\n  status: feedback\nprompts[1]"),
            "{text}"
        );
        assert!(text.contains("next_step: Apply\n"), "{text}");
        assert!(
            text.ends_with("[dom_snapshot left out: 28 bytes; the page is the report file]"),
            "{text}"
        );
    }

    #[test]
    fn only_the_snapshot_field_and_its_nested_lines_are_cut() {
        let raw = "session:\n  status: feedback\ndom_snapshot[2]:\n  - a\n  - b\nnext_step: Apply";
        let text = feedback_text(raw, &["x"]);
        assert!(text.starts_with("session:\n  status: feedback\nnext_step: Apply\n"));
        assert!(!text.contains("- a"), "{text}");
        let unchanged = "session:\n  status: feedback\nnext_step: \"dom_snapshot: kept\"\n";
        assert_eq!(feedback_text(unchanged, &["x"]), unchanged);
    }

    #[test]
    fn unparsed_feedback_keeps_its_detail_whole() {
        let detail = "d".repeat(3000);
        let raw = format!(
            "session:\n  status: feedback\nartifact_failures[1]:\n  - {detail}\ndom_snapshot: \"{}\"\n",
            "ü".repeat(8192)
        );
        let text = feedback_text(&raw, &[]);
        assert!(text.contains(&detail), "the failure detail is whole");
        assert!(!text.contains('ü'), "the snapshot is not sent");
        assert!(!text.contains("more bytes of poll output"), "{text}");
    }

    #[test]
    fn feedback_without_a_snapshot_is_still_bounded() {
        let raw = format!("session:\n  status: feedback\nnote: {}", "é".repeat(8192));
        for prompts in [&[][..], &["Blue"][..]] {
            let text = feedback_text(&raw, prompts);
            assert!(
                text.len() < FEEDBACK_EXCERPT_BYTES + 64,
                "{} bytes",
                text.len()
            );
            assert!(text.starts_with("session:\n"), "{text}");
            assert!(text.contains("more bytes of poll output left out"));
        }
    }
}
