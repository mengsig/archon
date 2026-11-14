//! When a tree counts as archived for good: its mark is dropped only once
//! its run's node retired, so the retirement never owes it a second time,
//! and it is listed only once its stage agent stopped, so no agent that one
//! delegates to meanwhile is left in view.

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::ports::DescendantState;
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::steer::{Steer, Then};
use archon_daemon::executor::archive::{self, CALL_LIMIT, Progress};
use tokio::time::Instant;

use super::worker::{pass, promptly};
use super::{engine_died, lead_agent, owed_tree};
use crate::common::{self, Harness, MODEL};

/// A live run whose first agent a cold retry's fresh one replaced: the item,
/// the replaced attempt's owed mark and its agent.
async fn replaced_attempt(h: &Harness) -> (QueueItem, String, AetherAgentId) {
    let item = h.enqueue("flaky engine").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    for _ in 0..2 {
        h.backend.fail_agent(&agent);
        h.settle_running(|_| engine_died()).await;
        h.dispatch().await;
    }
    assert_ne!(lead_agent(&h.running().await[0]), agent, "a cold retry");
    let woken_attempt = h.stages(&item.id).await.remove(1);
    let mark = format!("archive:{}", woken_attempt.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    (item, mark, agent)
}

/// Settle what runs, passing every review, until `item`'s run succeeded
/// with its last stage agent still ending its turn, which keeps the run's
/// node from retiring; returns that agent.
async fn finish_with_the_node_kept(h: &Harness, item: &QueueItem) -> AetherAgentId {
    for _ in 0..12 {
        let last = lead_agent(&h.running().await[0]);
        h.backend.hold_turn(&last);
        h.settle_running(|stage| match stage.stage_id.as_str() {
            "review" => common::verdict(true),
            _ => common::completed("done"),
        })
        .await;
        h.dispatch().await;
        if matches!(h.state(&item.id).await, QueueState::Succeeded { .. }) {
            return last;
        }
        h.backend.end_turn(&last);
    }
    panic!("the run never ended");
}

/// An archive of a replaced agent, paused inside Aether while its run
/// finishes, answers before the run's node retires or only after it: either
/// way the tree is archived once and nothing is owed in the end.
async fn archived_once_around_retirement(answered_before_retirement: bool) {
    let h = Harness::new().await;
    let (item, mark, agent) = replaced_attempt(&h).await;
    let gate = h.backend.hold_archive(&agent);
    let mut progress = Progress::default();
    let mut paused = std::pin::pin!(archive::pass(&h.ctx, &mut progress));
    let asked = async {
        while h.backend.archive_calls().is_empty() {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        biased;
        ended = &mut paused => panic!("the pass is held in its call: {ended:?}"),
        () = promptly("the pass's archive call", asked) => {}
    }

    let last = finish_with_the_node_kept(&h, &item).await;
    let node = format!("run_node:{}", h.run(&item.id).await.id);
    assert!(h.kv(&node).await.is_some(), "the node waits for the turn");
    if answered_before_retirement {
        gate.add_permits(1);
        (&mut paused).await.expect("archive pass");
        assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
        let kept = h.kv(&mark).await;
        assert_eq!(kept.as_deref(), Some("done"), "kept until the node retires");
    }
    h.backend.end_turn(&last);
    h.dispatch().await;
    assert_eq!(h.kv(&node).await, None, "the node retired");
    if !answered_before_retirement {
        assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
        gate.add_permits(1);
        paused.await.expect("archive pass");
    }
    assert_eq!(h.kv(&mark).await, None, "nothing is owed a second time");

    h.archive().await;
    assert_eq!(h.backend.archived(), [agent], "archived once");
    assert_eq!(h.kv(&mark).await, None);
}

#[tokio::test]
async fn a_tree_archived_between_its_run_s_end_and_its_node_s_retirement_is_archived_once() {
    archived_once_around_retirement(true).await;
}

#[tokio::test]
async fn a_tree_whose_archive_outlasts_its_node_s_retirement_is_archived_once() {
    archived_once_around_retirement(false).await;
}

/// Two retirements of one run overlap: the first is held in its check that
/// the node stopped while the second retires the node and a pass archives
/// and forgets the tree. The first then owes nothing, so no second archive.
#[tokio::test]
async fn a_retirement_overtaken_by_another_and_a_pass_owes_no_second_archive() {
    let h = Harness::new().await;
    let (item, mark, agent) = replaced_attempt(&h).await;
    let last = finish_with_the_node_kept(&h, &item).await;
    let node = format!("run_node:{}", h.run(&item.id).await.id);
    let stored = h.kv(&node).await.expect("the node waits for the turn");
    let stored: serde_json::Value = serde_json::from_str(&stored).expect("node entry");
    let node_agent = AetherAgentId::parse(stored["agent"].as_str().expect("node agent")).unwrap();
    h.backend.end_turn(&last);
    let checks = h.backend.descendant_checks().len();
    let gate = h.backend.hold_descendants_after(0);
    let mut stale = std::pin::pin!(h.dispatch());
    let held = async {
        while h.backend.descendant_checks().len() == checks {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        biased;
        () = &mut stale => panic!("the first retirement is held in its check"),
        () = promptly("the first retirement's check", held) => {}
    }
    let asked = h.backend.descendant_checks().last().cloned();
    assert_eq!(asked, Some(node_agent), "held checking the node stopped");

    promptly("the second retirement", h.dispatch()).await;
    assert_eq!(h.kv(&node).await, None, "the node retired");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    h.archive().await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
    assert_eq!(h.kv(&mark).await, None, "archived for good");

    gate.add_permits(1);
    promptly("the first retirement", stale).await;
    assert_eq!(h.kv(&mark).await, None, "nothing is owed a second time");
    h.archive().await;
    assert_eq!(h.backend.archived(), [agent], "archived once");
}

/// A stage agent a fresh one replaced after an abort whose cancel Aether
/// only accepted: the agent runs on. Its tree is neither listed nor asked
/// of, so the delegate it makes while a pass would have been archiving is
/// archived with the tree once the agent stopped, not left in view.
#[tokio::test(start_paused = true)]
async fn a_delegate_made_while_the_stage_agent_still_ran_is_archived_with_its_tree() {
    let h = Harness::new().await;
    let item = h.enqueue("gone astray").await;
    h.dispatch().await;
    let aborted = h.running().await.remove(0);
    let agent = lead_agent(&aborted);
    let worker = h.backend.delegate(&agent, MODEL);
    h.backend.end_delegated(&worker, DescendantState::Stopped);
    h.backend.cancel_slowly(&agent);
    let abort = Steer {
        abort: Some(Then::Retry),
        ..Steer::default()
    };
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: abort,
    };
    h.cli(steer).await.expect("aborted");
    h.dispatch().await;
    assert_ne!(lead_agent(&h.running().await[0]), agent, "a fresh agent");
    let mark = format!("archive:{}", aborted.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    let listings = h.backend.descendant_checks().len();
    h.backend.slow_archives(Duration::from_secs(1));

    let mut progress = Progress::default();
    let still_running = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let late = h.backend.delegate(&agent, MODEL);
        h.backend.end_delegated(&late, DescendantState::Stopped);
        h.backend.kill(&agent);
        late
    };
    let ((), late) = tokio::join!(pass(&h, &mut progress), still_running);
    let owed = h.kv(&mark).await;
    assert_eq!(owed.as_deref(), Some("owed"), "its tree could still grow");
    assert!(h.backend.archive_calls().is_empty(), "no agent asked of");
    assert_eq!(h.backend.descendant_checks().len(), listings, "nor listed");

    pass(&h, &mut progress).await;
    let mut archived = h.backend.archived();
    assert_eq!(archived.pop(), Some(agent), "the stage agent last");
    archived.sort();
    let mut delegates = vec![worker, late];
    delegates.sort();
    assert_eq!(archived, delegates, "the late delegate too");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("done"));
}

/// Aether does not say in time whether the stage agent stopped: the pass
/// gives the call up at its limit, the tree is neither listed nor asked of,
/// the mark stays owed, and a later pass archives the tree.
#[tokio::test(start_paused = true)]
async fn a_stage_agent_of_unknown_state_times_out_and_keeps_its_tree_owed() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let calls = h.backend.archive_calls().len();
    let listings = h.backend.descendant_checks().len();
    let gate = h.backend.hold_live_check(&agent);
    let mut progress = Progress::default();
    let began = Instant::now();
    pass(&h, &mut progress).await;
    assert_eq!(began.elapsed(), CALL_LIMIT, "one call's limit, no more");
    assert_eq!(h.backend.archive_calls().len(), calls, "none asked");
    assert_eq!(h.backend.descendant_checks().len(), listings, "nor listed");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    gate.add_permits(1);
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archived(), [worker, agent]);
    assert_eq!(h.kv(&mark).await, None);
}
