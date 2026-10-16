//! Push notifications (`[notify]`): a gate opening runs the configured
//! command or POSTs to a local fake webhook; unlisted kinds send nothing;
//! a failing notifier is logged and leaves the item alone.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_core::autonomy::{AutonomyPatch, ProposalMode};
use archon_core::config::Config;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::push::PushKind;
use archon_core::queue::QueueItem;
use archon_core::run::{GateSubject, RunState, StageOutcome, StageRun};
use common::{Harness, PIPELINE, REPO_NAME, completed, plan};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a notification may take to arrive, or to prove it never does.
const ARRIVAL: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(500);

/// A notifier that appends each event it reads on stdin as one line.
fn recording_notifier(dir: &Path) -> (Vec<String>, PathBuf) {
    let log = dir.join("events.jsonl");
    let script = dir.join("notifier");
    std::fs::write(&script, "#!/bin/sh\ncat >> \"$1\"\necho >> \"$1\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let command = vec![script.display().to_string(), log.display().to_string()];
    (command, log)
}

fn events(log: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .map(|line| serde_json::from_str(line).expect("each line is the event JSON"))
        .collect()
}

/// The events once `count` have arrived; panics after [`ARRIVAL`].
async fn wait_for_events(log: &Path, count: usize) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + ARRIVAL;
    loop {
        let seen = events(log);
        if seen.len() >= count {
            tokio::time::sleep(QUIET).await;
            return events(log);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{seen:?} after {ARRIVAL:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn config(notify: impl FnOnce(&mut archon_core::push::NotifyConfig)) -> Config {
    let mut config = Config::default();
    config.notify.min_gap_secs = 0;
    notify(&mut config.notify);
    config.validate().expect("a valid [notify]");
    config
}

/// Enqueue an item whose proposal gate opens, and wait until it is asked.
async fn open_gate(h: &Harness, title: &str) -> QueueItem {
    let request = EnqueueRequest {
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: "secret brief text".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            proposal: Some(ProposalMode::Lavish),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    let Ok(Reply::Item(item)) = h.cli(Request::Enqueue(request)).await else {
        panic!("enqueue failed")
    };
    h.dispatch().await;
    for stage in h.running().await {
        assert_eq!(stage.stage_id, "propose", "the knob put propose first");
        let page = h
            .run(&item.id)
            .await
            .workspace
            .join(".archon/reports/proposal/index.html");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        std::fs::write(&page, "<html><body>page</body></html>").unwrap();
    }
    h.settle_running(|_: &StageRun| -> StageOutcome { completed("Approach: a flag.") })
        .await;
    h.dispatch().await;
    assert!(
        matches!(
            h.run(&item.id).await.state,
            RunState::AwaitingOperator {
                subject: GateSubject::Proposal,
                ..
            }
        ),
        "the proposal gate opened"
    );
    item
}

#[tokio::test]
async fn a_gate_opening_runs_the_command_once_with_the_event_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let (command, log) = recording_notifier(dir.path());
    let h = Harness::with_config(config(|n| n.command = command)).await;
    let item = open_gate(&h, "Add a greeting flag").await;
    let seen = wait_for_events(&log, 1).await;
    assert_eq!(seen.len(), 1, "one gate, one notification: {seen:?}");
    let event = &seen[0];
    assert_eq!(event["kind"], "gate");
    assert_eq!(event["id"], item.id.as_str());
    assert_eq!(event["title"], "Add a greeting flag");
    let line = event["line"].as_str().unwrap();
    assert!(
        line.starts_with('Q') && line.contains("waits for your decision"),
        "{line}"
    );
    let raw = std::fs::read_to_string(&log).unwrap();
    assert!(
        !raw.contains("secret brief") && !raw.contains("Approach"),
        "{raw}"
    );
}

#[tokio::test]
async fn kinds_not_listed_in_on_send_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (command, log) = recording_notifier(dir.path());
    let h = Harness::with_config(config(|n| {
        n.command = command;
        n.on = vec![PushKind::Question];
    }))
    .await;
    open_gate(&h, "Pick a cache").await;
    tokio::time::sleep(QUIET).await;
    assert!(events(&log).is_empty(), "the gate kind is not listed");
    let ask = Request::AskOperator {
        text: "Which cache, and the secret token is abc?".into(),
        item: None,
        options: Vec::new(),
        caller: None,
    };
    let Ok(Reply::Question(question)) = h.orchestrator(ask).await else {
        panic!("ask failed")
    };
    let seen = wait_for_events(&log, 1).await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["kind"], "question");
    assert_eq!(seen[0]["id"], question.id.as_str());
    assert_eq!(
        seen[0]["line"],
        format!("Q{} waits for your answer", question.number)
    );
}

/// Everything logged at `WARN` or above while it is held, on this thread.
struct Warnings {
    lines: Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Warnings {
    fn capture() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || Captured(Arc::clone(&sink)))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        Self { lines, _guard }
    }

    fn text(&self) -> String {
        String::from_utf8(self.lines.lock().unwrap().clone()).unwrap()
    }

    async fn wait_for(&self, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + ARRIVAL;
        while !self.text().contains(needle) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no {needle:?} in {}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.text()
    }
}

#[tokio::test]
async fn a_failing_notifier_is_logged_and_does_not_affect_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let failing = dir.path().join("failing");
    std::fs::write(
        &failing,
        "#!/bin/sh\necho 'ntfy: no such topic' >&2\nexit 3\n",
    )
    .unwrap();
    std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
    for program in [failing, dir.path().join("missing-notifier")] {
        let warnings = Warnings::capture();
        let program = program.display().to_string();
        let h = Harness::with_config(config(|n| n.command = vec![program.clone()])).await;
        let item = open_gate(&h, "Rename a flag").await;
        let logged = warnings.wait_for("push notification failed").await;
        assert!(logged.contains(&program), "{logged}");
        let run = h.run(&item.id).await;
        assert!(
            matches!(run.state, RunState::AwaitingOperator { .. }),
            "{:?}",
            run.state
        );
        assert!(
            !h.item(&item.id).await.state.is_terminal(),
            "the item waits on"
        );
    }
}

/// A one-request HTTP server on loopback that hands back the body it got.
async fn fake_webhook() -> (String, tokio::task::JoinHandle<(String, String)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook?token=abc", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let (head, body_len) = loop {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "the request ended early");
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let head = text[..end].to_owned();
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .expect("a POST names its length");
                break (head, end + 4 + len);
            }
        };
        while raw.len() < body_len {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "the body ended early");
            raw.extend_from_slice(&buf[..n]);
        }
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let body = String::from_utf8(raw[head.len() + 4..].to_vec()).unwrap();
        (head, body)
    });
    (url, server)
}

#[tokio::test]
async fn a_gate_opening_posts_the_event_to_the_webhook() {
    let (url, server) = fake_webhook().await;
    let h = Harness::with_config(config(|n| n.webhook = Some(url))).await;
    let item = open_gate(&h, "Add a greeting flag").await;
    let (head, body) = tokio::time::timeout(ARRIVAL, server)
        .await
        .expect("the webhook was called")
        .unwrap();
    assert!(head.starts_with("POST /hook?token=abc "), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{head}"
    );
    let event: Value = serde_json::from_str(&body).expect("the body is the event JSON");
    assert_eq!(event["kind"], "gate");
    assert_eq!(event["id"], item.id.as_str());
}
