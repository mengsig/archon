//! The archive worker's own limits: dispatch and settlement never wait on
//! it, a pass ends within its budget whatever Aether does, what it did not
//! reach stays owed for a later pass, and the worker stops and starts
//! without losing a mark.

use std::future::Future;
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::DescendantState;
use archon_core::queue::QueueState;
use archon_daemon::executor::archive::{self, CALL_LIMIT, PASS_BUDGET, Progress, REFUSED_RETRY};
use tokio::time::Instant;

use super::{engine_died, failed, lead_agent, owed_tree};
use crate::common::{self, Harness, MODEL};

/// Real time a step that must not wait on Aether may take before the test fails.
const PROMPT: Duration = Duration::from_secs(20);
/// A wait no test outlasts, so only a new mark wakes a worker given it.
const NEVER: Duration = Duration::from_secs(3600);

/// One pass over `progress`, which must end by its own limits: under the
/// paused clock a pass that hangs fails here at once.
async fn pass(h: &Harness, progress: &mut Progress) {
    let ended = tokio::time::timeout(NEVER, archive::pass(&h.ctx, progress)).await;
    ended
        .expect("the pass ends by itself")
        .expect("archive pass");
}

/// Wait in real time until `done` holds; a worker that never gets there
/// fails the test within [`PROMPT`].
async fn eventually(what: &str, done: impl AsyncFn() -> bool) {
    let waited = async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let reached = tokio::time::timeout(PROMPT, waited).await;
    assert!(reached.is_ok(), "{what} within {PROMPT:?}");
}

/// `work`, which must not wait on Aether's archiving.
async fn promptly<T>(what: &str, work: impl Future<Output = T>) -> T {
    let done = tokio::time::timeout(PROMPT, work).await;
    done.unwrap_or_else(|_| panic!("{what} waited on archiving"))
}

/// `count` failed runs of one stage agent each, their archives owed and not
/// asked of yet: `(mark key, stage agent)`, in the order a pass takes them.
async fn owed_agents(h: &Harness, count: usize) -> Vec<(String, AetherAgentId)> {
    for n in 0..count {
        h.enqueue(&format!("failing build {n}")).await;
    }
    h.dispatch().await;
    let mut running = h.running().await;
    assert_eq!(running.len(), count, "they run at once");
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    running.sort_by(|a, b| a.id.cmp(&b.id));
    let mut owed = Vec::new();
    for stage in &running {
        let mark = format!("archive:{}", stage.id);
        assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
        owed.push((mark, lead_agent(stage)));
    }
    assert!(h.backend.archive_calls().is_empty(), "none asked of yet");
    owed
}

/// Settling a stage and dispatching write the owed mark and ask Aether
/// nothing, even while every archive would hang; unrelated work starts.
#[tokio::test]
async fn settlement_and_dispatch_never_ask_aether_to_archive() {
    let h = Harness::new().await;
    let item = h.enqueue("failing build").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    let worker = h.backend.delegate(&agent, MODEL);
    h.backend.end_delegated(&worker, DescendantState::Stopped);
    let _never_open = [
        h.backend.hold_archive(&agent),
        h.backend.hold_archive(&worker),
    ];
    let next = h.enqueue("unrelated").await;

    let moved_on = async {
        h.settle_running(|_| failed("tests failed")).await;
        h.dispatch().await;
        h.dispatch().await;
    };
    promptly("settlement and dispatch", moved_on).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(h.backend.archive_calls().is_empty(), "left to the worker");
    let stage = h.stages(&item.id).await.remove(0);
    let mark = h.kv(&format!("archive:{}", stage.id)).await;
    assert_eq!(mark.as_deref(), Some("owed"));
    let started = h.state(&next.id).await;
    assert!(matches!(started, QueueState::Running { .. }), "{started:?}");
}

/// A pass waiting inside an archive call holds nothing dispatch needs: an
/// unrelated item starts while the call is still open.
#[tokio::test]
async fn a_pass_waiting_on_aether_holds_up_no_dispatch() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let gate = h.backend.hold_archive(&worker);
    let calls = h.backend.archive_calls().len();
    let next = h.enqueue("unrelated").await;

    let mut progress = Progress::default();
    let mut waiting = std::pin::pin!(archive::pass(&h.ctx, &mut progress));
    let asked = async {
        while h.backend.archive_calls().len() == calls {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        biased;
        ended = &mut waiting => panic!("the pass is held in its call: {ended:?}"),
        () = promptly("the pass's first call", asked) => {}
    }
    promptly("dispatch", h.dispatch()).await;
    let started = h.state(&next.id).await;
    assert!(matches!(started, QueueState::Running { .. }), "{started:?}");
    assert!(h.backend.archived().is_empty(), "the call is still open");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    gate.add_permits(1);
    waiting.await.expect("archive pass");
    // A slow host may have run the held call into its limit: one more pass.
    h.archive().await;
    assert_eq!(h.backend.archived().last(), Some(&agent));
    assert_eq!(h.kv(&mark).await, None);
}

/// Many delegates whose archives each succeed slowly: every pass ends on
/// its budget with the mark owed and the stage agent in view, asks of no
/// delegate twice, and a later pass finishes the tree. Dispatch goes on.
#[tokio::test(start_paused = true)]
async fn a_tree_too_slow_for_one_pass_is_archived_over_several() {
    const DELEGATES: usize = 7;
    let h = Harness::new().await;
    let item = h.enqueue("many delegates").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    for _ in 0..DELEGATES {
        let worker = h.backend.delegate(&agent, MODEL);
        h.backend.end_delegated(&worker, DescendantState::Stopped);
    }
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    // Three fit a pass; the fourth is cut off by the budget.
    h.backend.slow_archives(Duration::from_secs(3));

    let mut progress = Progress::default();
    for archived in [3, 6] {
        let began = Instant::now();
        pass(&h, &mut progress).await;
        assert_eq!(began.elapsed(), PASS_BUDGET, "the pass ends on its budget");
        assert_eq!(h.backend.archived().len(), archived, "delegates so far");
        assert!(!h.backend.archive_calls().contains(&agent), "parent waits");
        assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
        let next = h.enqueue(&format!("unrelated after {archived}")).await;
        h.dispatch().await;
        let started = h.state(&next.id).await;
        assert!(matches!(started, QueueState::Running { .. }), "{started:?}");
    }
    pass(&h, &mut progress).await;
    let mut archived = h.backend.archived();
    assert_eq!(archived.pop(), Some(agent), "the stage agent last");
    assert_eq!(archived.len(), DELEGATES);
    archived.sort();
    archived.dedup();
    assert_eq!(archived.len(), DELEGATES, "each delegate archived once");
    assert_eq!(h.kv(&mark).await, None);
}

/// A delegate whose archive never answers: the pass gives the call up at
/// its limit, the stage agent is not asked of, the mark stays owed, and a
/// later pass archives the tree.
#[tokio::test(start_paused = true)]
async fn a_delegate_archive_that_hangs_times_out_and_keeps_its_parent() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let calls = h.backend.archive_calls().len();
    let gate = h.backend.hold_archive(&worker);
    let mut progress = Progress::default();
    let began = Instant::now();
    pass(&h, &mut progress).await;
    assert_eq!(began.elapsed(), CALL_LIMIT, "one call's limit, no more");
    assert_eq!(
        &h.backend.archive_calls()[calls..],
        std::slice::from_ref(&worker)
    );
    assert!(h.backend.archived().is_empty(), "the parent stays in view");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    gate.add_permits(1);
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archived(), [worker, agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// The stage agent's own archive never answers: its delegates are archived,
/// the mark stays owed, and a later pass asks of the stage agent alone.
#[tokio::test(start_paused = true)]
async fn a_stage_agent_archive_that_hangs_times_out_and_stays_owed() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let gate = h.backend.hold_archive(&agent);
    let mut progress = Progress::default();
    let began = Instant::now();
    pass(&h, &mut progress).await;
    assert_eq!(began.elapsed(), CALL_LIMIT, "one call's limit, no more");
    assert_eq!(h.backend.archived(), std::slice::from_ref(&worker));
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    gate.add_permits(1);
    let calls = h.backend.archive_calls().len();
    pass(&h, &mut progress).await;
    assert_eq!(
        &h.backend.archive_calls()[calls..],
        std::slice::from_ref(&agent)
    );
    assert_eq!(h.backend.archived(), [worker, agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// A tree listing Aether does not answer in time leaves the tree owed and
/// unasked; the next pass archives it.
#[tokio::test(start_paused = true)]
async fn a_tree_listing_that_hangs_times_out_and_is_retried() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let calls = h.backend.archive_calls().len();
    let gate = h.backend.hold_descendants_after(0);
    let mut progress = Progress::default();
    let began = Instant::now();
    pass(&h, &mut progress).await;
    assert_eq!(began.elapsed(), CALL_LIMIT, "one call's limit, no more");
    assert_eq!(h.backend.archive_calls().len(), calls, "none asked");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    gate.add_permits(1);
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archived(), [worker, agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// Two silent trees take a whole pass; the next starts at the marks that
/// pass never reached, so they are not starved by the silent ones.
#[tokio::test(start_paused = true)]
async fn a_pass_out_of_time_resumes_at_the_marks_it_did_not_reach() {
    let h = Harness::new().await;
    let owed = owed_agents(&h, 4).await;
    let agents: Vec<AetherAgentId> = owed.iter().map(|(_, agent)| agent.clone()).collect();
    let gates = [
        h.backend.hold_archive(&agents[0]),
        h.backend.hold_archive(&agents[1]),
    ];
    let mut progress = Progress::default();
    let began = Instant::now();
    pass(&h, &mut progress).await;
    assert_eq!(began.elapsed(), PASS_BUDGET, "the pass ends on its budget");
    assert_eq!(h.backend.archive_calls(), agents[..2], "oldest first");
    assert!(h.backend.archived().is_empty());

    let began = Instant::now();
    pass(&h, &mut progress).await;
    assert!(began.elapsed() <= PASS_BUDGET, "{:?}", began.elapsed());
    assert_eq!(h.backend.archived(), agents[2..], "those not reached first");
    for (mark, _) in &owed[..2] {
        assert_eq!(h.kv(mark).await.as_deref(), Some("owed"));
    }
    for (mark, _) in &owed[2..] {
        assert_eq!(h.kv(mark).await, None);
    }

    for gate in &gates {
        gate.add_permits(1);
    }
    pass(&h, &mut progress).await;
    assert_eq!(
        h.backend.archived().len(),
        4,
        "the silent ones once they answer"
    );
    for (mark, _) in &owed {
        assert_eq!(h.kv(mark).await, None);
    }
}

/// A tree Aether refuses to archive stays owed, with its stage agent in
/// view, and is left alone until [`REFUSED_RETRY`] has passed. The refused
/// agent is the tree's delegate, or else the stage agent itself.
async fn refused_then_allowed(refuse_delegate: bool) {
    let h = Harness::new().await;
    let item = h.enqueue("archive refused").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    let worker = h.backend.delegate(&agent, MODEL);
    h.backend.end_delegated(&worker, DescendantState::Stopped);
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    let refused = if refuse_delegate { &worker } else { &agent };
    let refusal = ArchonError::Aether("permission_denied: archive refused".into());
    h.backend.fail_next_archive_of(refused, refusal);

    let mut progress = Progress::default();
    pass(&h, &mut progress).await;
    assert!(!h.backend.archived().contains(&agent), "it stays in view");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"), "never given up");
    let calls = h.backend.archive_calls().len();
    let listings = h.backend.descendant_checks().len();

    tokio::time::advance(REFUSED_RETRY - Duration::from_secs(1)).await;
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archive_calls().len(), calls, "not asked so soon");
    assert_eq!(h.backend.descendant_checks().len(), listings, "nor listed");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    tokio::time::advance(Duration::from_secs(1)).await;
    pass(&h, &mut progress).await;
    let mut archived = h.backend.archived();
    assert_eq!(archived.pop(), Some(agent), "the stage agent last");
    assert_eq!(archived, [worker], "its delegate once");
    assert_eq!(h.kv(&mark).await, None);
}

#[tokio::test(start_paused = true)]
async fn a_refused_delegate_archive_stays_owed_and_is_asked_again_much_later() {
    refused_then_allowed(true).await;
}

#[tokio::test(start_paused = true)]
async fn a_refused_stage_agent_archive_stays_owed_and_is_asked_again_much_later() {
    refused_then_allowed(false).await;
}

/// A delegate Aether refused is left alone for [`REFUSED_RETRY`] even while
/// another of its tree goes unanswered: each pass asks of that other one
/// alone, and of nothing once only the refusal is left.
#[tokio::test(start_paused = true)]
async fn a_refused_delegate_is_left_alone_while_another_goes_unanswered() {
    let h = Harness::new().await;
    let item = h.enqueue("archive refused and unanswered").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    let refused = h.backend.delegate(&agent, MODEL);
    let silent = h.backend.delegate(&agent, MODEL);
    for delegate in [&refused, &silent] {
        h.backend.end_delegated(delegate, DescendantState::Stopped);
    }
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    let mark = format!("archive:{}", h.stages(&item.id).await.remove(0).id);
    let refusal = ArchonError::Aether("permission_denied: archive refused".into());
    h.backend.fail_next_archive_of(&refused, refusal);
    let gate = h.backend.hold_archive(&silent);

    let mut progress = Progress::default();
    let asked = Instant::now();
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archive_calls(), [refused.clone(), silent.clone()]);
    for _ in 0..3 {
        let calls = h.backend.archive_calls().len();
        pass(&h, &mut progress).await;
        assert_eq!(
            &h.backend.archive_calls()[calls..],
            std::slice::from_ref(&silent),
            "the refused one is not asked of again"
        );
    }
    assert!(h.backend.archived().is_empty(), "the tree stays in view");

    gate.add_permits(1);
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&silent));
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    let calls = h.backend.archive_calls().len();
    let listings = h.backend.descendant_checks().len();

    let early = asked + REFUSED_RETRY - Duration::from_secs(1);
    tokio::time::advance(early - Instant::now()).await;
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archive_calls().len(), calls, "not asked so soon");
    assert_eq!(h.backend.descendant_checks().len(), listings, "nor listed");

    tokio::time::advance(Duration::from_secs(1)).await;
    pass(&h, &mut progress).await;
    assert_eq!(h.backend.archived(), [silent, refused, agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// A running worker asks of a mark as it is written, without waiting for
/// its next unannounced pass.
#[tokio::test]
async fn the_worker_is_woken_by_a_new_mark() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let worker = tokio::spawn(archive::run_every(h.ctx.clone(), NEVER));
    // Its first pass, with nothing owed, is over before any mark is written.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!worker.is_finished(), "the worker runs until stopped");

    let item = h.enqueue("failing build").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    eventually("the new mark is settled", async || {
        h.kv(&mark).await.is_none()
    })
    .await;
    assert_eq!(h.backend.archived(), [agent]);

    worker.abort();
    let stopped = worker.await.expect_err("it never ends by itself");
    assert!(stopped.is_cancelled(), "{stopped}");
}

/// A fresh agent taking a failed one's place wakes the worker too: the
/// replaced agent is archived while its run is still live.
#[tokio::test]
async fn the_worker_is_woken_when_a_fresh_agent_replaces_a_failed_one() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let worker = tokio::spawn(archive::run_every(h.ctx.clone(), NEVER));
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    eventually("the replaced agent's mark is done", async || {
        h.kv(&mark).await.as_deref() == Some("done")
    })
    .await;
    assert_eq!(h.backend.archived(), [agent]);
    let live = h.state(&item.id).await;
    assert!(matches!(live, QueueState::Running { .. }), "{live:?}");
    worker.abort();
}

/// A worker stopped inside an Aether call leaves the mark owed; the worker
/// started after it archives what the marks still owe in its first pass.
#[tokio::test]
async fn a_worker_stopped_mid_call_leaves_its_mark_for_the_next_worker() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let gate = h.backend.hold_archive(&agent);

    let first = tokio::spawn(archive::run_every(h.ctx.clone(), NEVER));
    eventually("the stage agent's archive is asked", async || {
        h.backend.archive_calls().contains(&agent)
    })
    .await;
    first.abort();
    let stopped = first.await.expect_err("stopped, not ended");
    assert!(stopped.is_cancelled(), "{stopped}");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    assert_eq!(h.backend.archived(), std::slice::from_ref(&worker));

    gate.add_permits(1);
    let second = tokio::spawn(archive::run_every(h.ctx.clone(), NEVER));
    eventually("the owed mark is settled", async || {
        h.kv(&mark).await.is_none()
    })
    .await;
    assert_eq!(h.backend.archived().last(), Some(&agent));
    second.abort();
    let stopped = second.await.expect_err("stopped, not ended");
    assert!(stopped.is_cancelled(), "{stopped}");
}
