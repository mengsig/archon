//! The engine-login hold (ARCHITECTURE §7.3): a stage agent that fails on its
//! engine's login holds that engine with one `Blocker`, its stage waits
//! without spending an infra retry, and once a probe finds the engine logged
//! in the same agents are woken, each once. Other engines go on.

use crate::common;

use std::time::Instant;

use archon_core::model_evidence::ModelEvidenceQuery;
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{StageOutcome, StageState};
use common::Harness;

const ENGINE: &str = "claude-code";
const LOGIN: &str = "model error from claude-code/claude-sonnet-5: Auth: OAuth token has expired";

fn login_failed(_: &archon_core::run::StageRun) -> StageOutcome {
    StageOutcome::Failed {
        error: LOGIN.into(),
        infra: true,
    }
}

/// System lines of the feed, oldest first.
async fn system_lines(h: &Harness) -> Vec<String> {
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    feed.into_iter()
        .filter_map(|item| match item.entry {
            FeedEntry::System { text } => Some(text),
            _ => None,
        })
        .collect()
}

/// Two running items whose agents then fail on their engine's login, which
/// stays logged out; returns their agents.
async fn two_failed_on_login(h: &Harness) -> Vec<archon_core::aether_ids::AetherAgentId> {
    h.enqueue("first item").await;
    h.enqueue("second item").await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 2, "both items run");
    let agents: Vec<_> = running
        .iter()
        .map(|s| s.aether_agent.clone().expect("an agent"))
        .collect();
    for agent in &agents {
        h.backend.fail_agent(agent);
    }
    h.backend.log_out(ENGINE, "claude-code is not logged in");
    h.settle_running(login_failed).await;
    agents
}

#[tokio::test]
async fn a_login_failure_holds_its_engine_and_wakes_the_same_agents_once_it_works() {
    let h = Harness::new().await;
    let agents = two_failed_on_login(&h).await;
    h.dispatch().await;
    h.dispatch().await;

    assert!(h.backend.woken().is_empty(), "no wake while logged out");
    assert_eq!(h.backend.requests().len(), 2, "no fresh agent either");
    assert!(h.backend.login_probes().iter().all(|p| p == ENGINE));
    assert!(!h.backend.login_probes().is_empty(), "the engine is probed");
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "one for the engine: {blockers:?}");
    assert!(blockers[0].contains("Engine `claude-code` cannot log in"));
    assert!(blockers[0].contains("OAuth token has expired"));
    let waiting = h.live_stages().await;
    assert_eq!(waiting.len(), 2, "each stage waits: {waiting:?}");
    assert!(waiting.iter().all(|s| s.state == StageState::Spawning));
    let progress = h.notices(NotificationKind::Progress).await;
    assert!(
        progress.iter().any(|p| p.contains("no retry spent")),
        "{progress:?}"
    );

    h.backend.log_in(ENGINE);
    h.dispatch().await;
    let mut woken = h.backend.woken();
    woken.sort();
    let mut expected = agents.clone();
    expected.sort();
    assert_eq!(woken, expected, "the same agents, each once");
    assert_eq!(h.backend.requests().len(), 2, "woken, not replaced");
    assert!(!h.ctx.engines.is_held(ENGINE));
    let running = h.running().await;
    assert_eq!(running.len(), 2);
    for stage in &running {
        let told = h.backend.resumed_with(stage);
        assert!(told.contains("Auth: OAuth token has expired"), "{told}");
        assert_eq!(told.matches("Archon now stops you").count(), 1, "{told}");
    }
    let lifted = system_lines(&h).await;
    let lines = lifted.iter().filter(|l| l.contains("logs in again"));
    assert_eq!(lines.count(), 1, "{lifted:?}");

    h.dispatch().await;
    assert_eq!(h.backend.woken().len(), 2, "no duplicate wake");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
}

#[tokio::test]
async fn a_login_failure_spends_no_infra_retry_and_is_no_model_evidence() {
    let h = Harness::new().await;
    let item = h.enqueue("fails on its login twice").await;
    h.dispatch().await;
    for _ in 0..2 {
        let agent = h.running().await[0].aether_agent.clone().unwrap();
        h.backend.fail_agent(&agent);
        h.settle_running(login_failed).await;
        h.dispatch().await;
    }
    // `infra_retries = 1`: an infrastructure failure would have ended it.
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Running { .. }), "{state:?}");
    assert_eq!(h.backend.woken().len(), 2, "woken after each failure");
    assert_eq!(h.backend.requests().len(), 1, "one agent throughout");
    let blockers = h.notices(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "a relapse is not news: {blockers:?}");

    let reply = h
        .cli(Request::ModelEvidence(ModelEvidenceQuery::default()))
        .await
        .unwrap();
    let Reply::ModelEvidence(report) = reply else {
        panic!("evidence reply");
    };
    let row = report.rows.iter().find(|r| r.stage == "implement").unwrap();
    assert_eq!(row.attempts, 1, "only the running attempt counts");
}

#[tokio::test]
async fn a_failure_that_is_not_a_login_holds_no_engine() {
    let h = Harness::new().await;
    h.enqueue("engine died").await;
    h.dispatch().await;
    let agent = h.running().await[0].aether_agent.clone().unwrap();
    h.backend.fail_agent(&agent);
    h.settle_running(|_| StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    })
    .await;
    h.dispatch().await;
    assert!(!h.ctx.engines.any_held());
    assert!(h.backend.login_probes().is_empty());
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
    assert_eq!(h.backend.woken(), [agent], "the usual infra wake");
}

#[tokio::test]
async fn a_held_engine_takes_no_new_agent_while_another_engine_goes_on() {
    let h = Harness::new().await;
    h.ctx.engines.report("codex", LOGIN, Instant::now());
    h.backend.log_out("codex", "codex is not logged in");
    h.enqueue("runs on claude-code").await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1, "another engine is held");

    h.ctx.engines.report(ENGINE, LOGIN, Instant::now());
    h.backend.log_out(ENGINE, "claude-code is not logged in");
    h.enqueue("waits for claude-code").await;
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1, "no agent on a held engine");
    let spawning = h.live_stages().await;
    assert!(spawning.iter().any(|s| s.state == StageState::Spawning));

    h.backend.log_in(ENGINE);
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 2, "it starts once logged in");
    assert!(h.ctx.engines.is_held("codex"), "still out");
}

#[tokio::test]
async fn a_restart_keeps_holding_the_engine_without_a_second_blocker() {
    let mut h = Harness::new().await;
    let agents = two_failed_on_login(&h).await;
    h.dispatch().await;
    assert!(h.backend.woken().is_empty());

    h.restart();
    assert!(h.ctx.engines.is_held(ENGINE), "restored from the store");
    h.dispatch().await;
    assert!(h.backend.woken().is_empty(), "still held after the restart");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);

    h.backend.log_in(ENGINE);
    h.dispatch().await;
    assert_eq!(h.backend.woken().len(), agents.len(), "each woken once");
    assert_eq!(h.kv(archon_daemon::engine_login::HOLDS_KEY).await, None);
}
