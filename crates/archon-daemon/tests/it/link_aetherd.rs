//! archond's supervision of Archon's own aetherd against the fake: one
//! that is killed is started again and the running item completes, and
//! one that cannot be started is a blocker whose remedy is truthful.

use crate::link_common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_aether::client::Endpoint;
use archon_aether::fake::{self, FakeAether, Scripted};
use archon_core::error::ArchonError;
use archon_core::notify::NotificationKind;
use archon_daemon::pump::AetherStarter;
use archon_queue::store::keys;
use link_common::{Harness, running_stage, spawn_link};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const STAGE: &str = "agt_0201";
const STAGE_TASK: &str = "tsk_0201";
/// Bounds every wait here: the grace, one start and a reconnect fit well within.
const RECOVERY: Duration = Duration::from_secs(15);

/// Stands in for `aether daemon`: binds a fresh fake on the socket the
/// killed one left behind, or fails as an absent `aether` would.
#[derive(Debug)]
struct FakeStarter {
    socket: PathBuf,
    runtime: tokio::runtime::Handle,
    /// What the next fake answers for the stage's task.
    task_state: Value,
    fails: bool,
    calls: AtomicUsize,
    started: Mutex<Option<FakeAether>>,
    /// Removes the restart record while aetherd starts, as the next
    /// archond's report does when the start outlasts its wait.
    reporter: Mutex<Option<Reporter>>,
}

struct Reporter(Arc<archon_daemon::context::DaemonContext>);

impl std::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reporter")
    }
}

impl AetherStarter for FakeStarter {
    fn ensure_running(&self) -> Result<(), ArchonError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fails {
            return Err(ArchonError::Unavailable(
                "`aether daemon` could not run: No such file or directory".into(),
            ));
        }
        if let Some(Reporter(ctx)) = self.reporter.lock().unwrap().take() {
            let reported = self
                .runtime
                .block_on(async { ctx.store.lock().await.kv_delete("upgrade_restart") });
            reported.expect("the report removes the record");
        }
        std::fs::remove_file(&self.socket).expect("the killed fake left its socket");
        let started = self.runtime.block_on(FakeAether::start(&self.socket));
        let started = started.map_err(|e| ArchonError::io("binding the fake", e))?;
        serve_stage(&started, self.task_state.clone());
        started.set_instance_id("fake-instance-2");
        *self.started.lock().unwrap() = Some(started);
        Ok(())
    }
}

/// A fake that knows the orchestrator and the stage's agent as running and
/// the stage's task in `task_state`.
fn serve_stage(fake: &FakeAether, task_state: Value) {
    let task = fake::task(STAGE_TASK, Some(STAGE), "run_0001", task_state);
    fake.set_default("get_task", Scripted::reply("task", task));
    fake.set_responder(
        "get_agent",
        Arc::new(|cmd| {
            let mut agent = fake::agent(cmd["id"].as_str().unwrap_or_default(), None);
            agent["status"] = json!({ "state": "running" });
            Scripted::reply("agent", agent)
        }),
    );
}

fn starter(socket: &Path, fails: bool) -> Arc<FakeStarter> {
    let done = json!({"state": "completed",
                      "result": {"summary": "done", "artifacts": [], "structured": null}});
    Arc::new(FakeStarter {
        socket: socket.to_owned(),
        runtime: tokio::runtime::Handle::current(),
        task_state: done,
        fails,
        calls: AtomicUsize::new(0),
        started: Mutex::new(None),
        reporter: Mutex::new(None),
    })
}

/// A first fake with the stage still active, and a pump connected to it
/// that supervises it through `starter`.
async fn connected_pump(
    harness: &Harness,
    socket: &Path,
    starter: Arc<FakeStarter>,
) -> (FakeAether, tokio::task::JoinHandle<Result<(), ArchonError>>) {
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let first = FakeAether::start(socket).await.unwrap();
    serve_stage(&first, json!({"state": "active"}));
    let endpoint = Endpoint::Unix(socket.to_owned());
    let pump = spawn_link(harness, endpoint, Some(starter));
    harness
        .eventually("the pump follows both streams", || async {
            first.live_subscriptions() == 2
        })
        .await;
    (first, pump)
}

/// Why the link is down; empty while it is up.
fn link_error(harness: &Harness) -> String {
    let link = harness.ctx.aether.client();
    link.err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

async fn within<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + RECOVERY;
    while !check().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not within {RECOVERY:?}: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_aetherd_is_started_again_and_the_running_item_completes() {
    let harness = Harness::new().await;
    let repo = tempfile::tempdir().unwrap();
    let stage = running_stage(&harness.ctx, repo.path(), STAGE, STAGE_TASK).await;
    let socket = harness.dir.path().join("own-aetherd.sock");
    let starter = starter(&socket, false);
    let (first, pump) = connected_pump(&harness, &socket, starter.clone()).await;
    assert!(
        starter.calls.load(Ordering::SeqCst) == 0,
        "a live aetherd is left alone"
    );

    drop(first);
    within("the stage settles on the restarted aetherd", || async {
        let store = harness.ctx.store.lock().await;
        store.stage_run(&stage.id).unwrap().state.is_terminal()
    })
    .await;
    assert!(!pump.is_finished(), "the pump outlives the outage");
    pump.abort();
    assert_eq!(starter.calls.load(Ordering::SeqCst), 1, "started once");
    let notes = harness.system_notes().await;
    assert!(
        notes.iter().any(|n| n.contains("archond started it again")),
        "{notes:?}"
    );
}

/// The feed's notes once the pump has started an aetherd killed while
/// `upgrade_restart` held `record`, as a restart archond hands off leaves it;
/// `reported_meanwhile` removes the record while aetherd starts.
async fn notes_after_a_restart_recorded_as(record: Value, reported_meanwhile: bool) -> Vec<String> {
    let harness = Harness::new().await;
    harness.set_kv("upgrade_restart", &record.to_string()).await;
    let socket = harness.dir.path().join("own-aetherd.sock");
    let starter = starter(&socket, false);
    if reported_meanwhile {
        *starter.reporter.lock().unwrap() = Some(Reporter(harness.ctx.clone()));
    }
    let (first, pump) = connected_pump(&harness, &socket, starter.clone()).await;
    drop(first);
    within("the pump starts aetherd again", || async {
        starter.calls.load(Ordering::SeqCst) == 1 && !harness.system_notes().await.is_empty()
    })
    .await;
    pump.abort();
    harness.system_notes().await
}

fn restart_record(what: &str, restart_aether: bool) -> Value {
    json!({"what": what, "before": {"archon": {}, "aether": {}}, "restart_aether": restart_aether})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_that_restarts_aetherd_is_reported_as_a_restart_not_a_stop() {
    let record = restart_record("upgrade archon to 9.9.9", true);
    let notes = notes_after_a_restart_recorded_as(record, false).await;
    assert!(
        notes
            .iter()
            .any(|n| n == "Archon's own aetherd restarted, as planned (upgrade archon to 9.9.9)."),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("had stopped")),
        "{notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drained_restart_of_aetherd_is_reported_as_a_restart_not_a_stop() {
    let notes =
        notes_after_a_restart_recorded_as(restart_record("a drained restart", true), false).await;
    assert!(
        notes
            .iter()
            .any(|n| n == "Archon's own aetherd restarted, as planned (a drained restart)."),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("had stopped")),
        "{notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_that_keeps_aetherd_still_reports_its_absence_as_a_stop() {
    let notes =
        notes_after_a_restart_recorded_as(restart_record("a drained restart", false), false).await;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("had stopped; archond started it again")),
        "{notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planned_restart_reported_before_aetherd_is_back_is_still_planned() {
    let record = restart_record("upgrade archon to 9.9.9", true);
    let notes = notes_after_a_restart_recorded_as(record, true).await;
    assert!(
        notes
            .iter()
            .any(|n| n == "Archon's own aetherd restarted, as planned (upgrade archon to 9.9.9)."),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("had stopped")),
        "{notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aetherd_that_cannot_be_started_is_a_blocker_with_a_working_remedy() {
    let harness = Harness::new().await;
    let socket = harness.dir.path().join("own-aetherd.sock");
    let starter = starter(&socket, true);
    let (first, pump) = connected_pump(&harness, &socket, starter.clone()).await;

    drop(first);
    within("the operator hears of the failed start", || async {
        !harness
            .notifications(NotificationKind::Blocker)
            .await
            .is_empty()
    })
    .await;
    let error = harness.ctx.aether.client().unwrap_err().to_string();
    assert!(error.contains("could not start it"), "{error}");
    assert!(error.contains("`archon daemon start`"), "{error}");
    // The next start waits out its retry; the blocker is said once.
    tokio::time::sleep(Duration::from_secs(3)).await;
    pump.abort();
    assert_eq!(starter.calls.load(Ordering::SeqCst), 1, "no start loop");
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aetherd_that_answers_but_refuses_replaces_the_absence_in_status() {
    let harness = Harness::new().await;
    let socket = harness.dir.path().join("own-aetherd.sock");
    let (first, pump) = connected_pump(&harness, &socket, starter(&socket, true)).await;

    drop(first);
    within("the failed start is the link's reason", || async {
        link_error(&harness).contains("could not start it")
    })
    .await;
    std::fs::remove_file(&socket).unwrap();
    let refusing = FakeAether::start(&socket).await.unwrap();
    refusing.set_protocol_window(1000, 1001);
    within(
        "status says why the listening aetherd is refused",
        || async {
            let error = harness.ctx.aether.client().unwrap_err().to_string();
            error.contains("answers, but archond cannot connect")
        },
    )
    .await;
    pump.abort();
    let error = harness.ctx.aether.client().unwrap_err().to_string();
    assert!(!error.contains("not running"), "{error}");
}
