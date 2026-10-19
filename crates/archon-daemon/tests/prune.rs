//! Pruning the store's `kv` against a scripted backend: what a
//! finished run leaves behind goes, what a live run, recovery or late
//! Aether events still need stays, and the entries stay bounded as runs
//! accumulate.

mod common;

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use archon_core::id::{QueueItemId, ReportId};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageRun};
use archon_daemon::prune::{self, Pruned};
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{executor, feed, reports};
use common::{Harness, completed, verdict};
use serde_json::json;

/// Every per-stage, per-run and per-child `kv` prefix the daemon writes.
const PREFIXES: &[&str] = &[
    "spawn_request:",
    "run_vars:",
    "team_merge:",
    "run_node:",
    "run_node_agent:",
    "link_child_",
    "link_unchecked_",
    "link_cancelled_",
    "link_ended_",
    "link_stage_agent_",
    "report:",
];

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(serde_json::json!({"pr_url": "https://example.invalid/pr/1"})),
        },
        _ => completed("implemented"),
    }
}

/// Past the retention window of everything finished by now.
fn after_retention() -> Timestamp {
    archon_core::now() + prune::retention() + chrono::Duration::hours(1)
}

async fn keys(h: &Harness, prefix: &str) -> Vec<String> {
    let store = h.ctx.store.lock().await;
    let entries = store.kv_prefix(prefix).expect("kv range");
    entries.into_iter().map(|(key, _)| key).collect()
}

async fn entry_count(h: &Harness) -> usize {
    let mut count = 0;
    for prefix in PREFIXES {
        count += keys(h, prefix).await.len();
    }
    count
}

/// Enqueue an item and pass every stage until it succeeds.
async fn run_to_success(h: &Harness, title: &str) -> QueueItemId {
    let item = h.enqueue(title).await;
    h.dispatch().await;
    for expected in ["implement", "review", "ship"] {
        let settled = h.settle_running(passing).await;
        assert!(
            !settled.is_empty() && settled.iter().all(|s| s.stage_id == expected),
            "{expected}: {settled:?}"
        );
        h.dispatch().await;
    }
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    item.id
}

async fn prune_at(h: &Harness, now: Timestamp) -> Pruned {
    prune::prune_once(&h.ctx, now).await.expect("prune pass")
}

#[tokio::test]
async fn a_finished_runs_entries_go_and_a_live_runs_stay_for_recovery() {
    let mut h = Harness::new().await;
    let done = run_to_success(&h, "finished work").await;
    let done_run = h.run(&done).await;
    let live = h.enqueue("live work").await;
    h.dispatch().await;
    let live_run = h.run(&live.id).await;
    let live_stage = h.running().await.remove(0);
    let spawn_key = format!("spawn_request:{}", live_stage.id);
    assert!(
        keys(&h, "spawn_request:").await.len() > 1,
        "both runs spawned"
    );

    let pruned = prune_at(&h, archon_core::now()).await;
    assert!(pruned.spawn_requests >= 3, "{pruned:?}");
    assert_eq!(pruned.run_vars, 1, "{pruned:?}");
    assert_eq!(keys(&h, "spawn_request:").await, vec![spawn_key]);
    assert_eq!(
        keys(&h, "run_vars:").await,
        vec![format!("run_vars:{}", live_run.id)]
    );
    assert_eq!(
        keys(&h, "run_node_agent:").await.len(),
        2,
        "a retired node is still recognised within the retention"
    );

    let pruned = prune_at(&h, after_retention()).await;
    assert_eq!(pruned.node_agents, 1, "{pruned:?}");
    let nodes = h
        .ctx
        .store
        .lock()
        .await
        .kv_prefix("run_node_agent:")
        .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(
        nodes[0].1,
        live_run.id.as_str(),
        "the live run's node stays"
    );
    assert_ne!(live_run.id, done_run.id);
    assert_eq!(
        keys(&h, "run_node:").await,
        vec![format!("run_node:{}", live_run.id)],
        "the live run keeps its node binding; the finished run's was retired"
    );

    // What recovery needs survived: a restarted daemon finishes the live run.
    h.restart();
    executor::recover(&h.ctx).await.expect("recovers");
    for expected in ["implement", "review", "ship"] {
        let settled = h.settle_running(passing).await;
        assert!(
            !settled.is_empty() && settled.iter().all(|s| s.stage_id == expected),
            "{expected}: {settled:?}"
        );
        h.dispatch().await;
    }
    let state = h.state(&live.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
}

#[tokio::test]
async fn kv_entries_stay_bounded_as_runs_accumulate() {
    let h = Harness::new().await;
    let mut counts = Vec::new();
    for n in 0..4 {
        run_to_success(&h, &format!("work {n}")).await;
        let before = entry_count(&h).await;
        prune_at(&h, after_retention()).await;
        counts.push((before, entry_count(&h).await));
    }
    assert!(counts.iter().all(|(before, _)| *before > 0), "{counts:?}");
    assert!(
        counts.iter().all(|(_, after)| *after == 0),
        "every finished run's entries were pruned: {counts:?}"
    );
}

/// How far the watchdog got with a child before the pass.
#[derive(Clone, Copy)]
enum Verdict {
    /// Its check never finished (Aether was unreachable).
    Unchecked,
    Permitted,
    Cancelled,
}

/// A child entry for `agent` working for `item` (`None`: the orchestrator's),
/// with the markers the watchdog writes for `verdict`.
async fn watch_child(h: &Harness, agent: &str, item: Option<&QueueItemId>, verdict: Verdict) {
    let mut store = h.ctx.store.lock().await;
    let value = serde_json::to_string(&item).unwrap();
    store
        .kv_set(&format!("link_child_{agent}"), &value)
        .unwrap();
    let marker = match verdict {
        Verdict::Unchecked => format!("link_unchecked_{agent}"),
        Verdict::Cancelled => format!("link_cancelled_{agent}"),
        Verdict::Permitted => return,
    };
    store.kv_set(&marker, "1").unwrap();
}

#[tokio::test]
async fn watched_children_are_forgotten_once_their_events_are_over() {
    let h = Harness::new().await;
    let done = run_to_success(&h, "finished work").await;
    let live = h.enqueue("live work").await;
    h.dispatch().await;
    // An agent Aether reports live: the live run's node.
    let live_agent: AetherAgentId = h.backend.node_ids().pop().expect("the live run's node");
    watch_child(&h, "agt_done_child", Some(&done), Verdict::Cancelled).await;
    watch_child(&h, "agt_done_unchecked", Some(&done), Verdict::Unchecked).await;
    watch_child(&h, "agt_live_child", Some(&live.id), Verdict::Unchecked).await;
    watch_child(&h, "agt_ended_orphan", None, Verdict::Permitted).await;
    watch_child(&h, live_agent.as_str(), None, Verdict::Permitted).await;

    let pruned = prune_at(&h, archon_core::now()).await;
    assert_eq!(pruned.children, 0, "all within the retention");
    assert_eq!(
        keys(&h, "link_ended_").await,
        vec!["link_ended_agt_ended_orphan".to_owned()],
        "the ended orchestrator child's end is recorded; the live one's is not"
    );

    let pruned = prune_at(&h, after_retention()).await;
    assert_eq!(pruned.children, 3, "{pruned:?}");
    let mut left = keys(&h, "link_child_").await;
    left.sort();
    let mut expected = vec![
        format!("link_child_{live_agent}"),
        "link_child_agt_live_child".to_owned(),
    ];
    expected.sort();
    assert_eq!(left, expected);
    assert!(keys(&h, "link_cancelled_").await.is_empty());
    assert!(keys(&h, "link_ended_").await.is_empty());
    assert_eq!(
        keys(&h, "link_unchecked_").await,
        vec!["link_unchecked_agt_live_child".to_owned()],
        "a live item's child still pending its verdict is checked again on replay"
    );
}

#[tokio::test]
async fn stage_agents_recorded_at_creation_go_with_their_items() {
    let h = Harness::new().await;
    let done = run_to_success(&h, "finished work").await;
    let live = h.enqueue("live work").await;
    h.dispatch().await;
    {
        let mut store = h.ctx.store.lock().await;
        for (agent, item) in [
            ("agt_done_stage", &done),
            ("agt_live_stage", &live.id),
            ("agt_gone_stage", &QueueItemId::new()),
        ] {
            store
                .kv_set(&format!("link_stage_agent_{agent}"), item.as_str())
                .unwrap();
        }
    }

    let pruned = prune_at(&h, archon_core::now()).await;
    assert_eq!(
        pruned.stage_agents, 1,
        "only the vanished item's: {pruned:?}"
    );
    assert_eq!(keys(&h, "link_stage_agent_").await.len(), 2);

    let pruned = prune_at(&h, after_retention()).await;
    assert_eq!(pruned.stage_agents, 1, "{pruned:?}");
    assert_eq!(
        keys(&h, "link_stage_agent_").await,
        vec!["link_stage_agent_agt_live_stage".to_owned()],
        "a live item's stage agent still places its children"
    );
}

#[tokio::test]
async fn keys_of_the_retired_give_up_rule_go_and_live_supervisor_state_stays() {
    let h = Harness::new().await;
    {
        let mut store = h.ctx.store.lock().await;
        for (key, value) in [
            ("orchestrator_gave_up", "1"),
            ("orchestrator_failures", "[]"),
            ("host_context:old-key", "\"lean\""),
            ("agent_host_context:old-agent", "lean"),
            ("orchestrator_host_context", "\"lean\""),
            ("orchestrator_backoff", r#"{"revivals":0,"last":null}"#),
            ("trigger_outbox", "{}"),
        ] {
            store.kv_set(key, value).unwrap();
        }
    }

    let pruned = prune_at(&h, archon_core::now()).await;
    assert_eq!(pruned.retired_keys, 4, "{pruned:?}");
    assert_eq!(h.kv("orchestrator_gave_up").await, None);
    assert_eq!(h.kv("orchestrator_failures").await, None);
    assert!(h.kv("host_context:old-key").await.is_none());
    assert!(h.kv("agent_host_context:old-agent").await.is_none());
    assert_eq!(
        h.kv("orchestrator_host_context").await.as_deref(),
        Some("\"lean\"")
    );
    assert!(h.kv("orchestrator_backoff").await.is_some());
    assert!(h.kv("trigger_outbox").await.is_some(), "held news is kept");
    assert_eq!(prune_at(&h, after_retention()).await.retired_keys, 0);
}

/// A replaced orchestrator stays on record while its late events can still
/// arrive, and for as long as the sweep of what it left running is owed.
#[tokio::test]
async fn replaced_orchestrators_go_after_the_retention_unless_their_sweep_is_owed() {
    let h = Harness::new().await;
    let now = archon_core::now();
    let long_ago = now - prune::retention() - chrono::Duration::hours(1);
    let record = json!([
        {"agent": "agt_0901", "at": long_ago, "owed": false},
        {"agent": "agt_0902", "at": long_ago, "owed": true},
        {"agent": "agt_0903", "at": now, "owed": false},
    ]);
    let stored = record.to_string();
    h.ctx
        .store
        .lock()
        .await
        .kv_set("orchestrator_retired", &stored)
        .unwrap();

    let pruned = prune_at(&h, now).await;
    assert_eq!(pruned.retired_orchestrators, 1, "{pruned:?}");
    let kept = h.kv("orchestrator_retired").await.expect("still a record");
    assert!(
        !kept.contains("agt_0901"),
        "swept and past retention: {kept}"
    );
    assert!(kept.contains("agt_0902"), "its sweep is owed: {kept}");
    assert!(
        kept.contains("agt_0903"),
        "its events may still come: {kept}"
    );
    assert_eq!(prune_at(&h, now).await.retired_orchestrators, 0);
}

#[tokio::test]
async fn settled_reports_go_after_the_retention_and_the_legacy_blob_is_split() {
    let h = Harness::new().await;
    let legacy = r#"{"rpt_01ARZ3NDEKTSV4RRFFQ69G5FAV":{"html":"/r/old.html","title":"Old","item":null,"run":null,"state":"ended"}}"#;
    h.ctx
        .store
        .lock()
        .await
        .kv_set(reports::LEGACY_KEY, legacy)
        .unwrap();
    let pages = h.dir.path().join("pages");
    std::fs::create_dir(&pages).unwrap();
    for n in 0..3 {
        let html = pages.join(format!("report-{n}.html"));
        std::fs::write(&html, "<p>findings</p>").unwrap();
        let html = html.canonicalize().unwrap();
        let published = reports::publish(&h.ctx, "Findings", &html, None)
            .await
            .expect("published");
        assert!(!published.published, "no Lavish: shown as a path, settled");
    }
    assert_eq!(h.kv(reports::LEGACY_KEY).await, None, "split on first use");
    assert_eq!(keys(&h, reports::KEY_PREFIX).await.len(), 4);

    assert_eq!(prune_at(&h, archon_core::now()).await.reports, 0);
    assert_eq!(prune_at(&h, after_retention()).await.reports, 4);
    assert!(keys(&h, reports::KEY_PREFIX).await.is_empty());
}

/// A disconnected or ended report stays listed, so `archon reports
/// reopen` can show it, while its file exists; then the retention applies.
#[tokio::test]
async fn resumable_reports_are_kept_while_their_file_exists() {
    let h = Harness::new().await;
    let html = h.dir.path().join("plan.html");
    std::fs::write(&html, "<p>plan</p>").unwrap();
    let html = html.canonicalize().unwrap();
    let mut keys = Vec::new();
    for state in ["disconnected", "ended", "stopped"] {
        let key = format!("{}{}", reports::KEY_PREFIX, ReportId::new());
        let record = json!({
            "html": html,
            "title": "Plan",
            "item": null,
            "run": null,
            "state": state,
            "updated_at": archon_core::now(),
        });
        h.ctx
            .store
            .lock()
            .await
            .kv_set(&key, &record.to_string())
            .unwrap();
        keys.push(key);
    }

    assert_eq!(prune_at(&h, after_retention()).await.reports, 1, "stopped");
    assert_eq!(h.kv(&keys[2]).await, None);
    assert!(h.kv(&keys[0]).await.is_some() && h.kv(&keys[1]).await.is_some());
    let Ok(Reply::Reports(listed)) = h.cli(Request::ListReports).await else {
        panic!("expected the reports");
    };
    assert_eq!(listed.len(), 2, "{listed:?}");

    std::fs::remove_file(&html).unwrap();
    assert_eq!(prune_at(&h, archon_core::now()).await.reports, 0, "recent");
    assert_eq!(
        prune_at(&h, after_retention()).await.reports,
        2,
        "file gone"
    );
}

/// Supervisor, delivery and cursor state kept across restarts,
/// none of it tied to a run, with a value each.
const LIVE_STATE: &[(&str, &str)] = &[
    ("orchestrator_agent", "agt_orchestrator"),
    ("orchestrator_generation", "2"),
    ("orchestrator_restarts", "1"),
    ("orchestrator_blocker", "The orchestrator stopped."),
    ("orchestrator_backoff", r#"{"revivals":1,"last":null}"#),
    ("store_id", "st_prune"),
    ("trigger_seq", "7"),
    ("aether_cursor_orchestrator", "41"),
    ("aether_cursor_fleet", "42"),
    ("aether_instance_id", "inst_1"),
];

/// News keyed for delivery and news pushed since outlive every pass,
/// as does the state a restarted daemon resumes from, while the finished
/// run's own entries go.
#[tokio::test]
async fn held_news_and_supervisor_state_survive_every_pass() {
    let h = Harness::new().await;
    let blocked = TriggerEvent::ItemBlocked {
        item: QueueItemId::new(),
        reason: "dependency failed".into(),
    };
    let outbox = json!({
        "in_flight": {"key": "trg_st_prune_7_7", "events": [blocked], "directives": []},
        "pending": [],
    });
    {
        let mut store = h.ctx.store.lock().await;
        store.kv_set("trigger_outbox", &outbox.to_string()).unwrap();
        for (key, value) in LIVE_STATE {
            store.kv_set(key, value).unwrap();
        }
    }
    let done = run_to_success(&h, "finished work").await;
    let held = triggers::held(&h.ctx).await.unwrap();
    assert_eq!(held.len(), 2, "keyed, then the run's own news: {held:?}");
    assert!(
        matches!(&held[1], TriggerEvent::ItemFinished { item, .. } if *item == done),
        "{held:?}"
    );
    assert!(entry_count(&h).await > 0, "the run left entries");

    prune_at(&h, archon_core::now()).await;
    prune_at(&h, after_retention()).await;
    assert_eq!(entry_count(&h).await, 0, "the finished run's entries went");
    assert_eq!(triggers::held(&h.ctx).await.unwrap(), held, "no news lost");
    for (key, value) in LIVE_STATE {
        assert_eq!(h.kv(key).await.as_deref(), Some(*value), "{key}");
    }
}

/// A finished run whose node still waits for a stage agent to end its turn
/// keeps its binding, and the node stays recognised, until it is retired.
#[tokio::test]
async fn a_node_awaiting_retirement_is_kept_past_the_retention() {
    let h = Harness::new().await;
    let item = h.enqueue("slow ending").await;
    h.dispatch().await;
    for expected in ["implement", "review"] {
        let settled = h.settle_running(passing).await;
        assert!(
            !settled.is_empty() && settled.iter().all(|s| s.stage_id == expected),
            "{settled:?}"
        );
        h.dispatch().await;
    }
    let ship = h.running().await.remove(0);
    let agent = ship.aether_agent.clone().expect("a spawned agent");
    h.backend.hold_turn(&agent);
    h.settle_running(passing).await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let run = h.run(&item.id).await;
    let binding = format!("run_node:{}", run.id);

    let pruned = prune_at(&h, after_retention()).await;
    assert_eq!(pruned.node_agents, 0, "{pruned:?}");
    assert!(h.kv(&binding).await.is_some(), "the binding is kept");
    assert_eq!(keys(&h, "run_node_agent:").await.len(), 1);

    h.backend.end_turn(&agent);
    h.dispatch().await;
    assert_eq!(h.kv(&binding).await, None, "retired");
    let pruned = prune_at(&h, after_retention()).await;
    assert_eq!(pruned.node_agents, 1, "{pruned:?}");
    assert!(keys(&h, "run_node_agent:").await.is_empty());
}

/// A report whose "Reopen it?" question waits for the operator is kept
/// however old, so the answer still reaches it; answered, it goes.
#[tokio::test]
async fn a_report_awaiting_its_reopen_answer_is_kept_until_answered() {
    let h = Harness::new().await;
    let question = feed::question(
        &h.ctx,
        "The review window was closed. Reopen it?".into(),
        vec!["Reopen".into(), "leave it closed".into()],
        None,
    )
    .await
    .unwrap();
    let key = format!("{}{}", reports::KEY_PREFIX, ReportId::new());
    let record = json!({
        "html": h.dir.path().join("report.html"),
        "title": "Findings",
        "item": null,
        "run": null,
        "state": "disconnected",
        "reopen_question": question.id,
        "updated_at": archon_core::now(),
    });
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &record.to_string())
        .unwrap();

    assert_eq!(prune_at(&h, after_retention()).await.reports, 0);
    assert!(
        h.kv(&key).await.is_some(),
        "the pending answer's report stays"
    );

    let answer = Request::Answer {
        chat: false,
        question: question.id.clone(),
        text: "leave it closed".into(),
    };
    h.cli(answer).await.expect("the answer is delivered");
    let asked = h.ctx.store.lock().await.question(&question.id).unwrap();
    assert!(!asked.is_open(), "delivered, then recorded");
    let settled: serde_json::Value = serde_json::from_str(&h.kv(&key).await.unwrap()).unwrap();
    assert_eq!(settled["reopen_question"], serde_json::Value::Null);

    assert_eq!(prune_at(&h, after_retention()).await.reports, 1);
    assert_eq!(h.kv(&key).await, None);
}
