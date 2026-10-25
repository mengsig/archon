//! Tracked changes: a repeat under an idempotency key returns the first
//! outcome, `request_status` follows a change from pending to applied, and
//! the read tools answer on reader connections while the writer is held.

use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RequestId};
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, ItemQuery, Reply, Request, StatusQuery};
use archon_core::queue::QueueState;
use archon_core::tracked::TrackedState;
use archon_daemon::context::DaemonContext;
use archon_daemon::handlers;
use archon_queue::store::{TrackedRequest, keys};

use crate::common::{self, Harness};

/// How long a test holds the writer: well over what a read may take.
const HOLD: Duration = Duration::from_secs(3);
/// The longest a read may take while the writer is held.
const READ_LIMIT: Duration = Duration::from_secs(1);

fn notify(text: &str) -> Request {
    Request::Notify {
        kind: NotificationKind::Decision,
        text: text.into(),
        item: None,
    }
}

fn tracked(id: &RequestId, key: Option<&str>, request: Request) -> Request {
    Request::Tracked {
        request_id: id.clone(),
        idempotency_key: key.map(str::to_owned),
        request: Box::new(request),
    }
}

/// The orchestrator is recorded, so its calls go through without the writer.
async fn orchestrator_named(h: &Harness) -> AetherAgentId {
    let mut store = h.ctx.store.lock().await;
    store
        .kv_set(keys::ORCHESTRATOR_AGENT, common::ORCHESTRATOR)
        .expect("kv");
    AetherAgentId::parse(common::ORCHESTRATOR).expect("an agent id")
}

/// `request` as `archon mcp` forwards it for `caller`.
async fn forward(
    ctx: &DaemonContext,
    caller: &AetherAgentId,
    request: Request,
) -> Result<Reply, ArchonError> {
    let request = Request::ForAgent {
        caller: caller.clone(),
        request: Box::new(request),
    };
    handlers::handle(ctx, ClientKind::ArchonMcp, request).await
}

async fn status_of(ctx: &DaemonContext, caller: &AetherAgentId, id: &RequestId) -> Reply {
    let asked = Request::RequestStatus {
        request: id.clone(),
    };
    forward(ctx, caller, asked).await.expect("request_status")
}

#[tokio::test]
async fn a_repeat_under_its_key_returns_the_first_reply_without_applying_again() {
    let h = Harness::new().await;
    let caller = orchestrator_named(&h).await;
    let first = RequestId::new();
    let call = |id: &RequestId| tracked(id, Some("tell-once"), notify("deployed"));
    let applied = forward(&h.ctx, &caller, call(&first)).await.unwrap();
    let repeated = forward(&h.ctx, &caller, call(&RequestId::new()))
        .await
        .unwrap();
    assert_eq!(repeated, applied, "the first reply, replayed");
    assert_eq!(
        h.notices(NotificationKind::Decision).await,
        vec!["deployed"]
    );

    let other = tracked(&RequestId::new(), Some("tell-once"), notify("other"));
    let refused = forward(&h.ctx, &caller, other).await.unwrap_err();
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    let message = refused.to_string();
    assert!(
        message.contains("already used for a different `notify` call")
            && message.contains(first.as_str()),
        "{message}"
    );
    assert_eq!(
        h.notices(NotificationKind::Decision).await.len(),
        1,
        "nothing more was posted"
    );

    let Reply::RequestStatus(status) = status_of(&h.ctx, &caller, &first).await else {
        panic!("the status of a tracked change");
    };
    assert_eq!(status.state, TrackedState::Applied);
    assert_eq!(status.reply.as_deref(), Some(&applied));
}

#[tokio::test]
async fn a_change_is_pending_while_it_waits_then_reports_its_reply() {
    let h = Harness::new().await;
    let caller = orchestrator_named(&h).await;
    let id = RequestId::new();
    let writer = h.ctx.store.lock().await;
    let ctx = Arc::clone(&h.ctx);
    let call = tracked(&id, Some("slow"), notify("under load"));
    let running = tokio::spawn({
        let caller = caller.clone();
        async move { forward(&ctx, &caller, call).await }
    });
    let pending = wait_for_status(&h.ctx, &caller, &id).await;
    assert_eq!(pending.state, TrackedState::Pending);
    assert_eq!(pending.idempotency_key.as_deref(), Some("slow"));
    assert!(!running.is_finished(), "the change waits on the writer");

    drop(writer);
    let reply = running.await.expect("joins").expect("applied");
    let Reply::RequestStatus(done) = status_of(&h.ctx, &caller, &id).await else {
        panic!("the status of a tracked change");
    };
    assert_eq!(done.state, TrackedState::Applied);
    assert_eq!(done.reply.as_deref(), Some(&reply));
    let stranger = AetherAgentId::parse("agt_someone_else").unwrap();
    let hidden = forward(&h.ctx, &stranger, Request::RequestStatus { request: id }).await;
    assert!(
        matches!(hidden, Err(ArchonError::NotFound(_))),
        "{hidden:?}"
    );
}

/// The status of `id` once archond has read the request.
async fn wait_for_status(
    ctx: &DaemonContext,
    caller: &AetherAgentId,
    id: &RequestId,
) -> archon_core::tracked::RequestStatus {
    let asked = || Request::RequestStatus {
        request: id.clone(),
    };
    let started = Instant::now();
    loop {
        match forward(ctx, caller, asked()).await {
            Ok(Reply::RequestStatus(status)) => return *status,
            Err(ArchonError::NotFound(_)) if started.elapsed() < READ_LIMIT => {}
            other => panic!("request_status: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_keyed_change_archond_stopped_during_is_reported_interrupted_not_run_again() {
    let h = Harness::new().await;
    let caller = orchestrator_named(&h).await;
    let call = notify("half done");
    let left = TrackedRequest {
        id: RequestId::new(),
        caller: caller.as_str().into(),
        command: "notify".into(),
        idempotency_key: Some("crashed".into()),
        request: serde_json::to_value(&call).unwrap().to_string(),
        started_at: archon_core::now(),
    };
    {
        let mut store = h.ctx.store.lock().await;
        store.claim_tracked(&left).expect("claimed");
        assert_eq!(store.interrupt_tracked(archon_core::now()).unwrap(), 1);
    }
    let repeat = tracked(&RequestId::new(), Some("crashed"), call);
    let Reply::RequestStatus(status) = forward(&h.ctx, &caller, repeat).await.unwrap() else {
        panic!("an interrupted call answers with its status");
    };
    assert_eq!(status.state, TrackedState::Interrupted);
    assert_eq!(status.request, left.id);
    assert!(
        h.notices(NotificationKind::Decision).await.is_empty(),
        "not run again"
    );
}

/// An open write transaction on another connection, as a long commit holds one.
fn hold_write_transaction(ctx: &DaemonContext) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(&ctx.paths.store).expect("open");
    conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO kv (key, value) VALUES ('held', 'x');")
        .expect("the write lock");
    conn
}

/// Hold archond's run table, its writer and an open SQLite write for [`HOLD`],
/// as an admission pass on a commit stalled by a loaded disk does; resolves
/// once all are released.
async fn hold_the_writer(ctx: Arc<DaemonContext>) -> tokio::task::JoinHandle<()> {
    let (taken, held) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        let transaction = hold_write_transaction(&ctx);
        let table = ctx.runs.lock().await;
        let writer = ctx.store.lock().await;
        taken.send(()).expect("the test waits");
        tokio::time::sleep(HOLD).await;
        drop(writer);
        drop(table);
        transaction.execute_batch("ROLLBACK").expect("released");
    });
    held.await.expect("the writer is held");
    holder
}

#[tokio::test]
async fn read_tools_answer_while_the_writer_is_held() {
    let h = Harness::new().await;
    let item = h.enqueue("read me").await;
    let settled = h.enqueue("read me settled").await;
    settle(&h, &settled.id).await;
    // queue_status reads a recorded orchestrator's freshness too.
    orchestrator_named(&h).await;
    let reads = [
        (
            "list_queue",
            Request::ListItemSummaries(ItemQuery::default()),
        ),
        (
            "get_item",
            Request::GetItem {
                item: item.id.clone(),
            },
        ),
        (
            "get_item of a settled item",
            Request::GetItem {
                item: settled.id.clone(),
            },
        ),
        ("queue_status", Request::Status(StatusQuery::default())),
        ("list_repos", Request::RepoList),
    ];
    let holder = hold_the_writer(Arc::clone(&h.ctx)).await;
    let started = Instant::now();
    for (name, read) in reads {
        let began = Instant::now();
        let reply = h.cli(read).await;
        let took = began.elapsed();
        eprintln!("{name} answered in {took:?} with the writer held");
        assert!(reply.is_ok(), "{name}: {reply:?}");
        assert!(took < READ_LIMIT, "{name} waited {took:?} on the writer");
    }
    assert!(!holder.is_finished(), "every read answered within the hold");
    assert!(started.elapsed() < HOLD);
    holder.await.expect("the holder ends");
    assert_eq!(
        viewed_at(&h, &settled.id).await,
        None,
        "a busy writer is not waited on"
    );
    let reply = h
        .cli(Request::GetItem {
            item: settled.id.clone(),
        })
        .await;
    assert!(reply.is_ok(), "{reply:?}");
    let head = h
        .ctx
        .store
        .lock()
        .await
        .last_feed_seq()
        .expect("the feed head");
    assert_eq!(
        viewed_at(&h, &settled.id).await,
        Some(head.to_string()),
        "a later view acknowledges the settled item"
    );
}

async fn settle(h: &Harness, item: &QueueItemId) {
    let cancelled = QueueState::Cancelled { reason: None };
    let mut store = h.ctx.store.lock().await;
    store
        .transition_item(item, cancelled, archon_core::now())
        .expect("cancelled");
    assert!(store.item(item).expect("the item").state.is_terminal());
}

async fn viewed_at(h: &Harness, item: &QueueItemId) -> Option<String> {
    let key = format!("{}{item}", keys::NOTIFICATION_CENTER_VIEWED);
    h.ctx
        .store
        .lock()
        .await
        .kv_get(&key)
        .expect("the view mark")
}

#[tokio::test]
async fn standby_still_reports_what_became_of_a_tracked_change() {
    let h = Harness::new().await;
    let caller = orchestrator_named(&h).await;
    let id = RequestId::new();
    let applied = forward(
        &h.ctx,
        &caller,
        tracked(&id, Some("before"), notify("sent")),
    )
    .await
    .expect("applied before the disk filled");
    h.ctx
        .standby
        .set("store: disk full: insert feed entry".into())
        .expect("not in standby yet");
    let Reply::RequestStatus(status) = status_of(&h.ctx, &caller, &id).await else {
        panic!("the status of a tracked change");
    };
    assert_eq!(status.state, TrackedState::Applied);
    assert_eq!(status.reply.as_deref(), Some(&applied));
    let refused = forward(
        &h.ctx,
        &caller,
        tracked(&RequestId::new(), None, notify("x")),
    )
    .await;
    assert!(
        matches!(refused, Err(ArchonError::DiskFull(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_reader_sees_one_commit_for_as_long_as_it_is_held() {
    let h = Harness::new().await;
    h.enqueue("listed before the reader").await;
    let everything = ItemQuery::default();
    let reader = h.ctx.read_store().await.expect("a reader");
    assert_eq!(reader.items(&everything).expect("items").len(), 1);
    tokio::time::timeout(READ_LIMIT, h.enqueue("enqueued while it reads"))
        .await
        .expect("a held reader never holds the writer");
    let again = reader.items(&everything).expect("items");
    assert_eq!(
        again.len(),
        1,
        "a held reader keeps the commit it started at"
    );
    drop(reader);
    let fresh = h.ctx.read_store().await.expect("a reader");
    assert_eq!(fresh.items(&everything).expect("items").len(), 2);
}
