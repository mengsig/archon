//! Stage agents the pipeline moved past are archived in Aether, with what
//! they delegated: a finished run's failed attempts, an attempt a fresh
//! agent replaced, and an escalation once its item moves on. An unresolved
//! escalation or blocker stays in view under the one notice its run posts.
//! Dispatch and settlement only write the owed marks; `Harness::archive` is
//! one pass of the worker that asks Aether (`worker` covers its limits).

mod settled;
mod worker;

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_core::ports::DescendantState;
use archon_core::protocol::Request;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{StageOutcome, StageRun};
use archon_core::steer::{InsertStage, Steer, Then};
use common::{Harness, MODEL, PIPELINE};

fn failed(error: &str) -> StageOutcome {
    StageOutcome::Failed {
        error: error.into(),
        infra: false,
    }
}

fn engine_died() -> StageOutcome {
    StageOutcome::Failed {
        error: "engine died".into(),
        infra: true,
    }
}

fn lead_agent(stage: &StageRun) -> AetherAgentId {
    stage
        .aether_agent
        .clone()
        .expect("a running stage has an agent")
}

fn retry(item: &QueueItem) -> Request {
    Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    }
}

/// The implementer, which delegated a worker that ended, fails its run.
#[tokio::test]
async fn a_finished_run_archives_its_failed_stage_agents_and_their_delegates() {
    let h = Harness::new().await;
    let item = h.enqueue("failing build").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    let worker = h.backend.delegate(&agent, MODEL);
    h.backend.end_delegated(&worker, DescendantState::Stopped);
    assert!(h.backend.archived().is_empty(), "nothing moved past yet");

    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    assert!(h.backend.archive_calls().is_empty(), "left to the worker");

    h.archive().await;
    assert_eq!(h.backend.archived(), [worker, agent.clone()]);
    assert_eq!(h.kv(&mark).await, None);

    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.backend.archived().len(), 2, "archived once");
}

/// A woken agent is never archived while its slot can still use it; the
/// cold retry's fresh agent moves past it, so it is archived then.
#[tokio::test]
async fn a_retried_attempt_archives_the_agent_it_replaced() {
    let h = Harness::new().await;
    let item = h.enqueue("flaky engine").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend.fail_agent(&agent);
    h.settle_running(|_| engine_died()).await;
    h.dispatch().await;
    assert_eq!(h.backend.woken(), std::slice::from_ref(&agent));
    assert!(h.backend.archived().is_empty(), "the woken agent works on");

    h.backend.fail_agent(&agent);
    h.settle_running(|_| engine_died()).await;
    h.dispatch().await;
    let cold = lead_agent(&h.running().await[0]);
    assert_ne!(cold, agent, "the cold retry is a fresh agent");
    h.archive().await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    let woken_attempt = h.stages(&item.id).await.remove(1);
    let mark = format!("archive:{}", woken_attempt.id);
    assert_eq!(
        h.kv(&mark).await.as_deref(),
        Some("done"),
        "kept while the run is live, so it is archived once"
    );
    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.backend.archived().len(), 1, "archived once");

    run_out(&h).await;
    h.archive().await;
    assert_eq!(
        h.backend.archived(),
        std::slice::from_ref(&agent),
        "completed attempts stay"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(h.kv(&mark).await, None, "dropped when the node retired");
}

/// The cold retry's spawn owes the archive of the agent it replaced in the
/// same store write as its own `running`: a daemon gone before its worker
/// asked Aether still archives it after a restart.
#[tokio::test]
async fn a_replacement_s_owed_archive_survives_a_restart_right_after_its_spawn() {
    let mut h = Harness::new().await;
    let item = h.enqueue("flaky engine, then a crash").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend.fail_agent(&agent);
    h.settle_running(|_| engine_died()).await;
    h.dispatch().await;
    assert_eq!(h.backend.woken(), std::slice::from_ref(&agent));

    h.backend.fail_agent(&agent);
    h.settle_running(|_| engine_died()).await;
    h.dispatch().await;
    let cold = lead_agent(&h.running().await[0]);
    assert_ne!(cold, agent, "the cold retry is spawned");
    assert!(
        h.backend.archive_calls().is_empty(),
        "cut before any archive"
    );
    let woken_attempt = h.stages(&item.id).await.remove(1);
    let mark = format!("archive:{}", woken_attempt.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    h.restart();
    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
    assert_eq!(h.kv(&mark).await.as_deref(), Some("done"));
}

/// An archive owed when the daemon stopped is made after its restart.
#[tokio::test]
async fn a_mark_owed_across_a_restart_is_archived_after_it() {
    let mut h = Harness::new().await;
    let item = h.enqueue("aether away, then a restart").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend
        .fail_next_archive(ArchonError::Unavailable("aether away".into()));
    h.settle_running(|_| failed("tests failed")).await;
    h.archive().await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    assert_eq!(h.backend.archive_calls(), std::slice::from_ref(&agent));
    assert!(h.backend.archived().is_empty());

    h.restart();
    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// A failed run whose implementer delegated `worker`, which ended; the
/// worker's first archive, in the one pass made here, fails with `error`.
/// Returns the item's mark key, the stage agent and the worker.
async fn refused_worker(h: &Harness, error: ArchonError) -> (String, AetherAgentId, AetherAgentId) {
    let item = h.enqueue("a delegate refused").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    let worker = h.backend.delegate(&agent, MODEL);
    h.backend.end_delegated(&worker, DescendantState::Stopped);
    h.backend.fail_next_archive(error);
    h.settle_running(|_| failed("tests failed")).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    h.archive().await;
    assert_eq!(
        h.backend.archive_calls().first(),
        Some(&worker),
        "delegates go first"
    );
    let stage = h.stages(&item.id).await.remove(0);
    (format!("archive:{}", stage.id), agent, worker)
}

/// A delegate Aether holds live, or did not answer for, keeps its stage
/// agent in view and the mark owed; the next pass archives the tree.
#[tokio::test]
async fn a_delegate_not_archived_yet_keeps_its_stage_agent_for_the_next_pass() {
    let errors = [
        ArchonError::Invalid("aether: agent is live".into()),
        ArchonError::InvalidTransition("aether: agent is live".into()),
        ArchonError::Protocol("garbled reply".into()),
        ArchonError::io("aether socket", std::io::Error::other("reset")),
        ArchonError::Aether("internal: database is locked".into()),
    ];
    for error in errors {
        let h = Harness::new().await;
        let shown = error.to_string();
        let (mark, agent, worker) = refused_worker(&h, error).await;
        assert!(h.backend.archived().is_empty(), "{shown}: none archived");
        assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"), "{shown}");

        h.archive().await;
        assert_eq!(h.backend.archived(), [worker, agent], "{shown}");
        assert_eq!(h.kv(&mark).await, None, "{shown}");
    }
}

/// A delegate Aether no longer knows is gone: its stage agent is archived.
#[tokio::test]
async fn a_delegate_aether_no_longer_knows_counts_as_archived() {
    let h = Harness::new().await;
    let gone = ArchonError::NotFound("aether: no agent".into());
    let (mark, agent, _) = refused_worker(&h, gone).await;
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// A delegate Aether refuses to archive keeps the whole tree in view and
/// the mark owed: a restarted daemon asks again, and archives the tree once
/// Aether allows it.
#[tokio::test]
async fn a_refused_delegate_keeps_its_tree_in_view_and_its_mark_owed() {
    let mut h = Harness::new().await;
    let refused = ArchonError::Aether("permission_denied: archive refused".into());
    let (mark, agent, worker) = refused_worker(&h, refused).await;
    assert!(h.backend.archived().is_empty(), "the stage agent stays too");
    assert_eq!(h.backend.archive_calls(), std::slice::from_ref(&worker));
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"), "never given up");

    h.restart();
    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.backend.archived(), [worker, agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// An owed mark of `h`'s new failed run whose implementer delegated a
/// worker that ended: the first archive went unanswered.
async fn owed_tree(h: &Harness) -> (String, AetherAgentId, AetherAgentId) {
    let unanswered = ArchonError::Unavailable("aether away".into());
    let (mark, agent, worker) = refused_worker(h, unanswered).await;
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    (mark, agent, worker)
}

/// A tree Aether cannot list, for any reason, is not archived in part:
/// a `NotFound` listing the run's agents does not prove the tree gone.
#[tokio::test]
async fn an_unreadable_tree_is_never_archived_in_part() {
    let errors = [
        ArchonError::Aether("descendants: internal error".into()),
        ArchonError::NotFound("aether: run not found".into()),
    ];
    for error in errors {
        let h = Harness::new().await;
        let shown = error.to_string();
        let (mark, agent, worker) = owed_tree(&h).await;
        let calls = h.backend.archive_calls().len();
        h.backend.fail_next_descendants(error);
        h.archive().await;
        assert_eq!(
            h.backend.archive_calls().len(),
            calls,
            "{shown}: no agent alone"
        );
        assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"), "{shown}");

        h.archive().await;
        assert_eq!(h.backend.archived(), [worker, agent], "{shown}");
        assert_eq!(h.kv(&mark).await, None, "{shown}");
    }
}

/// No agent of a tree is asked to archive while one of it still runs.
#[tokio::test]
async fn a_tree_with_a_live_delegate_waits_for_it() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let late = h.backend.delegate(&agent, MODEL);
    let calls = h.backend.archive_calls();
    h.archive().await;
    assert_eq!(h.backend.archive_calls(), calls, "no member asked");
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    h.backend.end_delegated(&late, DescendantState::Stopped);
    h.archive().await;
    let mut archived = h.backend.archived();
    assert_eq!(archived.pop(), Some(agent), "the stage agent last");
    archived.sort();
    let mut delegates = vec![worker, late];
    delegates.sort();
    assert_eq!(archived, delegates);
    assert_eq!(h.kv(&mark).await, None);
}

/// Aborting the running stage for a retry archives the aborted agent once
/// the retry's fresh agent took its place, and the run goes on.
#[tokio::test]
async fn an_aborted_attempt_is_archived_once_its_retry_spawned() {
    let h = Harness::new().await;
    let item = h.enqueue("gone astray").await;
    h.dispatch().await;
    let aborted = h.running().await.remove(0);
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: Steer {
            abort: Some(Then::Retry),
            ..Steer::default()
        },
    };
    h.cli(steer).await.expect("aborted");
    h.dispatch().await;
    let retry = h.running().await.remove(0);
    assert_ne!(lead_agent(&retry), lead_agent(&aborted));
    h.archive().await;
    assert_eq!(h.backend.archived(), [lead_agent(&aborted)]);
    let mark = format!("archive:{}", aborted.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("done"));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

/// Settle what runs, passing every review, until nothing runs.
async fn run_out(h: &Harness) {
    for _ in 0..12 {
        if h.running().await.is_empty() {
            return;
        }
        h.settle_running(|stage| match stage.stage_id.as_str() {
            "review" => common::verdict(true),
            _ => common::completed("done"),
        })
        .await;
        h.dispatch().await;
    }
    panic!("the run never ended");
}

/// Fail the only stage of a new item's run with `error`, which ends the run,
/// and check its agent is kept, with the one notice of `kind` the finished
/// run posts; returns the item and the kept agent.
async fn kept_failure(
    h: &Harness,
    error: &str,
    kind: NotificationKind,
) -> (QueueItem, AetherAgentId) {
    let item = h.enqueue("needs the operator").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.settle_running(|_| failed(error)).await;
    for _ in 0..2 {
        h.dispatch().await;
        h.archive().await;
    }
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(
        h.backend.archive_calls().is_empty(),
        "kept for the operator"
    );
    let notices = h.notices(kind).await;
    assert_eq!(notices.len(), 1, "one notice: {notices:?}");
    let other = match kind {
        NotificationKind::Blocker => NotificationKind::Outcome,
        _ => NotificationKind::Blocker,
    };
    assert!(h.notices(other).await.is_empty(), "and no other");
    (item, agent)
}

/// A `BLOCKED:` failure waits under its one blocker; retrying the item
/// moves past it, and its agent is archived.
#[tokio::test]
async fn an_unresolved_blocker_stays_in_view_until_its_item_is_retried() {
    let h = Harness::new().await;
    let blocked = "BLOCKED: the deploy key is missing";
    let (item, agent) = kept_failure(&h, blocked, NotificationKind::Blocker).await;
    assert!(h.notices(NotificationKind::Blocker).await[0].contains("deploy key"));

    h.cli(retry(&item)).await.expect("retry");
    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
    assert_eq!(
        h.notices(NotificationKind::Blocker).await.len(),
        1,
        "no second notice"
    );
}

/// An `ESCALATE:` failure hands the item back under the run's one notice;
/// a steer that revives the item moves past it, and its agent is archived.
#[tokio::test]
async fn an_unresolved_escalation_stays_in_view_until_its_item_is_steered() {
    let h = Harness::new().await;
    let escalated = "ESCALATE: this needs a schema migration first";
    let (item, agent) = kept_failure(&h, escalated, NotificationKind::Outcome).await;

    let switch = Steer {
        switch_pipeline: Some(PIPELINE.into()),
        ..Steer::default()
    };
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: switch,
    };
    h.orchestrator(steer).await.expect("revived");
    h.dispatch().await;
    h.archive().await;
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
}

/// A routine failure gets no blocker, and its agent is archived at once.
#[tokio::test]
async fn a_routine_failure_is_archived_without_a_blocker() {
    let h = Harness::new().await;
    let item = h.enqueue("routine failure").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.settle_running(|_| failed("tests failed")).await;
    h.dispatch().await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    assert!(h.notices(NotificationKind::Blocker).await.is_empty());
    h.archive().await;
    assert_eq!(h.backend.archived(), [agent]);
}

/// An archive Aether did not answer stays owed, and the next pass makes it.
#[tokio::test]
async fn an_unanswered_archive_is_retried_by_the_next_pass() {
    let h = Harness::new().await;
    let item = h.enqueue("aether away").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend
        .fail_next_archive(ArchonError::Unavailable("aether away".into()));
    h.settle_running(|_| failed("tests failed")).await;
    assert!(matches!(h.state(&item.id).await, QueueState::Failed { .. }));
    h.archive().await;
    assert_eq!(h.backend.archive_calls(), std::slice::from_ref(&agent));
    assert!(
        h.backend.archived().is_empty(),
        "the archive went unanswered"
    );
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));

    h.archive().await;
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None, "done and forgotten");
}

/// A failure inside Aether, such as a busy database, passes: the stage
/// agent's archive is owed and the next pass makes it.
#[tokio::test]
async fn an_internal_aether_error_on_the_stage_agent_is_retried() {
    let h = Harness::new().await;
    let item = h.enqueue("aether busy").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend
        .fail_next_archive(ArchonError::Aether("internal: disk full".into()));
    h.settle_running(|_| failed("tests failed")).await;
    h.archive().await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.backend.archive_calls(), std::slice::from_ref(&agent));
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"), "still owed");

    h.archive().await;
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None);
}

/// A corrupt archive mark is dropped by the pass that reads it, which still
/// archives the valid owed agent; dispatch goes on beside it.
#[tokio::test]
async fn a_corrupt_archive_mark_does_not_stop_the_pass() {
    let h = Harness::new().await;
    let item = h.enqueue("aether away").await;
    h.dispatch().await;
    let agent = lead_agent(&h.running().await[0]);
    h.backend
        .fail_next_archive(ArchonError::Unavailable("aether away".into()));
    h.settle_running(|_| failed("tests failed")).await;
    h.archive().await;
    let stage = h.stages(&item.id).await.remove(0);
    let mark = format!("archive:{}", stage.id);
    assert_eq!(h.kv(&mark).await.as_deref(), Some("owed"));
    let bad_key = "archive:not-a-stage-run";
    let bad_value = format!("archive:{}", archon_core::id::StageRunId::new());
    {
        let mut store = h.ctx.store.lock().await;
        store.kv_set(bad_key, "owed").unwrap();
        store.kv_set(&bad_value, "garbled").unwrap();
    }
    let next = h.enqueue("after the corrupt marks").await;

    h.dispatch().await;
    h.archive().await;
    assert_eq!(h.kv(bad_key).await, None, "corrupt key dropped");
    assert_eq!(h.kv(&bad_value).await, None, "corrupt value dropped");
    assert_eq!(h.backend.archived(), [agent]);
    assert_eq!(h.kv(&mark).await, None, "valid mark done and forgotten");
    assert!(matches!(
        h.state(&next.id).await,
        QueueState::Running { .. }
    ));
}

/// An owed mark naming a stage run that never had an agent is dropped as
/// corrupt, not a panic that ends the worker; the pass still archives the
/// valid owed agent.
#[tokio::test]
async fn an_owed_mark_of_a_stage_run_without_an_agent_is_dropped_as_corrupt() {
    let h = Harness::new().await;
    let (mark, agent, worker) = owed_tree(&h).await;
    let unbound = h.enqueue("never spawned").await;
    h.backend
        .fail_next(ArchonError::Unavailable("aether away".into()));
    h.dispatch().await;
    let stage = h.stages(&unbound.id).await.remove(0);
    assert_eq!(stage.aether_agent, None, "its spawn went unanswered");
    let corrupt = format!("archive:{}", stage.id);
    h.ctx.store.lock().await.kv_set(&corrupt, "owed").unwrap();

    h.archive().await;
    assert_eq!(h.kv(&corrupt).await, None, "corrupt mark dropped");
    assert_eq!(h.backend.archived(), [worker, agent]);
    assert_eq!(h.kv(&mark).await, None, "valid mark done and forgotten");
}

/// A pipeline to borrow stage `verify` from.
const EXTRA: &str = r#"[pipeline]
name = "archive-extra"
description = "a stage to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "verify"
kind = "work"
prompt = "verify.md"
timeout_minutes = 30
"#;

/// An escalation revived by inserting a stage moves past it: its agent is
/// archived once the revived run starts.
#[tokio::test]
async fn an_escalation_revived_by_an_inserted_stage_is_archived() {
    let h = Harness::new().await;
    h.add_pipeline("archive-extra", EXTRA);
    let prompt = h.repo.join(".archon/pipelines/archive-extra/verify.md");
    std::fs::write(prompt, "Verify {{task.title}}.\n\n{{task.brief}}\n").expect("prompt");
    let escalated = "ESCALATE: this needs a design pass first";
    let (item, agent) = kept_failure(&h, escalated, NotificationKind::Outcome).await;

    let insert = Steer {
        insert_stage: Some(InsertStage {
            stage: "verify".into(),
            from: Some("archive-extra".into()),
            after: None,
        }),
        ..Steer::default()
    };
    let steer = Request::SteerItem {
        item: item.id.clone(),
        steer: insert,
    };
    h.cli(steer).await.expect("revived");
    h.dispatch().await;
    let verifying = h.running().await.remove(0);
    assert_eq!(verifying.stage_id, "verify");
    h.archive().await;
    assert_eq!(h.backend.archived(), std::slice::from_ref(&agent));
}
