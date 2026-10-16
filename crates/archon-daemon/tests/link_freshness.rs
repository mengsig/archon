//! Whether the orchestrator runs a stale prompt, tools or lessons: what each
//! generation was created with against what archond would start now.

mod link_common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use archon_aether::fake::{self, Scripted};
use archon_core::config::Config;
use archon_core::lesson::{LessonDraft, LessonKind};
use archon_core::protocol::{ClientKind, OrchestratorInfo, Reply, Request};
use archon_daemon::{handlers, supervisor};
use link_common::Harness;
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";
/// `kv`: what the newest generation was created with.
const ORIGIN_KEY: &str = "orchestrator_origin";

/// `get_agent` answers from a table the test edits as agents change state.
fn agent_table(harness: &Harness) -> Arc<Mutex<HashMap<String, String>>> {
    let states: Arc<Mutex<HashMap<String, String>>> = Arc::default();
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

fn agent_in(id: &str, state: &str) -> Value {
    let mut agent = fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["status"] = json!({ "state": state });
    agent
}

async fn create(harness: &Harness, states: &Mutex<HashMap<String, String>>, id: &str) {
    states.lock().unwrap().insert(id.into(), "ready".into());
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", agent_in(id, "ready")),
    );
    let created = supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(created.as_str(), id);
}

async fn as_operator(harness: &Harness, request: Request) -> Reply {
    let reply = handlers::handle(&harness.ctx, ClientKind::ArchonCli, request).await;
    reply.expect("the operator's request")
}

async fn orchestrator_status(harness: &Harness) -> OrchestratorInfo {
    match as_operator(harness, Request::OrchestratorStatus).await {
        Reply::Orchestrator(info) => info,
        other => panic!("expected the orchestrator, got {other:?}"),
    }
}

/// The orchestrator as `archon status` and `queue_status` see it.
async fn daemon_status(harness: &Harness) -> OrchestratorInfo {
    match as_operator(harness, Request::Status(Default::default())).await {
        Reply::Status(status) => status.orchestrator.expect("an orchestrator"),
        other => panic!("expected the status, got {other:?}"),
    }
}

async fn restart(harness: &Harness, states: &Mutex<HashMap<String, String>>) -> OrchestratorInfo {
    states
        .lock()
        .unwrap()
        .insert(ORCH.into(), "cancelled".into());
    states.lock().unwrap().insert(ORCH_2.into(), "ready".into());
    harness.fake.script("cancel", Scripted::ok());
    let successor = Scripted::reply("agent", agent_in(ORCH_2, "ready"));
    harness.fake.script("create_agent", successor);
    match as_operator(harness, Request::OrchestratorRestart).await {
        Reply::Orchestrator(info) => info,
        other => panic!("expected the orchestrator, got {other:?}"),
    }
}

/// Rewrite the stored origin as an older Archon with another prompt would
/// have recorded it.
async fn as_if_created_by_an_older_build(harness: &Harness) {
    let raw = harness
        .kv(ORIGIN_KEY)
        .await
        .expect("the creation was recorded");
    let mut origin: Value = serde_json::from_str(&raw).unwrap();
    origin["archon_version"] = json!("0.0.1");
    origin["prompt"] = json!("0000000000000000");
    harness.set_kv(ORIGIN_KEY, &origin.to_string()).await;
}

#[tokio::test]
async fn after_an_upgrade_that_changes_the_prompt_status_says_so_until_a_restart() {
    let harness = Harness::new().await;
    harness.connect().await;
    let states = agent_table(&harness);
    create(&harness, &states, ORCH).await;
    let fresh = orchestrator_status(&harness).await;
    assert_eq!(fresh.generation, 1);
    let current = format!(
        "generation 1, created by {}; up to date",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(fresh.describe_freshness(), current);

    as_if_created_by_an_older_build(&harness).await;
    for info in [
        orchestrator_status(&harness).await,
        daemon_status(&harness).await,
    ] {
        let freshness = info.freshness.as_ref().expect("the creation was recorded");
        assert_eq!(freshness.prompt_outdated, Some(true));
        assert!(!freshness.tools_outdated, "the tool set is this build's");
        let line = info.describe_freshness();
        assert_eq!(line, "generation 1, created by 0.0.1; prompt outdated");
    }

    let restarted = restart(&harness, &states).await;
    assert_eq!(restarted.agent.as_str(), ORCH_2);
    assert_eq!(restarted.generation, 2);
    let line = daemon_status(&harness).await.describe_freshness();
    assert_eq!(line, current.replace("generation 1", "generation 2"));
}

#[tokio::test]
async fn lessons_recorded_since_creation_are_counted_until_a_restart() {
    let harness = Harness::new().await;
    harness.connect().await;
    let states = agent_table(&harness);
    create(&harness, &states, ORCH).await;
    for text in ["Run the repo check first.", "Read the brief twice."] {
        let draft = LessonDraft {
            kind: LessonKind::Practice,
            text: text.into(),
            repo: None,
            pipeline: None,
            item: None,
        };
        as_operator(&harness, Request::RecordLesson(draft)).await;
    }
    let stale = daemon_status(&harness).await;
    assert_eq!(stale.freshness.as_ref().unwrap().new_lessons, 2);
    assert!(
        stale.describe_freshness().ends_with("; 2 new lessons"),
        "{}",
        stale.describe_freshness()
    );

    restart(&harness, &states).await;
    let current = daemon_status(&harness).await;
    assert_eq!(current.freshness.as_ref().unwrap().new_lessons, 0);
    assert!(current.describe_freshness().ends_with("; up to date"));
}

#[tokio::test]
async fn an_edited_instructions_file_makes_the_prompt_outdated() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, file) = with_instructions(dir.path(), "Prefer small changes.").await;
    let states = agent_table(&harness);
    create(&harness, &states, ORCH).await;
    let fresh = orchestrator_status(&harness).await.freshness.unwrap();
    assert_eq!(fresh.prompt_outdated, Some(false));

    std::fs::write(&file, "Prefer large changes.").unwrap();
    let edited = orchestrator_status(&harness).await.freshness.unwrap();
    assert_eq!(edited.prompt_outdated, Some(true));
    std::fs::remove_file(&file).unwrap();
    let unreadable = orchestrator_status(&harness).await;
    assert_eq!(unreadable.freshness.as_ref().unwrap().prompt_outdated, None);
    assert!(unreadable.describe_freshness().contains("prompt unknown"));
}

#[tokio::test]
async fn an_orchestrator_created_before_archon_recorded_it_has_no_freshness() {
    let harness = Harness::new().await;
    harness.connect().await;
    let states = agent_table(&harness);
    create(&harness, &states, ORCH).await;
    harness.set_kv(ORIGIN_KEY, "").await;
    let info = orchestrator_status(&harness).await;
    assert_eq!(info.freshness, None);
    assert!(
        info.describe_freshness()
            .starts_with("generation 1, created before Archon recorded its prompt"),
        "{}",
        info.describe_freshness()
    );
}

/// A harness whose `[orchestrator] instructions_file` holds `text`.
async fn with_instructions(dir: &std::path::Path, text: &str) -> (Harness, std::path::PathBuf) {
    let file = dir.join("orchestrator.md");
    std::fs::write(&file, text).unwrap();
    let mut config = Config::default();
    config.orchestrator.instructions_file = Some(file.clone());
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    (harness, file)
}

#[tokio::test]
async fn a_retried_creation_records_the_prompt_it_sent() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, file) = with_instructions(dir.path(), "Prefer small changes.").await;
    let states = agent_table(&harness);
    let refused = Scripted::refuse("internal", "store busy");
    harness.fake.script("create_agent", refused);
    supervisor::ensure_orchestrator(&harness.ctx)
        .await
        .expect_err("the first create is refused");
    std::fs::write(&file, "Prefer large changes.").unwrap();
    create(&harness, &states, ORCH).await;
    let fresh = orchestrator_status(&harness).await.freshness.unwrap();
    assert_eq!(fresh.prompt_outdated, Some(false), "the retry sent today's");

    std::fs::write(&file, "Prefer small changes.").unwrap();
    let reverted = orchestrator_status(&harness).await.freshness.unwrap();
    assert_eq!(reverted.prompt_outdated, Some(true));
}

/// Aether answers a reused creation key with the agent the earlier attempt
/// created, whose instructions are not the ones this attempt sent: here the
/// operator since removed a trailing instruction.
#[tokio::test]
async fn a_replayed_creation_with_older_instructions_is_outdated() {
    let dir = tempfile::tempdir().unwrap();
    let older = "Prefer small changes.\nRun the old check.";
    let (harness, file) = with_instructions(dir.path(), older).await;
    let states = agent_table(&harness);
    states.lock().unwrap().insert(ORCH.into(), "ready".into());
    let built = supervisor::orchestrator_instructions(
        &harness.ctx.config,
        &harness.ctx.paths.orchestrator_workspace,
        true,
    )
    .unwrap();
    std::fs::write(&file, "Prefer small changes.").unwrap();
    let mut replayed = agent_in(ORCH, "ready");
    replayed["config"] = json!({ "instructions": built });
    harness
        .fake
        .script("create_agent", Scripted::reply("agent", replayed));
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let stale = orchestrator_status(&harness).await.freshness.unwrap();
    assert_eq!(stale.prompt_outdated, Some(true));
}
