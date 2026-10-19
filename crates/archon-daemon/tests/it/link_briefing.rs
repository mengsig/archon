//! Every newly created generation receives durable continuity before ordinary news.

use crate::link_common;

use archon_aether::fake::{self, Scripted};
use archon_core::id::{InitiativeId, PipelineRunId, QuestionId, QueueItemId};
use archon_core::notify::{FeedEntry, Question};
use archon_core::queue::QueueState;
use archon_core::run::{GateSubject, OperatorGate, RunState, StageState};
use archon_daemon::{supervisor, triggers};
use archon_queue::Store;
use archon_queue::store::keys;
use link_common::{Harness, WAIT, agent_id, running_stage};
use serde_json::{Value, json};

async fn harness() -> Harness {
    let harness = Harness::new().await;
    harness.connect().await;
    let mut agent = fake::agent("agt_new", None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    harness
        .fake
        .set_default("create_agent", Scripted::reply("agent", agent.clone()));
    harness.fake.set_responder(
        "get_agent",
        std::sync::Arc::new(|cmd| {
            let mut agent = fake::agent(cmd["id"].as_str().unwrap(), None);
            agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
            Scripted::reply("agent", agent)
        }),
    );
    harness.fake.set_default("cancel", Scripted::ok());
    harness
}

fn briefing(harness: &Harness, index: usize) -> String {
    let sent = harness.fake.received_named("trigger");
    let event = &sent[index]["payload"]["events"][0];
    assert_eq!(event["type"], "orchestrator_briefing");
    assert!(serde_json::to_vec(event).unwrap().len() <= 4096);
    event["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn restart_briefs_every_running_item_before_delivering_an_existing_batch() {
    let harness = harness().await;
    let expected = running_items(&harness).await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, "agt_old").await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    let event = triggers::TriggerEvent::Budget {
        scope: "daily".into(),
        spent_usd: 2.0,
        limit_usd: 1.0,
    };
    harness
        .set_kv(
            "trigger_outbox",
            &json!({"in_flight": {
        "key": "old-key", "events": [event]
    }, "pending": []})
            .to_string(),
        )
        .await;
    tokio::time::timeout(WAIT, supervisor::restart_orchestrator(&harness.ctx))
        .await
        .unwrap()
        .unwrap();
    let text = briefing(&harness, 0);
    for (item, stage) in expected {
        assert!(
            text.lines().any(
                |line| line.contains(item.as_str()) && line.contains(&format!("stage={stage}"))
            ),
            "{text}"
        );
    }
    let pump = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    let sent = harness.fake.wait_for("trigger", 2, WAIT).await;
    pump.abort();
    assert_eq!(sent[1]["idempotency_key"], "old-key");
    assert_eq!(sent[1]["payload"]["events"][0]["type"], "budget");
}

#[tokio::test]
async fn failed_briefing_delivery_is_frozen_and_retried_before_adoption() {
    let harness = harness().await;
    harness
        .fake
        .script("trigger", Scripted::refuse("internal", "retry me"));
    assert!(supervisor::ensure_orchestrator(&harness.ctx).await.is_err());
    assert!(harness.kv(keys::ORCHESTRATOR_AGENT).await.is_none());
    harness
        .ctx
        .store
        .lock()
        .await
        .append_feed(
            FeedEntry::Operator {
                text: "later".into(),
            },
            archon_core::now(),
        )
        .unwrap();
    assert_eq!(
        supervisor::ensure_orchestrator(&harness.ctx).await.unwrap(),
        agent_id("agt_new")
    );
    let sent = harness.fake.received_named("trigger");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
    assert_eq!(sent[0]["payload"], sent[1]["payload"]);
    assert!(!briefing(&harness, 1).contains("later"));
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    assert_eq!(
        harness.fake.received_named("trigger").len(),
        2,
        "reuse does not rebrief"
    );
}

#[tokio::test]
async fn briefing_snapshot_lists_questions_items_initiatives_and_chat() {
    let harness = harness().await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let mut store = harness.ctx.store.lock().await;
    let q = ask(&mut store, Some(item.clone()));
    let (initiative, member) = initiative_member(&mut store, &item);
    store
        .append_feed(
            FeedEntry::Operator {
                text: "Please continue".into(),
            },
            archon_core::now(),
        )
        .unwrap();
    drop(store);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let text = briefing(&harness, 0)
        .replace(item.as_str(), "ITEM")
        .replace(initiative.as_str(), "INITIATIVE")
        .replace(member.as_str(), "MEMBER");
    assert_eq!(q.number, 1);
    assert_eq!(
        text,
        concat!(
            "Non-terminal items:\nITEM queued waits=dispatch capacity/pause | link test item\nMEMBER queued waits=dispatch capacity/pause | link test item\n",
            "Finished since last batch:\nnone\n",
            "Open questions and gates:\nQ1 ITEM Which branch?\n",
            "Open initiatives:\nINITIATIVE open Release\n",
            "Recent chat (newest first):\noperator: Please continue\n"
        )
    );
}

#[tokio::test]
async fn finished_items_are_selected_since_the_last_batch_snapshot() {
    let harness = harness().await;
    let old = link_common::cancelled_item(&harness.ctx, harness.dir.path()).await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, "agt_old").await;
    harness.set_kv(keys::ORCHESTRATOR_GENERATION, "1").await;
    let pump = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push_quiet(
        &harness.ctx,
        triggers::TriggerEvent::Budget {
            scope: "daily".into(),
            spent_usd: 2.0,
            limit_usd: 1.0,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, triggers::flush_quiet(&harness.ctx))
        .await
        .unwrap()
        .unwrap();
    pump.abort();
    let recent = link_common::cancelled_item(&harness.ctx, harness.dir.path()).await;
    supervisor::restart_orchestrator(&harness.ctx)
        .await
        .unwrap();
    let text = briefing(&harness, 1);
    assert!(!text.contains(old.as_str()));
    assert!(text.contains(&format!("{recent} cancelled")));
}

#[tokio::test]
async fn briefing_bounds_unicode_and_json_escaping_and_counts_omitted_rows() {
    let harness = harness().await;
    for _ in 0..80 {
        let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
        let mut store = harness.ctx.store.lock().await;
        let mut item = store.item(&item).unwrap();
        item.title = "\"\\界".repeat(200);
        store.update_item(&item).unwrap();
    }
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let text = briefing(&harness, 0);
    let listed = text.lines().filter(|line| line.starts_with("qi_")).count();
    assert!(listed > 0 && listed < 80);
    assert!(text.contains(&format!("+{} more", 80 - listed)), "{text}");
    assert!(text.contains('…'));
    let saved: Value =
        serde_json::from_str(&harness.kv("orchestrator_briefing").await.unwrap()).unwrap();
    assert_eq!(saved["generation"], 1);
}

async fn running_items(harness: &Harness) -> Vec<(QueueItemId, String)> {
    let stage = running_stage(&harness.ctx, harness.dir.path(), "agt_stage", "tsk_stage").await;
    let template = harness.ctx.store.lock().await.run(&stage.run).unwrap();
    let mut expected = vec![(template.item.clone(), stage.stage_id)];
    for n in 1..3 {
        let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
        let mut run = template.clone();
        run.id = PipelineRunId::new();
        run.item = item.clone();
        let stage_id = format!("review{n}");
        run.state = RunState::Running {
            stage_id: stage_id.clone(),
            attempt: 1,
        };
        let mut store = harness.ctx.store.lock().await;
        store
            .transition_item(&item, QueueState::Dispatching, archon_core::now())
            .unwrap();
        store.insert_run(&run).unwrap();
        store
            .transition_item(
                &item,
                QueueState::Running { run: run.id },
                archon_core::now(),
            )
            .unwrap();
        expected.push((item, stage_id));
    }
    assert_eq!(expected.len(), 3);
    assert_eq!(
        harness.ctx.store.lock().await.active_runs().unwrap().len(),
        3
    );
    expected
}

fn ask(store: &mut Store, item: Option<QueueItemId>) -> Question {
    let q = store
        .insert_question(&Question {
            id: QuestionId::new(),
            number: 0,
            text: "Which branch?".into(),
            options: vec![],
            item,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        })
        .unwrap();
    assert!(q.number > 0);
    assert!(q.is_open());
    q
}

fn initiative_member(store: &mut Store, item: &QueueItemId) -> (InitiativeId, QueueItemId) {
    let initiative = archon_core::initiative::Initiative::new(
        "Release",
        "Keep compatible",
        false,
        archon_core::now(),
    )
    .unwrap();
    let mut member = store.item(item).unwrap();
    member.id = QueueItemId::parse("qi_7ZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
    member.initiative = Some(initiative.id.clone());
    let request = serde_json::from_value(json!({"title": "Release", "contract": "Keep compatible",
        "members": [{"repo": "alpha", "brief": "ship it"}]}))
    .unwrap();
    let detail = store
        .insert_initiative(&initiative, &[member.clone()], &request)
        .unwrap();
    assert_eq!(detail.members.len(), 1);
    assert_eq!(detail.initiative.id, initiative.id);
    (initiative.id, member.id)
}

#[tokio::test]
async fn open_gate_names_its_question_item_stage_and_pr() {
    let harness = harness().await;
    let mut stage = running_stage(&harness.ctx, harness.dir.path(), "agt_stage", "tsk_stage").await;
    let mut store = harness.ctx.store.lock().await;
    let mut run = store.run(&stage.run).unwrap();
    let q = ask(&mut store, Some(run.item.clone()));
    stage.state = StageState::Completed;
    stage.finished_at = Some(archon_core::now());
    stage.structured = Some(json!({"pr_url": "https://example.test/pull/7"}));
    store.upsert_stage_run(&stage).unwrap();
    run.state = RunState::AwaitingOperator {
        stage_id: stage.stage_id.clone(),
        attempt: 1,
        subject: GateSubject::Proposal,
    };
    store.update_run(&run).unwrap();
    store
        .open_gate(&OperatorGate {
            run: run.id,
            item: run.item.clone(),
            stage_id: stage.stage_id,
            attempt: 1,
            subject: GateSubject::Proposal,
            summary: "plan".into(),
            page: None,
            location: None,
            report: None,
            question: Some(q.id),
            decision: None,
            withdrawn: None,
            opened_at: archon_core::now(),
            decided_at: None,
        })
        .unwrap();
    drop(store);
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let text = briefing(&harness, 0);
    assert!(
        text.contains(&format!(
            "Q{} {} gate=direction stage=implement",
            q.number, run.item
        )),
        "{text}"
    );
    assert!(
        text.contains("stage=implement waits=operator PR=https://example.test/pull/7"),
        "{text}"
    );
}

#[tokio::test]
async fn a_refused_briefing_does_not_discard_the_batch_waiting_for_creation() {
    let harness = harness().await;
    harness
        .fake
        .script("trigger", Scripted::refuse("invalid_argument", "not ready"));
    let pump = tokio::spawn(triggers::deliver_loop(harness.ctx.clone()));
    triggers::push(
        &harness.ctx,
        triggers::TriggerEvent::Budget {
            scope: "daily".into(),
            spent_usd: 2.0,
            limit_usd: 1.0,
        },
    )
    .await
    .unwrap();
    let sent = harness.fake.wait_for("trigger", 3, WAIT).await;
    pump.abort();
    assert_eq!(sent[0]["payload"], sent[1]["payload"]);
    assert_eq!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
    assert_eq!(sent[2]["payload"]["events"][0]["type"], "budget");
    assert_eq!(harness.fake.received_named("create_agent").len(), 2);
}

#[tokio::test]
async fn a_stopped_unbriefed_agent_is_swept_before_a_fresh_generation_is_created() {
    let harness = harness().await;
    harness.fake.script(
        "create_agent",
        Scripted::reply("agent", fake::agent("agt_dead", None)),
    );
    let mut dead = fake::agent("agt_dead", None);
    dead["status"] = json!({"state": "failed"});
    harness
        .fake
        .script("get_agent", Scripted::reply("agent", dead));
    harness.fake.script(
        "trigger",
        Scripted::refuse("invalid_argument", "agent stopped"),
    );
    let first = tokio::time::timeout(WAIT, supervisor::ensure_orchestrator(&harness.ctx))
        .await
        .unwrap();
    assert!(first.is_err());
    assert!(harness.kv(keys::ORCHESTRATOR_AGENT).await.is_none());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_GENERATION).await.as_deref(),
        Some("2")
    );
    assert_eq!(harness.fake.received_named("cancel").len(), 1);
    assert!(
        harness
            .kv("orchestrator_retired")
            .await
            .unwrap()
            .contains("agt_dead")
    );
    let second = tokio::time::timeout(WAIT, supervisor::ensure_orchestrator(&harness.ctx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second, agent_id("agt_new"));
    let creates = harness.fake.received_named("create_agent");
    assert_ne!(creates[0]["idempotency_key"], creates[1]["idempotency_key"]);
    assert_eq!(
        harness.fake.received_named("trigger")[1]["agent"],
        "agt_new"
    );
    briefing(&harness, 1);
}

#[tokio::test]
async fn a_briefing_without_a_batch_cutoff_prioritizes_the_newest_finished_items() {
    let harness = harness().await;
    let mut items = Vec::new();
    for _ in 0..12 {
        items.push(link_common::cancelled_item(&harness.ctx, harness.dir.path()).await);
    }
    supervisor::ensure_orchestrator(&harness.ctx).await.unwrap();
    let text = briefing(&harness, 0);
    let finished = text.split("Finished since last batch:\n").nth(1).unwrap();
    assert!(
        finished.starts_with(items.last().unwrap().as_str()),
        "{finished}"
    );
    assert!(!finished.contains(items[0].as_str()), "{finished}");
    assert!(finished.contains(" more"));
}

#[tokio::test]
async fn an_unbriefed_generation_is_not_replaced_until_its_subtree_is_cancelled() {
    let harness = harness().await;
    let mut dead = fake::agent("agt_dead", None);
    dead["status"] = json!({"state": "failed"});
    harness
        .fake
        .set_default("create_agent", Scripted::reply("agent", dead.clone()));
    for _ in 0..2 {
        harness
            .fake
            .script("get_agent", Scripted::reply("agent", dead.clone()));
        harness.fake.script(
            "trigger",
            Scripted::refuse("invalid_argument", "agent stopped"),
        );
    }
    harness.fake.script(
        "cancel",
        Scripted::refuse("internal", "cleanup unavailable"),
    );
    let first = tokio::time::timeout(WAIT, supervisor::ensure_orchestrator(&harness.ctx))
        .await
        .unwrap()
        .unwrap_err();
    assert!(first.to_string().contains("cleanup unavailable"));
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_GENERATION).await.as_deref(),
        Some("1")
    );
    assert!(harness.kv("orchestrator_retired").await.is_none());
    let second = tokio::time::timeout(WAIT, supervisor::ensure_orchestrator(&harness.ctx))
        .await
        .unwrap();
    assert!(second.is_err());
    assert_eq!(
        harness.kv(keys::ORCHESTRATOR_GENERATION).await.as_deref(),
        Some("2")
    );
    let creates = harness.fake.received_named("create_agent");
    assert_eq!(creates[0]["idempotency_key"], creates[1]["idempotency_key"]);
    assert_eq!(harness.fake.received_named("cancel").len(), 2);
}
