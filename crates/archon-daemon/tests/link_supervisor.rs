//! The orchestrator supervisor against the fake aetherd: creation keys,
//! reuse, wake, restart, adopting successors, revival backoff, and
//! operator messages.

mod link_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::aether_ids::AetherQuestionRef;
use archon_core::autonomy::{Directive, DirectiveSource};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{FeedItemId, QuestionId};
use archon_core::lesson::{LessonDraft, LessonKind, LessonPatch};
use archon_core::notify::{AnswerVia, FeedEntry, NotificationKind, Question};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{handlers, mirror, supervisor};
use archon_queue::store::keys;
use link_common::{Harness, WAIT, agent_id};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";
const ORCH_3: &str = "agt_0003";
/// `kv`: the orchestrators Archon replaced, and whether their sweep is owed.
const RETIRED_KEY: &str = "orchestrator_retired";

/// The replaced orchestrators on record, each with whether what it left
/// running is still to be cancelled.
async fn retired(harness: &Harness) -> Vec<(String, bool)> {
    let raw = harness.kv(RETIRED_KEY).await.unwrap_or_else(|| "[]".into());
    let record: Vec<Value> = serde_json::from_str(&raw).expect("the record is a list");
    let entry = |retired: &Value| {
        let agent = retired["agent"].as_str().expect("an agent id").to_owned();
        (agent, retired["owed"].as_bool().expect("owed or not"))
    };
    record.iter().map(entry).collect()
}

fn owed(agent: &str) -> (String, bool) {
    (agent.to_owned(), true)
}

fn swept_already(agent: &str) -> (String, bool) {
    (agent.to_owned(), false)
}

fn agent_in(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

/// How far ahead the deferred revival is scheduled.
fn next_revival_in(harness: &Harness) -> Duration {
    let at = harness
        .ctx
        .supervisor
        .next_revival()
        .expect("a revival is scheduled");
    (at - archon_core::now()).to_std().unwrap_or_default()
}

/// `get_agent` answers from a table the test edits as agents change state.
fn agent_table(harness: &Harness, initial: &[(&str, &str)]) -> Arc<Mutex<HashMap<String, String>>> {
    let states: HashMap<String, String> = initial
        .iter()
        .map(|(id, state)| ((*id).to_owned(), (*state).to_owned()))
        .collect();
    let states = Arc::new(Mutex::new(states));
    let table = states.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            match table.lock().unwrap().get(id) {
                Some(state) => Scripted::reply("agent", agent_in(id, state)),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
    states
}

async fn create_key(harness: &Harness) -> String {
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH, "ready")),
    );
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let creates = harness.fake.received_named("create_agent");
    creates.last().unwrap()["idempotency_key"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn the_first_ensure_creates_the_orchestrator_under_a_store_scoped_key() {
    let harness = Harness::new().await;
    harness.connect().await;
    let mut watch = harness.ctx.supervisor.watch();
    let key = create_key(&harness).await;
    let store = supervisor::store_id(&harness.ctx).await.unwrap();
    assert_eq!(key, format!("archon-orchestrator-{store}-1"));
    assert_eq!(*watch.borrow_and_update(), Some(agent_id(ORCH)));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert!(
        harness.system_notes().await.is_empty(),
        "a first start is not news"
    );
}

#[tokio::test]
async fn fresh_stores_never_reuse_an_orchestrator_key() {
    let first = Harness::new().await;
    first.connect().await;
    let second = Harness::new().await;
    second.connect().await;
    let (a, b) = (create_key(&first).await, create_key(&second).await);
    assert!(a.ends_with("-1") && b.ends_with("-1"), "{a} {b}");
    assert_ne!(a, b, "a fresh store must not get the old orchestrator back");
}

#[tokio::test]
async fn a_live_stored_orchestrator_is_reused_and_announced_to_the_pump() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "waiting")]);
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH));
    assert!(harness.fake.received_named("create_agent").is_empty());
    assert_eq!(
        *harness.ctx.supervisor.watch().borrow(),
        Some(agent_id(ORCH))
    );
}

#[tokio::test]
async fn a_failed_orchestrator_is_woken_and_a_cancelled_one_restarted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let states = agent_table(&harness, &[(ORCH, "failed")]);
    harness
        .fake
        .script("wake", Scripted::reply("agent", agent_in(ORCH, "ready")));
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        agent_id(ORCH)
    );
    assert_eq!(harness.fake.received_named("wake")[0]["agent"], ORCH);
    // It ends a turn well, so the next stop is revived at once.
    supervisor::turn_ended(&harness.ctx, Some("ready"))
        .await
        .unwrap();

    states
        .lock()
        .unwrap()
        .insert(ORCH.into(), "cancelled".into());
    harness.fake.script(
        "restart",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].contains("woken") && notes[0].contains("failed"),
        "{notes:?}"
    );
    assert!(
        notes[1].contains("restarted") && notes[1].contains(ORCH_2),
        "{notes:?}"
    );
}

/// Aether restarts from a checkpoint and the tail of the transcript, which
/// may not hold what an earlier trigger stated.
#[tokio::test]
async fn a_restarted_orchestrator_is_told_its_directives_again() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let directive = Directive::new(
        "no yolo today",
        DirectiveSource::Operator,
        None,
        archon_core::now(),
    )
    .unwrap();
    {
        let mut store = harness.ctx.store.lock().await;
        store.insert_directive(&directive).unwrap();
    }
    agent_table(&harness, &[(ORCH, "cancelled")]);
    harness.fake.script(
        "restart",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    assert!(triggers::held(&harness.ctx).await.unwrap().is_empty());
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));

    let held = triggers::held(&harness.ctx).await.unwrap();
    let [TriggerEvent::DirectivesRestated { reason }] = held.as_slice() else {
        panic!("held {held:?}");
    };
    assert!(reason.contains("restarted"), "{reason}");
    assert!(triggers::due(&harness.ctx).await.unwrap());
}

#[tokio::test]
async fn a_refused_wake_falls_back_to_restart() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "failed")]);
    harness.fake.script(
        "wake",
        Scripted::refuse("invalid_transition", "not wakeable"),
    );
    harness.fake.script("cancel", Scripted::ok());
    harness.fake.script(
        "restart",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(harness.fake.received_named("restart").len(), 1);
    // A failed agent's children outlive it, and would report to nobody.
    let received = harness.fake.received();
    let sent = |cmd: &str| received.iter().position(|c| c["cmd"] == cmd).unwrap();
    assert_eq!(received[sent("cancel")]["agent"], ORCH);
    assert!(
        sent("cancel") < sent("restart"),
        "swept before it is replaced"
    );
    // On record all the same: its children's last events are still Archon's.
    assert_eq!(retired(&harness).await, [swept_already(ORCH)]);
}

/// The replacement is not made while the old agent's children may still
/// run: the revival fails, and is tried again at the streak's pace.
#[tokio::test]
async fn a_failed_sweep_of_the_old_orchestrators_children_defers_the_revival() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "failed")]);
    harness.fake.script(
        "wake",
        Scripted::refuse("invalid_transition", "not wakeable"),
    );
    harness
        .fake
        .script("cancel", Scripted::refuse("internal", "store busy"));
    let refused = supervisor::ensure_orchestrator(&harness.ctx).await;
    assert!(
        matches!(&refused, Err(ArchonError::Aether(why)) if why.contains("store busy")),
        "{refused:?}"
    );
    assert!(harness.fake.received_named("restart").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert!(next_revival_in(&harness) <= supervisor::REVIVAL_BACKOFF[0]);
    let paced = deferred(&harness).await;
    assert!(paced.contains("failed"), "the next try waits: {paced}");
    assert_eq!(harness.fake.received_named("wake").len(), 1);
}

/// A restart Aether fails without refusing says nothing about whether the
/// agent can be restarted: a fresh generation would lose its conversation.
#[tokio::test]
async fn a_restart_that_merely_fails_is_retried_and_never_replaced_by_a_new_generation() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "cancelled")]);
    harness
        .fake
        .script("restart", Scripted::refuse("internal", "store busy"));
    let failed = supervisor::ensure_orchestrator(&harness.ctx).await;
    assert!(
        matches!(&failed, Err(ArchonError::Aether(why)) if why.contains("store busy")),
        "{failed:?}"
    );
    assert!(harness.fake.received_named("create_agent").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );

    let pause = supervisor::REVIVAL_BACKOFF[0].as_secs_f64();
    harness.rewind_backoff(pause).await;
    harness.fake.script(
        "restart",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2), "the same conversation, restarted");
    assert!(harness.fake.received_named("create_agent").is_empty());
}

/// `get_agent` answers a restart chain: each agent's state and successor.
fn restart_chain(harness: &Harness, chain: &[(&str, &str, &str, Option<&str>)]) {
    let agents: HashMap<String, Value> = chain
        .iter()
        .map(|(id, role, state, successor)| {
            let mut agent = agent_in(id, state);
            agent["role"] = json!(role);
            agent["restarted_as"] = json!(successor);
            ((*id).to_owned(), agent)
        })
        .collect();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            match agents.get(id) {
                Some(agent) => Scripted::reply("agent", agent.clone()),
                None => Scripted::refuse("not_found", "no such agent"),
            }
        }),
    );
}

fn swept(harness: &Harness) -> Vec<Value> {
    let cancels = harness.fake.received_named("cancel");
    cancels.iter().map(|cmd| cmd["agent"].clone()).collect()
}

/// A sweep Aether refuses is owed and retried, said once, and neither holds
/// the successor up nor undoes its adoption.
#[tokio::test]
async fn a_refused_sweep_is_owed_and_said_once_while_the_successor_serves() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "failed", Some(ORCH_2)),
            (ORCH_2, role, "running", None),
        ],
    );
    let mut watch = harness.ctx.supervisor.watch();
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(*watch.borrow_and_update(), Some(agent_id(ORCH_2)));
    assert!(swept(&harness).is_empty(), "adoption waits for no sweep");
    assert_eq!(retired(&harness).await, [owed(ORCH)]);

    for _ in 0..2 {
        harness
            .fake
            .script("cancel", Scripted::refuse("internal", "store busy"));
        let done = supervisor::sweep_retired(&harness.ctx).await.unwrap();
        assert!(!done, "Aether has not acknowledged the sweep");
        assert_eq!(retired(&harness).await, [owed(ORCH)], "kept in the store");
        let serving = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
        assert_eq!(serving, agent_id(ORCH_2), "an owed sweep stops nothing");
    }
    let notes = harness.system_notes().await;
    let said: Vec<&String> = notes.iter().filter(|n| n.contains("store busy")).collect();
    assert_eq!(said.len(), 1, "a failing sweep is said once: {notes:?}");
    assert!(
        said[0].contains(ORCH) && said[0].contains("keeps retrying"),
        "{said:?}"
    );

    harness.fake.script("cancel", Scripted::ok());
    assert!(supervisor::sweep_retired(&harness.ctx).await.unwrap());
    assert_eq!(swept(&harness), [ORCH, ORCH, ORCH], "retried, then done");
    assert_eq!(retired(&harness).await, [swept_already(ORCH)]);
    let notes = harness.system_notes().await;
    assert!(
        notes.last().unwrap().contains("Cancelled what"),
        "{notes:?}"
    );

    // Nothing is owed any more: an unscripted `cancel` would be refused.
    assert!(supervisor::sweep_retired(&harness.ctx).await.unwrap());
    assert_eq!(swept(&harness).len(), 3);
    let adopted = notes
        .iter()
        .filter(|n| n.contains("was restarted in Aether"));
    assert_eq!(adopted.count(), 1, "adopted once: {notes:?}");
}

/// Aether restarted the orchestrator more than once while Archon was away:
/// each generation left behind has children that would report to nobody.
#[tokio::test]
async fn every_retired_generation_of_a_restart_chain_is_swept() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "failed", Some(ORCH_2)),
            (ORCH_2, role, "failed", Some(ORCH_3)),
            (ORCH_3, role, "running", None),
        ],
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_3));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_3)
    );
    assert_eq!(retired(&harness).await, [owed(ORCH), owed(ORCH_2)]);

    // The first sweep fails; the second generation's is made all the same.
    harness
        .fake
        .script("cancel", Scripted::refuse("internal", "store busy"));
    harness.fake.script("cancel", Scripted::ok());
    assert!(!supervisor::sweep_retired(&harness.ctx).await.unwrap());
    assert_eq!(swept(&harness), [ORCH, ORCH_2]);
    assert_eq!(retired(&harness).await, [owed(ORCH), swept_already(ORCH_2)]);

    harness.fake.script("cancel", Scripted::ok());
    assert!(supervisor::sweep_retired(&harness.ctx).await.unwrap());
    assert_eq!(swept(&harness), [ORCH, ORCH_2, ORCH], "never the successor");
    assert_eq!(
        retired(&harness).await,
        [swept_already(ORCH), swept_already(ORCH_2)]
    );
}

/// A successor that is no orchestrator ends the chain before it, and the
/// agent that stays in charge keeps its children.
#[tokio::test]
async fn the_orchestrator_left_in_charge_of_a_restart_chain_is_never_swept() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "cancelled", Some(ORCH_2)),
            (ORCH_2, role, "running", Some(ORCH_3)),
            (ORCH_3, "worker", "running", None),
        ],
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    harness.fake.script("cancel", Scripted::ok());
    assert!(supervisor::sweep_retired(&harness.ctx).await.unwrap());
    assert_eq!(swept(&harness), [ORCH]);
    assert_eq!(retired(&harness).await, [swept_already(ORCH)]);
}

/// An adoption that stopped before the stored id changed leaves the old
/// agent both in charge and on record: its sweep waits for the switch.
#[tokio::test]
async fn an_owed_sweep_never_cancels_the_orchestrator_still_in_charge() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let record = json!([{"agent": ORCH, "at": archon_core::now(), "owed": true}]);
    harness.set_kv(RETIRED_KEY, &record.to_string()).await;

    let done = supervisor::sweep_retired(&harness.ctx).await.unwrap();
    assert!(!done, "still owed once the switch is made");
    assert!(swept(&harness).is_empty());
    assert_eq!(retired(&harness).await, [owed(ORCH)]);
}

/// Aether restarts an agent as often as it is asked to: no length of chain
/// leaves Archon without its orchestrator.
#[tokio::test]
async fn a_long_restart_chain_is_followed_to_its_end() {
    const RESTARTS: usize = 40;
    let harness = Harness::new().await;
    harness.connect().await;
    let ids: Vec<String> = (1..=RESTARTS + 1).map(|n| format!("agt_{n:04}")).collect();
    harness.set_kv(keys::ORCHESTRATOR_AGENT, &ids[0]).await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    let chain: Vec<(&str, &str, &str, Option<&str>)> = ids
        .iter()
        .enumerate()
        .map(|(at, id)| match ids.get(at + 1) {
            Some(next) => (id.as_str(), role, "cancelled", Some(next.as_str())),
            None => (id.as_str(), role, "running", None),
        })
        .collect();
    restart_chain(&harness, &chain);

    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(&ids[RESTARTS]));
    let record = retired(&harness).await;
    assert_eq!(record.len(), RESTARTS, "every generation left behind");
    assert!(record.iter().all(|(_, owed)| *owed), "{record:?}");
    assert_eq!(record[0].0, ids[0]);
}

/// Aether forgot the end of the chain only: the orchestrator is the last
/// agent it still knows, and what the earlier ones left running is owed.
#[tokio::test]
async fn a_successor_aether_forgot_ends_the_chain_without_losing_its_sweeps() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "cancelled", Some(ORCH_2)),
            (ORCH_2, role, "running", Some(ORCH_3)),
        ],
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(retired(&harness).await, [owed(ORCH)]);
    assert!(harness.fake.received_named("create_agent").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_GENERATION).await.as_deref(),
        Some("1"),
        "no fresh generation"
    );
}

/// A restart never leads back to an agent, so a chain that does is Aether
/// answering wrongly: nothing is adopted and nothing is cancelled.
#[tokio::test]
async fn a_restart_chain_that_loops_is_refused_and_changes_nothing() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "cancelled", Some(ORCH_2)),
            (ORCH_2, role, "cancelled", Some(ORCH)),
        ],
    );
    let refused = supervisor::ensure_orchestrator(&harness.ctx).await;
    assert!(
        matches!(&refused, Err(ArchonError::Protocol(why)) if why.contains("lead back")),
        "{refused:?}"
    );
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert_eq!(harness.kv(RETIRED_KEY).await, None);
    assert!(swept(&harness).is_empty());
}

#[tokio::test]
async fn a_successor_left_by_restart_is_followed_not_restarted_again() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut old = agent_in(ORCH, "cancelled");
    old["restarted_as"] = json!(ORCH_2);
    harness
        .fake
        .script("get_agent", Scripted::reply("agent", old));
    harness.fake.script(
        "get_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "running")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert!(harness.fake.received_named("restart").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    harness.fake.script("cancel", Scripted::ok());
    assert!(supervisor::sweep_retired(&harness.ctx).await.unwrap());
    assert_eq!(swept(&harness), [ORCH], "the old agent's children only");
}

/// A record no version of Archon can read must not cost the orchestrator:
/// it is reset, and the operator is told what the reset loses.
#[tokio::test]
async fn an_unreadable_record_of_replaced_orchestrators_is_reset_and_reported() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(RETIRED_KEY, "{not a list").await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "failed", Some(ORCH_2)),
            (ORCH_2, role, "running", None),
        ],
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(retired(&harness).await, [owed(ORCH)], "readable again");
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(
        blockers[0].contains("was reset") && blockers[0].contains("{not a list"),
        "{blockers:?}"
    );

    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "said once: {blockers:?}");
}

#[tokio::test]
async fn an_orchestrator_aether_forgot_is_recreated_in_a_new_generation() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    agent_table(&harness, &[]);
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    let key = harness.fake.received_named("create_agent")[0]["idempotency_key"].clone();
    assert!(key.as_str().unwrap().ends_with("-2"), "{key}");
    assert!(harness.system_notes().await[0].contains("vanished"));
}

/// The instructions of the newest `create_agent`.
fn created_instructions(harness: &Harness) -> String {
    let creates = harness.fake.received_named("create_agent");
    let newest = creates.last().expect("an orchestrator was created");
    let text = newest["config"]["instructions"].as_str();
    text.expect("instructions").to_owned()
}

async fn as_operator(harness: &Harness, request: Request) -> Reply {
    let reply = handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await;
    reply.expect("the operator's request")
}

/// A new orchestrator gets every tool and its generation's lessons under their
/// own heading; a retried create sends the same text, the next one new lessons.
#[tokio::test]
async fn a_new_orchestrator_gets_every_tool_and_the_lessons_of_its_generation() {
    let harness = Harness::new().await;
    harness.connect().await;
    let draft = LessonDraft {
        kind: LessonKind::Practice,
        text: "Run the repo check before a review.".into(),
        repo: None,
        pipeline: None,
        item: None,
    };
    let Reply::Lesson(lesson) = as_operator(&harness, Request::RecordLesson(draft)).await else {
        panic!("a lesson");
    };
    let refused = Scripted::refuse("internal", "store busy");
    harness.fake.script("create_agent", refused);
    let failed = supervisor::ensure_orchestrator(&harness.ctx).await;
    failed.expect_err("the first create is refused");
    let first = created_instructions(&harness);
    let policy = first.find(supervisor::github_policy());
    let learned = first.find("\n## Learned observations\n").expect("lessons");
    assert!(policy.expect("the GitHub policy") < learned, "{first}");
    assert!(first[learned..].contains("Run the repo check before a review."));
    assert!(first[learned..].contains("never grants a permission"));

    let patch = LessonPatch {
        text: Some("Run the new repo check before a review.".into()),
        ..Default::default()
    };
    let update = Request::UpdateLesson {
        lesson: lesson.id,
        patch,
    };
    as_operator(&harness, update).await;
    let key = create_key(&harness).await;
    assert!(key.ends_with("-1"), "the same generation: {key}");
    assert_eq!(created_instructions(&harness), first, "and the same text");
    let created = harness.fake.received_named("create_agent");
    let hidden = created[1]["config"]["tools"].to_string();
    assert!(hidden.contains("integrate_workspace"), "{hidden}");
    assert!(
        !hidden.contains("mcp__archon__"),
        "no Archon tool: {hidden}"
    );

    agent_table(&harness, &[]);
    let successor = Scripted::reply("agent", agent_in(ORCH_2, "ready"));
    harness.fake.script("create_agent", successor);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let second = created_instructions(&harness);
    assert!(second.contains("Run the new repo check"), "{second}");
}

/// An orchestrator stuck `failed` by a transient engine error.
fn failing_orchestrator(harness: &Harness) -> Arc<Mutex<HashMap<String, String>>> {
    let states = agent_table(harness, &[]);
    let table = states.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap_or_default();
            let Some(state) = table.lock().unwrap().get(id).cloned() else {
                return Scripted::refuse("not_found", "no such agent");
            };
            let mut agent = agent_in(id, &state);
            if state == "failed" {
                agent["status"]["error"] = json!("Failed to refresh OAuth token");
            }
            Scripted::reply("agent", agent)
        }),
    );
    states.lock().unwrap().insert(ORCH.into(), "failed".into());
    harness.fake.set_responder(
        "wake",
        Arc::new(|cmd| {
            let id = cmd["agent"].as_str().unwrap_or_default();
            Scripted::reply("agent", agent_in(id, "ready"))
        }),
    );
    states
}

async fn deferred(harness: &Harness) -> String {
    let err = supervisor::ensure_orchestrator(&harness.ctx)
        .await
        .unwrap_err();
    let ArchonError::Unavailable(reason) = err else {
        panic!("a deferred revival is unavailable, got {err}");
    };
    reason
}

#[tokio::test]
async fn a_failing_orchestrator_is_revived_with_growing_pauses_and_never_given_up() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let states = failing_orchestrator(&harness);
    let wakes = || harness.fake.received_named("wake").len();

    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(wakes(), 1, "the first revival of a streak is immediate");
    for (revival, pause) in supervisor::REVIVAL_BACKOFF.iter().take(4).enumerate() {
        let reason = deferred(&harness).await;
        assert!(reason.contains("archon orchestrator restart"), "{reason}");
        assert_eq!(wakes(), revival + 1, "nothing is woken before it is due");
        let ahead = next_revival_in(&harness);
        assert!(
            ahead <= *pause && ahead + Duration::from_secs(2) > *pause,
            "{ahead:?}"
        );
        harness.rewind_backoff(pause.as_secs_f64()).await;
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
        assert_eq!(wakes(), revival + 2);
    }
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "told once per streak: {blockers:?}");
    assert!(blockers[0].contains("OAuth token"), "{blockers:?}");
    let woken = harness.system_notes().await;
    assert_eq!(woken.len(), 2, "quiet once the Blocker is up: {woken:?}");

    // The transient error clears: a good turn ends the streak.
    states.lock().unwrap().insert(ORCH.into(), "ready".into());
    supervisor::turn_ended(&harness.ctx, Some("ready"))
        .await
        .unwrap();
    assert!(
        harness
            .system_notes()
            .await
            .last()
            .unwrap()
            .contains("working again")
    );
    assert_eq!(harness.ctx.supervisor.next_revival(), None);
    states.lock().unwrap().insert(ORCH.into(), "failed".into());
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(wakes(), 6, "a new streak starts without a pause");
}

#[tokio::test]
async fn a_failed_turn_does_not_end_the_streak() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    failing_orchestrator(&harness);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    supervisor::turn_ended(&harness.ctx, Some("failed"))
        .await
        .unwrap();
    supervisor::turn_ended(&harness.ctx, None).await.unwrap();
    deferred(&harness).await;
    assert_eq!(harness.fake.received_named("wake").len(), 1);
}

#[tokio::test]
async fn a_deferred_revival_runs_by_itself_when_due() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let states = failing_orchestrator(&harness);
    let revivals = tokio::spawn(supervisor::revive_when_due(harness.ctx.clone()));
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    // Due in about 300ms rather than 10s.
    harness.rewind_backoff(9.7).await;
    deferred(&harness).await;
    let woken = states.clone();
    harness.fake.set_responder(
        "wake",
        Arc::new(move |_| {
            woken.lock().unwrap().insert(ORCH.into(), "ready".into());
            Scripted::reply("agent", agent_in(ORCH, "ready"))
        }),
    );
    harness.fake.wait_for("wake", 2, WAIT).await;
    harness
        .eventually("the revival is no longer pending", || async {
            harness.ctx.supervisor.next_revival().is_none()
        })
        .await;
    revivals.abort();
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        agent_id(ORCH)
    );
}

#[tokio::test]
async fn an_operator_restart_overrides_the_backoff() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    // A store left behind by the old give-up rule.
    harness.set_kv("orchestrator_gave_up", "1").await;
    let states = failing_orchestrator(&harness);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    deferred(&harness).await;

    harness.fake.script("cancel", Scripted::ok());
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    states.lock().unwrap().insert(ORCH_2.into(), "ready".into());
    // Through the operator's request, as `archon orchestrator restart` sends it.
    let reply = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        Request::OrchestratorRestart,
    )
    .await
    .unwrap();
    let Reply::Orchestrator(info) = reply else {
        panic!("expected the orchestrator, got {reply:?}");
    };
    assert_eq!(info.agent, agent_id(ORCH_2));
    assert_eq!(harness.fake.received_named("cancel")[0]["agent"], ORCH);
    assert_eq!(harness.ctx.supervisor.next_revival(), None);
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        info.agent
    );
    states
        .lock()
        .unwrap()
        .insert(ORCH_2.into(), "failed".into());
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(
        harness.fake.received_named("wake").last().unwrap()["agent"],
        ORCH_2,
        "the fresh generation starts a fresh streak"
    );
}

/// The stored id may be generations behind what Aether runs: the restart
/// cancels the agent that is running, and owes the ones in between.
#[tokio::test]
async fn an_operator_restart_cancels_the_newest_incarnation_and_owes_the_chain() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let role = supervisor::ORCHESTRATOR_ROLE;
    restart_chain(
        &harness,
        &[
            (ORCH, role, "failed", Some(ORCH_2)),
            (ORCH_2, role, "running", None),
            (ORCH_3, role, "ready", None),
        ],
    );
    harness.fake.script("cancel", Scripted::ok());
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH_3, "ready")),
    );
    let fresh = supervisor::restart_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(fresh, agent_id(ORCH_3));
    assert_eq!(swept(&harness), [ORCH_2], "the one still running");
    assert_eq!(retired(&harness).await, [owed(ORCH), swept_already(ORCH_2)]);
}

/// The operator's restart is the way out of a record that cannot be read.
#[tokio::test]
async fn an_operator_restart_works_over_an_unreadable_record() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(RETIRED_KEY, "[{\"agent\": 7}]").await;
    agent_table(&harness, &[(ORCH, "running"), (ORCH_2, "ready")]);
    harness.fake.script("cancel", Scripted::ok());
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH_2, "ready")),
    );
    let fresh = supervisor::restart_orchestrator(&harness.ctx)
        .await
        .unwrap();
    assert_eq!(fresh, agent_id(ORCH_2));
    assert_eq!(retired(&harness).await, [swept_already(ORCH)]);
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(blockers[0].contains("was reset"), "{blockers:?}");
}

#[tokio::test]
async fn a_restart_made_in_aether_while_archond_was_down_is_adopted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    harness.set_kv("orchestrator_gave_up", "1").await;
    harness
        .set_kv("orchestrator_blocker", "The orchestrator stopped.")
        .await;
    let mut old = agent_in(ORCH, "failed");
    old["restarted_as"] = json!(ORCH_2);
    let successor = agent_in(ORCH_2, "ready");
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| match cmd["id"].as_str() {
            Some(ORCH) => Scripted::reply("agent", old.clone()),
            _ => Scripted::reply("agent", successor.clone()),
        }),
    );
    let mut watch = harness.ctx.supervisor.watch();
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id(ORCH_2));
    assert_eq!(*watch.borrow_and_update(), Some(agent_id(ORCH_2)));
    assert_eq!(retired(&harness).await, [owed(ORCH)]);
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_GENERATION).await.as_deref(),
        Some("1"),
        "a restart continues the generation's conversation"
    );
    for cmd in ["wake", "restart", "create_agent"] {
        assert!(harness.fake.received_named(cmd).is_empty(), "{cmd}");
    }
    let notes = harness.system_notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(ORCH) && notes[0].contains(ORCH_2),
        "{notes:?}"
    );
    assert_eq!(
        harness.kv("orchestrator_blocker").await,
        None,
        "adoption deletes the blocker rather than blanking it"
    );
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        id
    );
    assert_eq!(harness.system_notes().await.len(), 1, "adopted once");
}

#[tokio::test]
async fn a_successor_that_is_not_an_orchestrator_is_never_adopted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut old = agent_in(ORCH, "cancelled");
    old["restarted_as"] = json!(ORCH_2);
    let stranger = fake::agent(ORCH_2, None);
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| match cmd["id"].as_str() {
            Some(ORCH) => Scripted::reply("agent", old.clone()),
            _ => Scripted::reply("agent", stranger.clone()),
        }),
    );
    supervisor::adopt_restart(&harness.ctx, &agent_id(ORCH), &agent_id(ORCH_2))
        .await
        .unwrap();
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH)
    );
    assert!(harness.system_notes().await[0].contains("did not adopt"));

    harness.fake.script(
        "restart",
        Scripted::refuse("invalid_argument", "already restarted"),
    );
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in("agt_0003", "ready")),
    );
    let id = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(id, agent_id("agt_0003"), "a fresh generation instead");
}

#[tokio::test]
async fn a_restart_of_an_agent_that_is_no_longer_the_orchestrator_is_ignored() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    supervisor::adopt_restart(&harness.ctx, &agent_id(ORCH), &agent_id("agt_0003"))
        .await
        .unwrap();
    assert!(harness.fake.received_named("get_agent").is_empty());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_AGENT).await.as_deref(),
        Some(ORCH_2)
    );
}

#[tokio::test]
async fn an_operator_message_is_keyed_by_its_feed_item() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "running")]);
    harness
        .fake
        .script("send_message", Scripted::reply("message_sent", json!({})));
    let item = FeedItemId::new();
    supervisor::deliver_operator_message(&harness.ctx, &item, "status?")
        .await
        .unwrap();
    let sent = &harness.fake.received_named("send_message")[0];
    assert_eq!(sent["idempotency_key"], item.as_str());
    assert_eq!(sent["to"], ORCH);
}

#[tokio::test]
async fn a_refused_operator_message_does_not_leave_the_chat_waiting() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "running")]);
    harness.fake.script(
        "send_message",
        Scripted::refuse("invalid_argument", "too long"),
    );
    let err = supervisor::deliver_operator_message(&harness.ctx, &FeedItemId::new(), "hi")
        .await
        .unwrap_err();
    assert!(matches!(err, ArchonError::Invalid(_)), "{err}");
    assert!(!harness.ctx.replies.lock().await.awaiting_reply());
    assert_eq!(harness.kv(archon_daemon::feed::REPLY_STATE_KEY).await, None);
}

#[tokio::test]
async fn answers_go_to_aether_or_to_the_orchestrator_by_question_origin() {
    let mut harness = Harness::new().await;
    harness.connect().await;
    let aether = AetherQuestionRef {
        agent: agent_id(ORCH_2),
        seq: 17,
    };
    let from_aether = archon_daemon::feed::aether_question(&harness.ctx, aether, "Which?", None)
        .await
        .unwrap()
        .unwrap();
    let from_archon: Question =
        archon_daemon::feed::question(&harness.ctx, "Merge?".into(), vec!["yes".into()], None)
            .await
            .unwrap();
    harness.fake.script(
        "reply_to_question",
        Scripted::reply("message_sent", json!({})),
    );
    supervisor::route_answer(&harness.ctx, &from_aether, "the first", AnswerVia::Archon)
        .await
        .unwrap();
    supervisor::route_answer(&harness.ctx, &from_archon, "yes", AnswerVia::Archon)
        .await
        .unwrap();
    let replied = &harness.fake.received_named("reply_to_question")[0];
    assert_eq!(
        (replied["to"].as_str(), replied["question_seq"].as_u64()),
        (Some(ORCH_2), Some(17))
    );
    let answer = TriggerEvent::OperatorAnswer {
        question: from_archon.id.clone(),
        number: Some(from_archon.number),
        question_text: from_archon.text.clone(),
        options: from_archon.options.clone(),
        item: from_archon.item.clone(),
        asked_at: Some(from_archon.asked_at),
        text: "yes".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![answer]);
}

async fn answer(
    harness: &Harness,
    question: &QuestionId,
    text: &str,
) -> Result<Reply, ArchonError> {
    let request = Request::Answer {
        question: question.clone(),
        text: text.into(),
    };
    handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await
}

async fn answers_in_feed(harness: &Harness) -> Vec<String> {
    let feed = harness.feed().await;
    feed.into_iter()
        .filter_map(|entry| match entry {
            FeedEntry::Answer { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

/// An answer given while Aether is unreachable is not recorded,
/// so answering again once the link is back delivers it.
#[tokio::test]
async fn an_undelivered_answer_leaves_the_question_open_for_another_try() {
    let harness = Harness::new().await;
    let aether = AetherQuestionRef {
        agent: agent_id(ORCH),
        seq: 9,
    };
    let asked = archon_daemon::feed::aether_question(&harness.ctx, aether, "Which?", None)
        .await
        .unwrap()
        .expect("a new question");

    let refused = answer(&harness, &asked.id, "the first").await.unwrap_err();
    assert!(matches!(refused, ArchonError::Unavailable(_)), "{refused}");
    let still = harness.ctx.store.lock().await.question(&asked.id).unwrap();
    assert!(still.is_open(), "an undelivered answer is not recorded");
    assert!(answers_in_feed(&harness).await.is_empty());

    harness.connect().await;
    harness.fake.script(
        "reply_to_question",
        Scripted::reply("message_sent", json!({})),
    );
    let Reply::Question(answered) = answer(&harness, &asked.id, "the second").await.unwrap() else {
        panic!("expected the answered question");
    };
    assert_eq!(answered.answer.as_deref(), Some("the second"));
    let replies = harness.fake.received_named("reply_to_question");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["content"], "the second");
    assert_eq!(answers_in_feed(&harness).await, ["the second"]);
}

#[tokio::test]
async fn an_answered_or_unknown_question_is_refused_without_delivery() {
    let mut harness = Harness::new().await;
    let asked: Question =
        archon_daemon::feed::question(&harness.ctx, "Merge?".into(), vec!["yes".into()], None)
            .await
            .unwrap();
    answer(&harness, &asked.id, "yes").await.unwrap();
    let again = answer(&harness, &asked.id, "no").await.unwrap_err();
    assert!(
        matches!(again, ArchonError::InvalidTransition(_)),
        "{again}"
    );
    let unknown = answer(&harness, &QuestionId::new(), "x").await.unwrap_err();
    assert!(matches!(unknown, ArchonError::NotFound(_)), "{unknown}");

    let delivered = TriggerEvent::OperatorAnswer {
        question: asked.id.clone(),
        number: Some(asked.number),
        question_text: asked.text.clone(),
        options: asked.options.clone(),
        item: asked.item.clone(),
        asked_at: Some(asked.asked_at),
        text: "yes".into(),
    };
    assert_eq!(harness.drain_triggers().await, vec![delivered]);
    assert_eq!(answers_in_feed(&harness).await, ["yes"]);
}

async fn attach(harness: &Harness) -> Result<Reply, ArchonError> {
    handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        Request::OrchestratorAttach,
    )
    .await
}

#[tokio::test]
async fn tui_attachment_creates_once_then_reuses_without_messages_or_workers() {
    let harness = Harness::new().await;
    harness.connect().await;
    agent_table(&harness, &[(ORCH, "ready")]);
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(ORCH, "ready")),
    );
    for _ in 0..2 {
        let Reply::Orchestrator(info) = attach(&harness).await.unwrap() else {
            panic!("expected an orchestrator snapshot");
        };
        assert_eq!(info.agent, agent_id(ORCH));
        assert_eq!(info.status, "ready");
    }
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0]["name"], "Archon");
    assert_eq!(creates[0]["role"], supervisor::ORCHESTRATOR_ROLE);
    assert!(creates[0]["parent"].is_null());
    for cmd in ["send_message", "restart", "wake"] {
        assert!(harness.fake.received_named(cmd).is_empty(), "{cmd}");
    }
    assert!(harness.feed().await.is_empty());
}

#[tokio::test]
async fn tui_attachment_selects_the_successor_without_restarting_it() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let mut old = agent_in(ORCH, "cancelled");
    old["restarted_as"] = json!(ORCH_2);
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            Scripted::reply(
                "agent",
                if cmd["id"] == ORCH {
                    old.clone()
                } else {
                    agent_in(ORCH_2, "running")
                },
            )
        }),
    );
    let Reply::Orchestrator(info) = attach(&harness).await.unwrap() else {
        panic!("expected the successor");
    };
    assert_eq!(info.agent, agent_id(ORCH_2));
    assert_eq!(info.status, "running");
    // Attaching waits for nothing: the old agent's sweep is owed, not made.
    for cmd in ["restart", "create_agent", "send_message", "cancel"] {
        assert!(harness.fake.received_named(cmd).is_empty(), "{cmd}");
    }
    assert_eq!(retired(&harness).await, [owed(ORCH)]);
}

#[tokio::test]
async fn tui_attachment_refuses_mcp_standby_and_disconnection_before_creation() {
    let harness = Harness::new().await;
    let denied = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonMcp,
        Request::OrchestratorAttach,
    )
    .await
    .unwrap_err();
    assert!(matches!(denied, ArchonError::Forbidden(_)), "{denied}");
    let unavailable = attach(&harness).await.unwrap_err();
    assert!(
        matches!(unavailable, ArchonError::Unavailable(_)),
        "{unavailable}"
    );
    harness.ctx.standby.set("disk full".into()).unwrap();
    let standby = attach(&harness).await.unwrap_err();
    assert!(matches!(standby, ArchonError::DiskFull(_)), "{standby}");
    assert!(harness.fake.received_named("create_agent").is_empty());
}

#[tokio::test]
async fn tui_attachment_preserves_revival_backoff() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    failing_orchestrator(&harness);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let error = attach(&harness).await.unwrap_err();
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error}");
    assert!(error.to_string().contains("archon orchestrator restart"));
    assert_eq!(harness.fake.received_named("wake").len(), 1);
    assert!(harness.fake.received_named("restart").is_empty());
}

#[tokio::test]
async fn tui_attachment_does_not_return_an_unknown_or_stopped_snapshot() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    for snapshot in [
        Scripted::refuse("not_found", "vanished during attachment"),
        Scripted::reply("agent", agent_in(ORCH, "cancelled")),
    ] {
        harness.fake.script(
            "get_agent",
            Scripted::reply("agent", agent_in(ORCH, "ready")),
        );
        harness.fake.script("get_agent", snapshot);
        let error = attach(&harness).await.unwrap_err();
        assert!(
            matches!(
                error,
                ArchonError::NotFound(_) | ArchonError::Unavailable(_)
            ),
            "{error}"
        );
    }
    assert!(harness.fake.received_named("create_agent").is_empty());
}

/// `git` in `dir` with a throwaway identity; returns its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Archon Test"])
        .args(["-c", "user.email=test@archon.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(repo: &Path, text: &str) -> String {
    std::fs::write(repo.join("README.md"), text).unwrap();
    git(repo, &["add", "README.md"]);
    git(repo, &["commit", "--quiet", "-m", text]);
    git(repo, &["rev-parse", "HEAD"])
}

/// The commit a mirror has checked out: its `.git` is a file naming the
/// daemon's git directory of it, whose detached `HEAD` holds the commit.
fn mirror_head(mirror: &Path) -> String {
    let gitfile = std::fs::read_to_string(mirror.join(".git")).unwrap_or_default();
    let gitdir = gitfile.trim().strip_prefix("gitdir: ").unwrap_or_default();
    let head = std::fs::read_to_string(Path::new(gitdir).join("HEAD"));
    head.unwrap_or_default().trim().to_owned()
}

/// An operator's turn reads the held routine news, in mirrors refreshed for
/// the question, without waiting for the next waking trigger.
#[tokio::test]
async fn an_operator_message_brings_the_quiet_news_and_a_current_mirror() {
    let mut config = Config::default();
    config.orchestrator.trigger_coalesce_ms = 60_000;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    agent_table(&harness, &[(ORCH, "running")]);
    let sent = Scripted::reply("message_sent", json!({}));
    harness.fake.set_default("send_message", sent);
    harness.fake.set_default("trigger", Scripted::ok());
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path().canonicalize().unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    let first = commit(&repo, "first");
    let item = link_common::cancelled_item(&harness.ctx, &repo).await;
    let mirror = mirror::mirrors_root(&harness.ctx.paths.orchestrator_workspace).join("alpha");
    let delivery = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    let mirrors = tokio::spawn(mirror::run(harness.ctx.clone()));
    let at = |commit: String| {
        let mirror = mirror.clone();
        move || std::future::ready(mirror_head(&mirror) == commit)
    };
    harness
        .eventually("the mirror is made at start", at(first))
        .await;

    let second = commit(&repo, "second");
    let routine = TriggerEvent::ItemFinished {
        item,
        title: "bump a dependency".into(),
        state: "cancelled".into(),
        summary: "cancelled".into(),
        pr_url: None,
        cost_usd: 0.0,
        origin: None,
        lesson_hint: None,
    };
    triggers::push_quiet(&harness.ctx, routine).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(harness.fake.received_named("trigger").is_empty());

    supervisor::deliver_operator_message(&harness.ctx, &FeedItemId::new(), "what changed?")
        .await
        .unwrap();
    let triggers = harness.fake.wait_for("trigger", 1, WAIT).await;
    let events = &triggers[0]["payload"]["events"];
    assert_eq!(events.as_array().map(Vec::len), Some(1), "{events}");
    assert_eq!(events[0]["type"], "items_finished");
    assert_eq!(harness.fake.received_named("send_message").len(), 1);
    harness
        .eventually("the mirror follows the base", at(second))
        .await;
    delivery.abort();
    mirrors.abort();
}

#[tokio::test]
async fn operator_answer_carries_clipped_question_context() {
    let mut harness = Harness::new().await;
    let item = archon_core::id::QueueItemId::new();
    let text = "界".repeat(513);
    let asked = archon_daemon::feed::question(
        &harness.ctx,
        text.clone(),
        vec!["yes".into(), "later".into()],
        Some(item.clone()),
    )
    .await
    .unwrap();
    answer(&harness, &asked.id, "yes").await.unwrap();
    let events = harness.drain_triggers().await;
    assert_eq!(events.len(), 1);
    let event = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(event["question"], json!(asked.id));
    assert_eq!(event["number"], asked.number);
    assert_eq!(event["text"], "yes");
    assert_eq!(event["question_text"], format!("{}…", "界".repeat(511)));
    assert_eq!(event["options"], json!(["yes", "later"]));
    assert_eq!(event["item"], json!(item));
    assert_eq!(event["asked_at"], json!(asked.asked_at));
    assert_eq!(
        harness
            .ctx
            .store
            .lock()
            .await
            .question(&asked.id)
            .unwrap()
            .text,
        text
    );
}
