//! Public CLI wait contract against an isolated protocol fixture.
mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use archon_core::id::{FeedItemId, PipelineRunId, QueueItemId};
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{
    ClientFrame, ItemDetail, PROTOCOL_VERSION, Reply, Request, ServerFrame, Welcome,
};
use archon_core::queue::{QueueItem, QueueState};
use common::Env;
use serde_json::Value;

fn item(state: QueueState) -> QueueItem {
    QueueItem {
        id: QueueItemId::new(),
        repo: None,
        pipeline: "ask".into(),
        title: "test".into(),
        brief: String::new(),
        priority: Default::default(),
        dependencies: vec![],
        tags: vec![],
        model_plan: Default::default(),
        model_reasons: None,
        budget_usd: None,
        autonomy: Default::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        initiative: None,
        state,
        created_at: archon_core::now(),
        updated_at: archon_core::now(),
    }
}

fn succeeded() -> QueueState {
    QueueState::Succeeded {
        run: PipelineRunId::new(),
        summary: "done".into(),
    }
}

/// The first snapshot can be queued; its feed wakeup makes the next one terminal.
/// A stalled request exercises the outer deadline, not just the idle wait.
fn wait(
    states: Vec<QueueState>,
    transition: bool,
    stall: bool,
) -> (std::process::Output, Duration) {
    wait_with_reason(states, transition, stall, None)
}

fn snapshot(row: QueueItem, waiting_reason: Option<archon_core::protocol::WaitingReason>) -> Reply {
    Reply::ItemDetail(Box::new(ItemDetail {
        default_efforts: Default::default(),
        steers: Vec::new(),
        item: row,
        runs: vec![],
        stages: vec![],
        spent_usd: 0.0,
        gates: vec![],
        delegations: Default::default(),
        waiting_on: None,
        waiting_reason,
        switched_to: None,
    }))
}

/// The feed event that wakes a waiter to read its items again.
fn completion_feed() -> ServerFrame {
    ServerFrame::Feed {
        sub: 1,
        item: FeedItem {
            id: FeedItemId::new(),
            seq: 1,
            at: archon_core::now(),
            entry: FeedEntry::System {
                text: "completion".into(),
            },
        },
    }
}

/// A daemon answering one `queue wait`: `transition` makes the first snapshot
/// queued and the feed wake it, `stall` holds every snapshot past the deadline.
fn serve(
    listener: UnixListener,
    rows: Vec<QueueItem>,
    transition: bool,
    stall: bool,
    waiting_reason: Option<archon_core::protocol::WaitingReason>,
) {
    let (stream, _) = listener.accept().unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut subscribed = false;
    let mut gets = 0;
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let frame: ClientFrame = serde_json::from_str(&line).unwrap();
        let reply = match frame.request {
            Request::Hello { .. } => Reply::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "fixture".into(),
                pid: std::process::id(),
            }),
            Request::SubscribeFeed { .. } => {
                subscribed = true;
                Reply::Subscribed { id: 1, last_seq: 0 }
            }
            Request::GetItem { item: id } => {
                assert!(subscribed, "subscribe before the first snapshot");
                assert!(
                    waiting_reason.is_none() || gets == 0,
                    "a failed dependency stops before requesting other items"
                );
                if stall {
                    std::thread::sleep(Duration::from_secs(4));
                }
                let mut row = rows.iter().find(|row| row.id == id).unwrap().clone();
                if transition && gets == 0 {
                    row.state = QueueState::Queued;
                }
                gets += 1;
                snapshot(row, waiting_reason.clone())
            }
            other => panic!("unexpected request {other:?}"),
        };
        let response = ServerFrame::Reply {
            id: frame.id,
            result: Ok(reply),
        };
        if writeln!(writer, "{}", serde_json::to_string(&response).unwrap()).is_err() {
            break;
        }
        if transition && gets == 1 {
            let feed = completion_feed();
            if writeln!(writer, "{}", serde_json::to_string(&feed).unwrap()).is_err() {
                break;
            }
        }
    }
}

fn wait_with_reason(
    states: Vec<QueueState>,
    transition: bool,
    stall: bool,
    waiting_reason: Option<archon_core::protocol::WaitingReason>,
) -> (std::process::Output, Duration) {
    let env = Env::new();
    let rows: Vec<_> = states.into_iter().map(item).collect();
    let mut args = vec!["queue".to_owned(), "wait".to_owned()];
    args.extend(rows.iter().map(|row| row.id.to_string()));
    args.extend(["--timeout", "1s", "--json"].map(str::to_owned));
    let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
    let server =
        std::thread::spawn(move || serve(listener, rows, transition, stall, waiting_reason));
    let started = Instant::now();
    let out = env
        .command(&args.iter().map(String::as_str).collect::<Vec<_>>())
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    server.join().unwrap();
    (out, elapsed)
}

#[test]
fn wait_success_failure_cancellation_and_mixed_results() {
    for (states, code) in [
        (vec![succeeded(), succeeded()], 0),
        (
            vec![QueueState::Failed {
                run: None,
                error: "failed".into(),
            }],
            3,
        ),
        (vec![QueueState::Cancelled { reason: None }], 3),
        (
            vec![
                succeeded(),
                QueueState::Failed {
                    run: None,
                    error: "failed".into(),
                },
            ],
            3,
        ),
    ] {
        let count = states.len();
        let (out, _) = wait(states, false, false);
        assert_eq!(out.status.code(), Some(code), "{out:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["timed_out"], false);
        assert_eq!(value["items"].as_array().unwrap().len(), count);
    }
}

#[test]
fn wait_observes_completion_after_subscribing() {
    let (out, _) = wait(vec![succeeded()], true, false);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["items"][0]["state"]["state"], "succeeded");
}

#[test]
fn timeout_includes_blocked_items_and_stalled_requests() {
    for (state, stall) in [
        (QueueState::Queued, false),
        (
            QueueState::Blocked {
                reason: "dependency".into(),
            },
            false,
        ),
        (succeeded(), true),
    ] {
        let (out, elapsed) = wait(vec![state], false, stall);
        assert_eq!(out.status.code(), Some(4), "{out:?}");
        assert!(
            elapsed < Duration::from_secs(4),
            "deadline exceeded: {elapsed:?}"
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["timed_out"], true);
    }
}

#[test]
fn empty_items_and_invalid_timeouts_fail_before_connecting() {
    let env = Env::new();
    let out = env.command(&["queue", "wait"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    for timeout in ["0s", "forever", "999999999999999999999w"] {
        let out = env
            .command(&["queue", "wait", "qi_bad", "--timeout", timeout, "--json"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["error"]["code"], "invalid_argument");
    }
    assert!(!env.path("archond.sock").exists());
}

#[test]
fn wait_does_not_autostart_daemons_in_dedicated_mode() {
    let env = Env::with_config("[aether]\nmode = 'dedicated'\n");
    let id = QueueItemId::new().to_string();
    let out = env
        .command(&["queue", "wait", &id, "--timeout", "1s", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "unavailable");
    assert!(!env.path("data").exists());
}

#[test]
fn failed_or_cancelled_dependency_returns_promptly_with_its_name() {
    for state in ["failed", "cancelled"] {
        let reason = archon_core::protocol::WaitingReason {
            code: archon_core::protocol::WaitingReasonCode::DependencyFailed,
            message: format!("Dependency qi_required (required migration) {state}"),
            next_action: "archon queue retry qi_required".into(),
        };
        let (out, _) = wait_with_reason(
            vec![
                QueueState::Blocked {
                    reason: "dependency".into(),
                },
                QueueState::Queued,
            ],
            false,
            false,
            Some(reason),
        );
        assert_eq!(out.status.code(), Some(5), "{out:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["timed_out"], false);
        assert!(
            value["blocked_dependencies"][0]
                .as_str()
                .unwrap()
                .contains("required migration")
        );
        assert!(
            value["blocked_dependencies"][0]
                .as_str()
                .unwrap()
                .contains(state)
        );
    }
}
