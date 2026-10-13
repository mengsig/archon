//! Who may change Archon over MCP (ARCHITECTURE §11, §18.4): only the
//! orchestrator of now, judged from the store and ordered with its replacement.

mod link_common;

use std::sync::Arc;
use std::time::Duration;

use archon_aether::fake::{self, Scripted};
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, PauseTarget, Reply, Request};
use archon_daemon::{handlers, supervisor};
use archon_queue::store::keys;
use link_common::{Harness, agent_id, running_stage};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";

fn pause() -> Request {
    Request::Pause(PauseTarget::Global { hard: false })
}

async fn paused(harness: &Harness) -> bool {
    let status = handlers::handle(&harness.ctx, ClientKind::ArchonCli, Request::Status).await;
    let Ok(Reply::Status(status)) = status else {
        panic!("status replied {status:?}")
    };
    status.pause.global
}

/// Why archond refused `caller` the change.
async fn refusal(harness: &Harness, caller: &str) -> String {
    let refused = harness.mcp_as(caller, pause()).await;
    let Err(ArchonError::Forbidden(why)) = refused else {
        panic!("{caller} changed Archon: {refused:?}")
    };
    assert!(why.ends_with("Nothing was changed."), "{why}");
    why
}

fn orchestrator(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

/// The harness is never connected: the answer comes from the store alone,
/// so an Aether that is slow or away neither delays nor decides it.
#[tokio::test]
async fn only_the_stored_orchestrator_changes_archon_and_aether_is_never_asked() {
    let harness = Harness::new().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;

    let other = refusal(&harness, "agt_0404").await;
    assert!(other.contains("does not know agent agt_0404"), "{other}");
    assert!(!paused(&harness).await, "no refused call paused the queue");

    let read = harness.mcp_as("agt_0404", Request::RepoList).await;
    assert!(
        matches!(read, Ok(Reply::Repos(_))),
        "reading is open: {read:?}"
    );
    let unnamed_read = handlers::handle(&harness.ctx, ClientKind::ArchonMcp, Request::Status).await;
    assert!(unnamed_read.is_ok(), "{unnamed_read:?}");

    harness
        .mcp_as(ORCH, pause())
        .await
        .expect("the orchestrator");
    assert!(paused(&harness).await);
    assert!(harness.fake.received().is_empty(), "Aether was not asked");
    let notices = harness.notifications(NotificationKind::Blocker).await;
    assert!(notices.is_empty(), "named calls raise nothing: {notices:?}");
}

/// A call that changes Archon, as an Aether that names no callers sends it.
async fn unnamed(harness: &Harness, request: Request) -> Result<Reply, ArchonError> {
    handlers::handle(&harness.ctx, ClientKind::ArchonMcp, request).await
}

fn orchestrator_without_delegate(id: &str) -> Value {
    let mut agent = orchestrator(id, "running");
    agent["config"]["tools"] = json!({
        "mode": "except",
        "names": ["delegate", "integrate_workspace"],
    });
    agent
}

fn hides_delegate(create: &Value) -> bool {
    let hidden = create["config"]["tools"]["names"].as_array().unwrap();
    hidden.contains(&json!("delegate"))
}

/// The upgrade: Aether names no callers and the orchestrator cannot delegate.
/// Its calls are its own, so Archon keeps working and tells the operator once.
#[tokio::test]
async fn an_install_whose_aether_names_no_callers_keeps_working_without_delegation() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let old = orchestrator_without_delegate(ORCH);
    harness
        .fake
        .set_default("get_agent", Scripted::reply("agent", old));
    harness.fake.set_default("cancel", Scripted::ok());
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();

    unnamed(&harness, pause()).await.expect("its own call");
    assert!(paused(&harness).await);
    let resume = Request::Resume(PauseTarget::Global { hard: false });
    unnamed(&harness, resume).await.expect("and the next");
    let notices = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 1, "told once: {notices:?}");
    assert!(
        notices[0].contains("not naming the agents") && notices[0].contains("Restart Archon's own"),
        "{notices:?}"
    );

    let successor = orchestrator_without_delegate(ORCH_2);
    harness
        .fake
        .set_default("create_agent", Scripted::reply("agent", successor));
    supervisor::restart_orchestrator(&harness.ctx)
        .await
        .unwrap();
    let created = harness.fake.received_named("create_agent");
    assert!(hides_delegate(&created[0]), "{}", created[0]);
    let instructions = created[0]["config"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("## Native delegation is off"));
    unnamed(&harness, pause())
        .await
        .expect("the new one's call");

    harness.mcp_as(ORCH_2, pause()).await.expect("named again");
    let notes = harness.system_notes().await;
    let again: Vec<_> = notes
        .iter()
        .filter(|n| n.contains("names the agents"))
        .collect();
    assert_eq!(again.len(), 1, "{notes:?}");
    assert!(
        again[0].contains("`archon orchestrator restart`"),
        "{again:?}"
    );
    harness
        .mcp_as(ORCH_2, pause())
        .await
        .expect("and stays named");
    assert_eq!(harness.system_notes().await.len(), notes.len(), "said once");
    let running = orchestrator(ORCH_2, "running");
    harness
        .fake
        .set_default("get_agent", Scripted::reply("agent", running));
    supervisor::restart_orchestrator(&harness.ctx)
        .await
        .unwrap();
    let created = harness.fake.received_named("create_agent");
    assert!(!hides_delegate(&created[1]), "{}", created[1]);
}

/// An orchestrator that can delegate has helpers that see Archon's tools: an
/// unnamed call may be theirs, so it is refused and the operator told once.
#[tokio::test]
async fn an_unnamed_call_is_refused_while_the_orchestrator_can_delegate() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let running = orchestrator(ORCH, "running");
    harness
        .fake
        .set_default("get_agent", Scripted::reply("agent", running));
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();

    for _ in 0..2 {
        let refused = unnamed(&harness, pause()).await;
        let Err(ArchonError::Forbidden(why)) = refused else {
            panic!("an unnamed caller changed Archon: {refused:?}")
        };
        assert!(why.contains("did not say which agent"), "{why}");
        assert!(why.contains("Restart Archon's own aetherd"), "{why}");
        assert!(why.contains("`archon orchestrator restart`"), "{why}");
        assert!(why.ends_with("Nothing was changed."), "{why}");
    }
    assert!(!paused(&harness).await);
    let notices = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 1, "told once: {notices:?}");
}

/// Before archond has seen what the orchestrator can do, and when there is
/// none, an unnamed call is nobody's for certain.
#[tokio::test]
async fn an_unnamed_call_is_refused_until_archond_knows_the_orchestrator_cannot_delegate() {
    let harness = Harness::new().await;
    assert!(unnamed(&harness, pause()).await.is_err(), "no orchestrator");
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    assert!(unnamed(&harness, pause()).await.is_err(), "not seen yet");
    assert!(!paused(&harness).await);
    let notices = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(notices.len(), 1, "{notices:?}");
}

/// Aether can wake an orchestrator Archon replaced, and its `archon mcp`
/// still answers it: having been the orchestrator earns nothing.
#[tokio::test]
async fn a_replaced_orchestrator_is_refused_from_its_next_call_and_told_to_stop() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    harness
        .mcp_as(ORCH, pause())
        .await
        .expect("still in charge");
    let resume = Request::Resume(PauseTarget::Global { hard: false });
    harness.mcp_as(ORCH, resume).await.expect("still in charge");

    let mut old = orchestrator(ORCH, "failed");
    old["restarted_as"] = json!(ORCH_2);
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| match cmd["id"].as_str() {
            Some(ORCH) => Scripted::reply("agent", old.clone()),
            _ => Scripted::reply("agent", orchestrator(ORCH_2, "running")),
        }),
    );
    let adopted = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(adopted, agent_id(ORCH_2));

    let replaced = refusal(&harness, ORCH).await;
    assert!(
        replaced.contains("an orchestrator Archon has replaced") && replaced.contains("Stop"),
        "{replaced}"
    );
    assert!(!paused(&harness).await);
    harness
        .mcp_as(ORCH_2, pause())
        .await
        .expect("the successor");
    assert!(paused(&harness).await);
}

/// A refusal names what the caller is, so it stops instead of retrying:
/// only an agent Archon has no record of is told that one retry may help.
#[tokio::test]
async fn every_other_agent_is_told_what_it_is_and_what_to_do_instead() {
    let harness = Harness::new().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let stage = running_stage(&harness.ctx, harness.dir.path(), "agt_0201", "tsk_0201").await;
    let item = harness.ctx.store.lock().await.run(&stage.run).unwrap().item;
    let delegated = serde_json::to_string(&item).unwrap();
    harness.set_kv("link_child_agt_0202", &delegated).await;
    harness.set_kv("link_child_agt_0101", "null").await;

    let stage_agent = refusal(&harness, "agt_0201").await;
    assert!(
        stage_agent.contains(&format!("a stage agent of item {item}"))
            && stage_agent.contains("`message_orchestrator`"),
        "{stage_agent}"
    );
    let stage_child = refusal(&harness, "agt_0202").await;
    assert!(
        stage_child.contains(&format!("delegated part of item {item}"))
            && stage_child.contains("the agent that delegated it to you"),
        "{stage_child}"
    );
    let helper = refusal(&harness, "agt_0101").await;
    assert!(
        helper.contains("one the orchestrator delegated to") && helper.contains("in your result"),
        "{helper}"
    );
    let stranger = refusal(&harness, "agt_0404").await;
    assert!(stranger.contains("call once more"), "{stranger}");
    for told_to_stop in [&stage_agent, &stage_child, &helper] {
        assert!(!told_to_stop.contains("once more"), "{told_to_stop}");
    }
    assert!(!paused(&harness).await);
}

/// Answer `create_agent` with `agent` only once the returned sender is used.
/// For a multi-thread runtime, whose other workers keep its timers going.
fn hold_create_agent(harness: &Harness, agent: Value) -> std::sync::mpsc::Sender<()> {
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    harness.fake.set_responder(
        "create_agent",
        Arc::new(move |_| {
            let wait = || held.lock().unwrap().recv_timeout(Duration::from_secs(10));
            tokio::task::block_in_place(wait).expect("the test releases create_agent");
            Scripted::reply("agent", agent.clone())
        }),
    );
    release
}

/// How long a call is watched to see that it waits for the switch.
const SWITCH_WATCH: Duration = Duration::from_millis(200);

/// The new orchestrator can call before archond has stored its id: the call
/// waits for the switch instead of being judged against a store naming nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_made_while_the_orchestrator_is_being_created_waits_for_the_switch() {
    let harness = Harness::new().await;
    harness.connect().await;
    let release = hold_create_agent(&harness, orchestrator(ORCH, "running"));
    let ctx = harness.ctx.clone();
    let creating = tokio::spawn(async move { supervisor::ensure_orchestrator(&ctx).await });
    harness
        .eventually("the orchestrator is being created", || async {
            !harness.fake.received_named("create_agent").is_empty()
        })
        .await;
    assert_eq!(harness.kv(keys::ORCHESTRATOR_AGENT).await, None);

    let first_call = harness.mcp_as(ORCH, pause());
    tokio::pin!(first_call);
    let early = tokio::time::timeout(SWITCH_WATCH, &mut first_call).await;
    assert!(early.is_err(), "judged before the switch: {early:?}");
    release.send(()).expect("create_agent is waiting");
    let first_call = first_call.await;
    assert!(
        first_call.is_ok(),
        "judged after the switch: {first_call:?}"
    );
    assert_eq!(creating.await.unwrap().unwrap(), agent_id(ORCH));
    assert!(paused(&harness).await);
}

/// The operator's restart replaces the orchestrator while calls arrive:
/// one made during the switch is judged against the orchestrator after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_made_during_a_replacement_is_judged_against_the_new_orchestrator() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let running = orchestrator(ORCH, "running");
    harness
        .fake
        .set_default("get_agent", Scripted::reply("agent", running));
    harness.fake.set_default("cancel", Scripted::ok());
    let release = hold_create_agent(&harness, orchestrator(ORCH_2, "running"));
    let ctx = harness.ctx.clone();
    let restarting = tokio::spawn(async move { supervisor::restart_orchestrator(&ctx).await });
    harness
        .eventually("the replacement is being created", || async {
            !harness.fake.received_named("create_agent").is_empty()
        })
        .await;

    let replaced = refusal(&harness, ORCH);
    tokio::pin!(replaced);
    let early = tokio::time::timeout(SWITCH_WATCH, &mut replaced).await;
    assert!(early.is_err(), "judged before the switch: {early:?}");
    release.send(()).expect("create_agent is waiting");
    let replaced = replaced.await;
    assert!(replaced.contains("Archon has replaced"), "{replaced}");
    assert_eq!(restarting.await.unwrap().unwrap(), agent_id(ORCH_2));
    assert!(
        !paused(&harness).await,
        "the old orchestrator changed nothing"
    );
    harness.mcp_as(ORCH_2, pause()).await.expect("the new one");
}

/// What `archon mcp` forwards names one caller: a request that names
/// another inside, or wraps a second forward, is malformed.
#[tokio::test]
async fn a_forwarded_request_names_exactly_one_caller() {
    let harness = Harness::new().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let ask_as_another = Request::AskOperator {
        text: "Ship?".into(),
        item: None,
        options: vec![],
        caller: Some(agent_id("agt_0404")),
    };
    let mismatched = harness.mcp_as(ORCH, ask_as_another).await;
    assert!(
        matches!(&mismatched, Err(ArchonError::Invalid(why)) if why.contains("agt_0404")),
        "{mismatched:?}"
    );
    let nested = Request::ForAgent {
        caller: agent_id(ORCH),
        request: Box::new(pause()),
    };
    let nested = harness.mcp_as(ORCH, nested).await;
    assert!(matches!(nested, Err(ArchonError::Invalid(_))), "{nested:?}");
    let asked = harness.ctx.store.lock().await.questions(true).unwrap();
    assert!(asked.is_empty() && !paused(&harness).await, "{asked:?}");
}
