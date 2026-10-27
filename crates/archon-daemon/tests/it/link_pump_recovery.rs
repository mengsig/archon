//! The event pump recovering on its own: from Aether history that retention
//! deleted (with and without the `history_expired` code), from a cursor it
//! cannot read, and from an event whose handling failed for a passing reason.

use crate::link_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use archon_aether::fake::{self, Scripted};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::model::ModelRef;
use archon_queue::store::keys;
use link_common::{Harness, spawn_link};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const ORCH: &str = "agt_0001";
const CHILD: &str = "agt_0101";
const DENIED: (&str, &str) = ("claude-code", "claude-haiku-4-5");
const EXPIRED_NOTE: &str = "deleted old event history";

/// A fleet event Archon ignores, so handling it only moves the cursor.
fn stranger_usage() -> Value {
    let usage = json!({"type": "agent.usage_updated", "total": {"cost_usd": 1.0}});
    fake::event(Some("agt_0999"), None, usage)
}

/// Six fleet events, of which 2..=4 expired; the fleet stream last handled
/// event 1 and the orchestrator stream is past the gap.
fn history_with_a_gap(harness: &Harness, coded: bool) {
    if coded {
        harness.fake.set_features(vec!["history_expired".into()]);
    }
    for _ in 0..6 {
        harness.fake.push_event(stranger_usage());
    }
    harness.fake.expire_history(2, 4);
}

/// The link following a live orchestrator from the stored cursors.
async fn pump_from(harness: &Harness, fleet: &str) -> JoinHandle<Result<(), ArchonError>> {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::CURSOR_FLEET, fleet).await;
    harness.set_kv(keys::CURSOR_ORCHESTRATOR, "6").await;
    let mut orchestrator = fake::agent(ORCH, None);
    orchestrator["status"] = json!({"state": "running"});
    harness
        .fake
        .set_default("get_agent", Scripted::reply("agent", orchestrator));
    spawn_link(harness, harness.fake.endpoint(), None)
}

async fn cursor_reaches(harness: &Harness, key: &str, seq: u64) {
    harness
        .eventually(&format!("cursor {key} reaches {seq}"), || async {
            let saved = harness.kv(key).await;
            saved.and_then(|s| s.parse::<u64>().ok()) == Some(seq)
        })
        .await;
}

async fn expired_notes(harness: &Harness) -> Vec<String> {
    let notes = harness.system_notes().await;
    notes
        .into_iter()
        .filter(|n| n.contains(EXPIRED_NOTE))
        .collect()
}

/// The fleet skips the expired span on the connection that found it, says
/// so once, and handles what comes after it.
async fn skips_the_gap_once(coded: bool) {
    let harness = Harness::new().await;
    history_with_a_gap(&harness, coded);
    let pump = pump_from(&harness, "1").await;
    cursor_reaches(&harness, keys::CURSOR_FLEET, 6).await;
    let seq = harness.fake.push_event(stranger_usage());
    cursor_reaches(&harness, keys::CURSOR_FLEET, seq).await;
    pump.abort();

    let notes = expired_notes(&harness).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("events 2..=4"), "{}", notes[0]);
    assert!(notes[0].contains("fleet"), "{}", notes[0]);
    assert_eq!(
        harness.fake.connections_accepted(),
        1,
        "no reconnect needed"
    );
    let probes = harness.fake.received_named("get_events");
    assert!(probes.iter().any(|p| p["after"] == 1), "{probes:?}");
}

#[tokio::test]
async fn expired_history_reported_by_its_code_is_skipped_once() {
    skips_the_gap_once(true).await;
}

#[tokio::test]
async fn expired_history_an_older_aether_names_only_in_text_is_skipped_once() {
    skips_the_gap_once(false).await;
}

/// The check before subscribing found nothing (say, the history expired
/// just after it); the replay hits the gap and Aether hangs up. The pump
/// resumes within one reconnect and still says so once.
#[tokio::test]
async fn a_replay_that_hits_expired_history_resumes_within_one_reconnect() {
    let harness = Harness::new().await;
    history_with_a_gap(&harness, true);
    let fleet_checks = Arc::new(AtomicUsize::new(0));
    let counted = fleet_checks.clone();
    let expired = "event history expired within sequence range 2..=4; obtain a fresh state snapshot before resuming replay";
    harness.fake.set_responder(
        "get_events",
        Arc::new(move |cmd| {
            let fleet = cmd["query"].get("agent").is_none();
            if !fleet || counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return Scripted::reply("events", json!([]));
            }
            // Without bounds, as a scripted refusal has none: read from its text.
            Scripted::refuse("history_expired", expired)
        }),
    );
    let pump = pump_from(&harness, "1").await;
    harness
        .eventually("the fleet checked once and was hung up on", || async {
            fleet_checks.load(Ordering::SeqCst) >= 1 && harness.fake.connections_accepted() >= 2
        })
        .await;
    cursor_reaches(&harness, keys::CURSOR_FLEET, 6).await;
    pump.abort();

    let notes = expired_notes(&harness).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(harness.fake.connections_accepted(), 2, "one reconnect");
}

/// A cursor that is not a seq would replay from 0, straight into expired
/// history: the stream follows live instead, without a gap to report.
#[tokio::test]
async fn an_unreadable_cursor_starts_the_stream_live() {
    let harness = Harness::new().await;
    history_with_a_gap(&harness, true);
    let pump = pump_from(&harness, "not-a-seq").await;
    cursor_reaches(&harness, keys::CURSOR_FLEET, 6).await;
    let seq = harness.fake.push_event(stranger_usage());
    cursor_reaches(&harness, keys::CURSOR_FLEET, seq).await;
    pump.abort();

    assert_eq!(expired_notes(&harness).await, Vec::<String>::new());
    let subscribes = harness.fake.received_named("subscribe");
    let fleet = subscribes
        .iter()
        .find(|s| s["query"].get("agent").is_none());
    assert_eq!(fleet.expect("the fleet subscribed")["after"], Value::Null);
}

/// An event whose handling failed inside Aether once is tried again, not
/// reported and skipped: the off-policy child is still cancelled.
#[tokio::test]
async fn an_event_that_failed_for_a_passing_reason_is_retried() {
    let mut config = Config::default();
    config.models.deny = vec![ModelRef::new(DENIED.0, DENIED.1)];
    let harness = Harness::with(config, None).await;
    let pump = pump_from(&harness, "0").await;
    harness
        .fake
        .script("cancel", Scripted::refuse("internal", "database is locked"));
    harness.fake.set_default("cancel", Scripted::ok());
    let kind = json!({"type": "agent.created", "name": CHILD,
                      "model": {"provider": DENIED.0, "model": DENIED.1}});
    let mut created = fake::event(Some(CHILD), None, kind);
    created["parent_agent_id"] = json!(ORCH);
    let seq = harness.fake.push_event(created);
    cursor_reaches(&harness, keys::CURSOR_FLEET, seq).await;
    pump.abort();

    let cancels = harness.fake.received_named("cancel");
    assert_eq!(
        cancels.len(),
        2,
        "refused once, then cancelled: {cancels:?}"
    );
    let notes = harness.system_notes().await;
    assert!(
        !notes
            .iter()
            .any(|n| n.contains("could not act on Aether event")),
        "{notes:?}"
    );
}
