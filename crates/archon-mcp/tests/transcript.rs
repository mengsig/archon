//! End to end: MCP JSON-RPC on stdin, forwarded over a fake `archond`
//! socket, the reply on stdout.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use archon_client::framing::{read_frame, write_frame};
use archon_core::autonomy::MergeMode;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{
    ClientFrame, ClientKind, ErrorCode, ErrorReply, PROTOCOL_VERSION, Reply, Request, ServerFrame,
    Welcome,
};
use archon_core::queue::{Priority, QueueItem, QueueState};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

type Answer = fn(&Request) -> Result<Reply, ErrorReply>;
/// Which requests the fake daemon records but never answers.
type Withhold = fn(&Request) -> bool;

/// A scripted `archond`: answers `hello`, records every other request and
/// answers it with `answer`, unless `withhold` picks it. Dropping it closes
/// every connection.
struct FakeDaemon {
    requests: Arc<Mutex<Vec<Request>>>,
    accept: JoinHandle<()>,
}

impl FakeDaemon {
    fn start(socket: &Path, answer: Answer) -> Self {
        Self::withholding(socket, answer, |_| false)
    }

    fn withholding(socket: &Path, answer: Answer, withhold: Withhold) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the fake archond");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let accept = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let recorded = recorded.clone();
                connections.spawn(serve_connection(stream, answer, withhold, recorded));
            }
        });
        Self { requests, accept }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve_connection(
    stream: UnixStream,
    answer: Answer,
    withhold: Withhold,
    recorded: Arc<Mutex<Vec<Request>>>,
) {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    while let Ok(Some(frame)) = read_frame::<_, ClientFrame>(&mut read).await {
        let result = match &frame.request {
            Request::Hello { client, .. } => {
                assert_eq!(*client, ClientKind::ArchonMcp);
                Ok(Reply::Welcome(Welcome {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: "fake".into(),
                    pid: 7,
                }))
            }
            request => {
                assert!(!request.is_operator_only(), "MCP forwarded {request:?}");
                recorded.lock().unwrap().push(request.clone());
                if withhold(request) {
                    continue;
                }
                answer(request)
            }
        };
        let reply = ServerFrame::Reply {
            id: frame.id,
            result,
        };
        if write_frame(&mut write, &reply).await.is_err() {
            return;
        }
    }
}

fn answer_everything(request: &Request) -> Result<Reply, ErrorReply> {
    match request {
        Request::RepoList => Ok(Reply::Repos(vec![])),
        Request::Enqueue(_) => Err(ErrorReply {
            code: ErrorCode::Policy,
            message: "model policy: stage \"implement\": model x/y is not allowed; allowed: \
                      claude-code/claude-opus-5-5"
                .into(),
        }),
        other => panic!("the fake daemon has no answer for {other:?}"),
    }
}

/// `archon mcp` on in-memory stdio.
struct Mcp {
    stdin: DuplexStream,
    stdout: BufReader<ReadHalf<DuplexStream>>,
    task: JoinHandle<Result<archon_mcp::Served, ArchonError>>,
}

impl Mcp {
    fn start(socket: &Path, archon_bin: &Path) -> Self {
        let (stdin, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, client_out) = tokio::io::duplex(1 << 16);
        let (socket, archon_bin): (PathBuf, PathBuf) = (socket.into(), archon_bin.into());
        let task = tokio::spawn(async move {
            archon_mcp::serve(server_in, server_out, &socket, &archon_bin).await
        });
        let (stdout, _unused_write) = tokio::io::split(client_out);
        Self {
            stdin,
            stdout: BufReader::new(stdout),
            task,
        }
    }

    async fn send_line(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
    }

    async fn send(&mut self, message: Value) {
        self.send_line(&message.to_string()).await;
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(20), self.stdout.read_line(&mut line));
        read.await
            .expect("a response in time")
            .expect("readable stdout");
        assert!(line.ends_with('\n'), "one message per line");
        assert!(
            line.len() <= archon_mcp::MAX_MESSAGE_BYTES,
            "a {}-byte response would make Aether drop the server",
            line.len()
        );
        serde_json::from_str(&line).expect("a JSON response")
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let response = self.recv().await;
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id, "responses come back in order");
        response
    }

    async fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        let params = json!({"name": tool, "arguments": arguments});
        let response = self.request(id, "tools/call", params).await;
        response["result"].clone()
    }

    /// A call Aether vouches came from `caller` (`send_caller_meta = true`).
    async fn call_as(&mut self, id: u64, caller: &str, tool: &str, arguments: Value) -> Value {
        let meta = json!({"aether/agent_id": caller});
        let params = json!({"name": tool, "arguments": arguments, "_meta": meta});
        let response = self.request(id, "tools/call", params).await;
        response["result"].clone()
    }

    async fn send_call(&mut self, id: impl Into<Value>, tool: &str) {
        let params = json!({"name": tool, "arguments": {}});
        self.send(
            json!({"jsonrpc": "2.0", "id": id.into(), "method": "tools/call", "params": params}),
        )
        .await;
    }

    /// Close stdin, leaving the server to answer what is in flight.
    async fn end_input(&mut self) {
        self.stdin.shutdown().await.unwrap();
    }

    async fn close(self) {
        drop(self.stdin);
        let served = self.task.await.expect("serve did not panic");
        let served = served.expect("serve ends cleanly when stdin closes");
        assert_eq!(served, archon_mcp::Served::InputEnded);
    }
}

fn enqueue_arguments() -> Value {
    json!({
        "repo": "web", "pipeline": "direct", "title": "Fix login", "brief": "Fix it.",
        "model_plan": {"implement": [{"model": "x/y", "effort": "high"}], "ship": [{"model": "x/y", "effort": "low"}]}
    })
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().expect("a text block")
}

#[tokio::test]
async fn a_session_initializes_lists_tools_and_relays_a_daemon_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = FakeDaemon::start(&socket, answer_everything);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));

    let init = mcp
        .request(1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "aether", "version": "1"}}))
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "archon");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;

    let listed = mcp.request(2, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), archon_core::mcp_tools::TOOLS.len());
    assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));

    let refused = mcp.call(3, "enqueue", enqueue_arguments()).await;
    assert_eq!(refused["isError"], true);
    assert!(text(&refused).contains("x/y is not allowed"), "{refused}");
    assert_eq!(refused["structuredContent"]["code"], "policy");

    let invalid = mcp.call(4, "enqueue", json!({"pipeline": "direct"})).await;
    assert_eq!(invalid["isError"], true);
    assert!(
        text(&invalid).contains("missing required argument(s)"),
        "{invalid}"
    );

    let repos = mcp.call(5, "list_repos", json!({})).await;
    assert_eq!(repos["isError"], false);
    assert_eq!(text(&repos), "No repositories.");
    assert_eq!(
        repos["structuredContent"],
        json!({"reply": "repos", "data": []})
    );

    let requests = daemon.requests();
    assert_eq!(
        requests.len(),
        2,
        "only valid calls reach the daemon: {requests:?}"
    );
    assert!(matches!(&requests[0], Request::Enqueue(e) if e.title == "Fix login"));
    assert_eq!(requests[1], Request::RepoList);
    mcp.close().await;
}

/// A fake archond for which `agt_orchestrator` is the orchestrator: who may
/// change Archon is archond's to decide, where the change is made (§18.4).
fn answer_only_the_orchestrator(request: &Request) -> Result<Reply, ErrorReply> {
    let (caller, request) = match request {
        Request::ForAgent { caller, request } => (Some(caller.as_str()), request.as_ref()),
        bare => (None, bare),
    };
    if request.is_read_only() {
        return answer_everything(request);
    }
    if caller != Some("agt_orchestrator") {
        return Err(ErrorReply {
            code: ErrorCode::Forbidden,
            message: format!("only the orchestrator may, not {caller:?}. Nothing was changed."),
        });
    }
    assert!(matches!(request, Request::Pause(_)), "{request:?}");
    Ok(Reply::Ok)
}

/// The server never decides who the orchestrator is, so no caller can slip
/// between a check here and the change there: each call is one request.
#[tokio::test]
async fn a_tool_call_reaches_archond_once_naming_its_caller_and_relays_the_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = FakeDaemon::start(&socket, answer_only_the_orchestrator);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));
    let global = json!({"scope": "global"});

    let refused = mcp.call_as(1, "agt_child", "pause", global.clone()).await;
    assert_eq!(refused["isError"], true);
    let why = text(&refused);
    assert!(why.contains("not Some(\"agt_child\")"), "{why}");
    assert_eq!(refused["structuredContent"]["code"], "forbidden");

    let read = mcp.call_as(2, "agt_child", "list_repos", json!({})).await;
    assert_eq!(read["isError"], false, "reading is open to a child: {read}");
    let paused = mcp
        .call_as(3, "agt_orchestrator", "pause", global.clone())
        .await;
    assert_eq!(paused["isError"], false, "{paused}");
    let unnamed = mcp.call(4, "pause", global).await;
    assert_eq!(unnamed["isError"], true, "{unnamed}");
    assert!(text(&unnamed).contains("not None"), "{unnamed}");

    let requests = daemon.requests();
    assert_eq!(requests.len(), 4, "one request for each call: {requests:?}");
    let callers: Vec<Option<&str>> = requests
        .iter()
        .map(|request| match request {
            Request::ForAgent { caller, .. } => Some(caller.as_str()),
            _bare => None,
        })
        .collect();
    let orchestrator = Some("agt_orchestrator");
    assert_eq!(
        callers,
        [Some("agt_child"), Some("agt_child"), orchestrator, None]
    );
    assert!(matches!(requests[3], Request::Pause(_)), "{requests:?}");
    mcp.close().await;
}

#[tokio::test]
async fn protocol_errors_use_json_rpc_codes_and_notifications_get_no_reply() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::start(&dir.path().join("absent.sock"), &dir.path().join("archon"));
    mcp.send_line("this is not json").await;
    let parse = mcp.recv().await;
    assert_eq!(
        (parse["error"]["code"].as_i64(), &parse["id"]),
        (Some(-32700), &Value::Null)
    );
    mcp.send_line("").await;
    mcp.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
    )
    .await;
    let pong = mcp.request(1, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    let missing = mcp.request(2, "resources/list", json!({})).await;
    assert_eq!(missing["error"]["code"], -32601);
    let params = json!({"name": "delete_everything", "arguments": {}});
    let unknown = mcp.request(3, "tools/call", params).await;
    assert_eq!(unknown["error"]["code"], -32602);
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("enqueue")
    );
    let newer = mcp
        .request(4, "initialize", json!({"protocolVersion": "2099-01-01"}))
        .await;
    assert_eq!(
        newer["result"]["protocolVersion"],
        archon_mcp::jsonrpc::MCP_PROTOCOL_VERSION
    );
    mcp.close().await;
}

#[tokio::test]
async fn an_unreachable_daemon_is_a_tool_error_and_the_next_call_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let mut mcp = Mcp::start(&socket, &dir.path().join("no-such-archon"));

    let down = mcp.call(1, "list_repos", json!({})).await;
    assert_eq!(down["isError"], true);
    let message = text(&down);
    assert!(
        message.contains("daemon is unavailable") && never_started(message),
        "{message}"
    );
    assert!(message.contains("archon daemon start"), "{message}");

    let daemon = FakeDaemon::start(&socket, answer_everything);
    let up = mcp.call(2, "list_repos", json!({})).await;
    assert_eq!(up["isError"], false, "{up}");

    // A daemon restart costs at most the one call that raced it.
    drop(daemon);
    std::fs::remove_file(&socket).unwrap();
    let restarted = FakeDaemon::start(&socket, answer_everything);
    let mut attempts = 0;
    let result = loop {
        attempts += 1;
        let result = mcp.call(2 + attempts, "list_repos", json!({})).await;
        if result["isError"] == false || attempts == 2 {
            break result;
        }
        assert!(text(&result).contains("unavailable"), "{result}");
    };
    assert_eq!(
        result["isError"], false,
        "reconnected after {attempts} call(s): {result}"
    );
    assert_eq!(restarted.requests(), vec![Request::RepoList]);
    mcp.close().await;
}

/// Whether a read's error says archond was not started for it: always, and
/// with the reason when this process's default dirs beside a temp socket mix.
fn never_started(message: &str) -> bool {
    [
        "a read-only call never starts it",
        "it is not started for you",
    ]
    .iter()
    .any(|said| message.contains(said))
}

/// A read never runs `archon daemon run --detach`: not on the first call,
/// not for an agent Aether names, and not to reconnect after archond left.
#[tokio::test]
async fn a_read_only_call_never_starts_archond() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let started = dir.path().join("started");
    let archon = dir.path().join("archon");
    let launcher = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\nexit 3\n",
        started.display()
    );
    archon_core::testing::write_script(&archon, &launcher);
    let mut mcp = Mcp::start(&socket, &archon);

    let down = mcp.call(1, "list_repos", json!({})).await;
    assert!(never_started(text(&down)), "{down}");
    let down = mcp.call_as(2, "agt_1", "list_queue", json!({})).await;
    assert!(never_started(text(&down)), "{down}");

    let daemon = FakeDaemon::start(&socket, answer_everything);
    let up = mcp.call(3, "list_repos", json!({})).await;
    assert_eq!(up["isError"], false, "{up}");
    drop(daemon);
    std::fs::remove_file(&socket).unwrap();
    // The first may still go out on the closing connection; the next reconnects.
    let gone = mcp.call(4, "list_repos", json!({})).await;
    assert_eq!(gone["isError"], true, "{gone}");
    let reconnect = mcp.call(5, "list_repos", json!({})).await;
    assert!(never_started(text(&reconnect)), "{reconnect}");
    assert!(!started.exists(), "the launcher ran for a read");
    assert!(!socket.exists(), "nothing listens again");
    mcp.close().await;
}

#[tokio::test]
async fn operator_only_arguments_never_reach_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = FakeDaemon::start(&socket, answer_everything);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));
    let item = QueueItemId::new().to_string();
    let merge = mcp
        .call(
            1,
            "register_repo",
            json!({"path": "/srv/web", "auto_merge": true}),
        )
        .await;
    assert_eq!(merge["isError"], true);
    let hard = mcp
        .call(2, "pause", json!({"scope": "global", "hard": true}))
        .await;
    assert_eq!(hard["isError"], true);
    let say = mcp
        .call(
            3,
            "notify",
            json!({"kind": "shout", "text": "hi", "item": item}),
        )
        .await;
    assert!(text(&say).contains("`kind`"), "{say}");
    assert!(daemon.requests().is_empty());
    mcp.close().await;
}

/// The daemon's ceiling check, reduced to the one knob the test raises.
fn refuse_auto_merge(request: &Request) -> Result<Reply, ErrorReply> {
    let Request::Enqueue(enqueue) = request else {
        panic!("the fake daemon only enqueues, got {request:?}");
    };
    assert_eq!(
        enqueue.autonomy.merge,
        Some(MergeMode::Auto),
        "passed through as asked"
    );
    Err(ErrorReply::from(&ArchonError::Forbidden(
        "autonomy above the ceiling for repo web: merge=auto (allowed: manual); only the \
         operator can raise it"
            .into(),
    )))
}

#[tokio::test]
async fn a_knob_above_the_ceiling_is_refused_with_what_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = FakeDaemon::start(&socket, refuse_auto_merge);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));

    let mut arguments = enqueue_arguments();
    arguments["autonomy"] = json!({"merge": "auto"});
    let refused = mcp.call(1, "enqueue", arguments).await;
    assert_eq!(refused["isError"], true);
    assert_eq!(refused["structuredContent"]["code"], "forbidden");
    assert!(
        text(&refused).contains("merge=auto (allowed: manual)"),
        "{refused}"
    );

    let mut unknown = enqueue_arguments();
    unknown["autonomy"] = json!({"merge": "always"});
    let invalid = mcp.call(2, "enqueue", unknown).await;
    assert_eq!(invalid["isError"], true);
    assert!(text(&invalid).contains("`autonomy`"), "{invalid}");
    assert_eq!(
        daemon.requests().len(),
        1,
        "an unknown knob value never reaches the daemon"
    );
    mcp.close().await;
}

fn queued_item(title: String, brief: String) -> QueueItem {
    QueueItem {
        onto: None,
        id: QueueItemId::new(),
        repo: None,
        pipeline: "ask".into(),
        title,
        brief,
        priority: Priority::Normal,
        dependencies: vec![],
        tags: vec![],
        model_plan: Default::default(),
        model_reasons: None,
        budget_usd: None,
        autonomy: Default::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        risk: None,
        state: QueueState::Queued,
        created_at: archon_core::now(),
        updated_at: archon_core::now(),
    }
}

/// `limit` 40: long briefs, so the reply data passes Aether's structured cap.
/// No limit: a history whose listing alone passes the MCP message cap.
fn answer_long_history(request: &Request) -> Result<Reply, ErrorReply> {
    let Request::ListItemSummaries(query) = request else {
        panic!("the fake daemon only lists items, got {request:?}");
    };
    let items: Vec<_> = match query.limit {
        Some(limit) => (0..limit)
            .map(|i| queued_item(format!("item {i}"), "b".repeat(2_000)))
            .collect(),
        None => (0..600)
            .map(|i| queued_item(format!("{i} {}", "t".repeat(8_000)), "b".into()))
            .collect(),
    };
    Ok(Reply::ItemSummaries(
        items
            .into_iter()
            .map(|item| {
                let next_action = format!("archon queue show {}", item.id);
                archon_core::protocol::ItemSummary {
                    item,
                    stage: None,
                    spent_usd: 0.0,
                    pr_url: None,
                    waiting_on: None,
                    waiting_reason: Some(archon_core::protocol::WaitingReason {
                        code: archon_core::protocol::WaitingReasonCode::NextInLine,
                        message: "Next in line for the next dispatch pass".into(),
                        next_action,
                    }),
                    switched_to: None,
                }
            })
            .collect(),
    ))
}

#[tokio::test]
async fn results_stay_under_aethers_message_and_structured_caps() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let _daemon = FakeDaemon::start(&socket, answer_long_history);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));

    let listed = mcp.call(1, "list_queue", json!({"limit": 40})).await;
    assert_eq!(listed["isError"], false, "{}", text(&listed));
    assert!(text(&listed).starts_with("40 items:"), "{}", text(&listed));
    assert!(
        listed.get("structuredContent").is_none(),
        "structured data Aether would drop is not sent"
    );

    let huge = mcp.call(2, "list_queue", json!({})).await;
    assert!(
        huge.to_string().len() < archon_mcp::MAX_MESSAGE_BYTES,
        "an oversized message would make Aether drop the server"
    );
    assert_eq!(huge["isError"], true);
    let message = text(&huge);
    assert!(
        message.contains("MCP message limit") && message.contains("`limit`"),
        "{message}"
    );
    mcp.close().await;
}

fn withhold_status(request: &Request) -> bool {
    matches!(request, Request::Status(_))
}

fn withhold_status_and_replies(request: &Request) -> bool {
    matches!(request, Request::Status(_) | Request::GithubReply { .. })
}

fn ids(responses: &[Value]) -> BTreeSet<u64> {
    responses.iter().filter_map(|r| r["id"].as_u64()).collect()
}

#[tokio::test]
async fn a_slow_call_holds_up_neither_the_calls_behind_it_nor_their_deadlines() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = FakeDaemon::withholding(&socket, answer_everything, withhold_status_and_replies);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));

    let sent = Instant::now();
    mcp.send_call(1, "queue_status").await;
    mcp.send_call(2, "queue_status").await;
    mcp.send_call(3, "list_repos").await;
    mcp.send(json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}))
        .await;
    let reply = json!({"repository": "o/r", "number": 7, "body": "Queued."});
    mcp.send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": {"name": "github_reply", "arguments": reply}}))
        .await;
    mcp.end_input().await;

    let quick = [mcp.recv().await, mcp.recv().await];
    assert_eq!(ids(&quick), BTreeSet::from([3, 4]), "{quick:?}");
    assert!(
        sent.elapsed() < Duration::from_secs(5),
        "{:?}",
        sent.elapsed()
    );
    let repos = quick.iter().find(|r| r["id"] == 3).unwrap();
    assert_eq!(repos["result"]["isError"], false, "{repos}");

    let slow = [mcp.recv().await, mcp.recv().await, mcp.recv().await];
    assert_eq!(ids(&slow), BTreeSet::from([1, 2, 5]), "{slow:?}");
    for response in slow.iter().filter(|r| r["id"] != 5) {
        assert!(
            text(&response["result"]).contains("did not answer `queue_status` within 15s"),
            "{response}"
        );
    }
    // A comment may be up on GitHub: the thread says, not the queue.
    let reply = text(&slow.iter().find(|r| r["id"] == 5).unwrap()["result"]);
    assert!(
        reply.contains("https://github.com/o/r/issues/7") && reply.contains("never repeat"),
        "{reply}"
    );
    assert!(!reply.contains("list_queue"), "{reply}");
    let elapsed = sent.elapsed();
    let budget = archon_mcp::CALL_BUDGET;
    assert!(
        elapsed >= budget && elapsed < budget + Duration::from_secs(3),
        "both deadlines ran from intake, not one after the other: {elapsed:?}"
    );
    assert_eq!(daemon.requests().len(), 4, "{:?}", daemon.requests());
    mcp.close().await;
}

#[tokio::test]
async fn calls_past_the_concurrency_cap_are_refused_as_busy() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let _daemon = FakeDaemon::withholding(&socket, answer_everything, withhold_status);
    let mut mcp = Mcp::start(&socket, &dir.path().join("archon"));
    let cap = archon_mcp::MAX_CONCURRENT_CALLS as u64;
    for id in 1..=cap {
        mcp.send_call(id, "queue_status").await;
    }
    mcp.send_call(cap + 1, "list_repos").await;
    mcp.send(json!({"jsonrpc": "2.0", "id": cap + 2, "method": "ping"}))
        .await;

    let busy = mcp.recv().await;
    assert_eq!(busy["id"], cap + 1, "{busy}");
    assert_eq!(busy["result"]["isError"], true);
    let message = text(&busy["result"]);
    assert!(message.contains("retry shortly") && message.contains("Nothing was changed"));
    let pong = mcp.recv().await;
    assert_eq!(
        (&pong["id"], &pong["result"]),
        (&json!(cap + 2), &json!({}))
    );
    mcp.task.abort();
}

/// A `tools/call` line whose tool name alone nearly fills the message cap.
fn near_cap(filler: usize) -> String {
    "n".repeat(archon_mcp::MAX_MESSAGE_BYTES - filler)
}

#[tokio::test]
async fn answers_to_near_cap_requests_stay_within_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcp = Mcp::start(&dir.path().join("absent.sock"), &dir.path().join("archon"));

    let unknown_tool = json!({"name": near_cap(100), "arguments": {}});
    let refused = mcp.request(1, "tools/call", unknown_tool).await;
    assert_eq!(refused["error"]["code"], -32602);
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("[truncated from") && message.contains("enqueue"),
        "{}",
        &message[..300]
    );

    let unknown_method = mcp.request(2, &near_cap(100), json!({})).await;
    assert_eq!(unknown_method["error"]["code"], -32601);

    let bad_state = json!({"states": [near_cap(200)]});
    let result = mcp.call(3, "list_queue", bad_state).await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).starts_with("list_queue: `states` contains"));
    assert!(text(&result).ends_with(" bytes]"), "a truncation marker");

    let unknown_argument = json!({near_cap(200): 1});
    let result = mcp.call(4, "list_repos", unknown_argument).await;
    assert!(text(&result).contains("unknown argument(s)"), "{result}");

    let longest_id = "i".repeat(archon_mcp::jsonrpc::MAX_ID_BYTES - 2);
    let request = json!({"jsonrpc": "2.0", "id": longest_id, "method": "tools/call", "params": {"name": near_cap(400)}});
    mcp.send(request).await;
    let answered = mcp.recv().await;
    assert_eq!(
        (&answered["id"], &answered["error"]["code"]),
        (&json!(longest_id), &json!(-32602))
    );

    let too_long_id = "i".repeat(archon_mcp::jsonrpc::MAX_ID_BYTES);
    mcp.send(json!({"jsonrpc": "2.0", "id": too_long_id, "method": "ping"}))
        .await;
    let refused = mcp.recv().await;
    assert_eq!(
        (&refused["id"], &refused["error"]["code"]),
        (&Value::Null, &json!(-32600))
    );
    mcp.close().await;
}

/// An archond of another protocol version: it refuses every hello.
fn refuse_every_hello(socket: &Path) -> JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("bind the fake archond");
    tokio::spawn(async move {
        let mut connections = JoinSet::new();
        while let Ok((stream, _)) = listener.accept().await {
            connections.spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut read = BufReader::new(read);
                while let Ok(Some(frame)) = read_frame::<_, ClientFrame>(&mut read).await {
                    let refusal = ErrorReply {
                        code: ErrorCode::Incompatible,
                        message: format!("archond speaks protocol {}", PROTOCOL_VERSION + 1),
                    };
                    let reply = ServerFrame::Reply {
                        id: frame.id,
                        result: Err(refusal),
                    };
                    if write_frame(&mut write, &reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    })
}

#[tokio::test]
async fn a_bridge_whose_binary_was_replaced_asks_to_restart_when_archond_refuses_its_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = refuse_every_hello(&socket);
    let bin = dir.path().join("archon");
    std::fs::write(&bin, "the version the bridge started as").unwrap();
    let mut mcp = Mcp::start(&socket, &bin);
    // Let the bridge read its binary before the upgrade replaces it.
    let listed = mcp.request(1, "tools/list", json!({})).await;
    assert!(listed["result"]["tools"].is_array());
    let upgraded = dir.path().join("archon.upgraded");
    std::fs::write(&upgraded, "the upgraded build").unwrap();
    std::fs::rename(&upgraded, &bin).unwrap();

    let refused = mcp.call(2, "list_repos", json!({})).await;
    assert_eq!(refused["isError"], true);
    assert!(text(&refused).contains("restarts itself"), "{refused}");
    assert!(text(&refused).contains("nothing was changed"), "{refused}");

    let served = tokio::time::timeout(Duration::from_secs(20), mcp.task)
        .await
        .expect("the bridge stops to restart")
        .expect("serve did not panic")
        .expect("serve ends cleanly");
    assert_eq!(served, archon_mcp::Served::Restart);
    daemon.abort();
}

#[tokio::test]
async fn a_restart_waits_until_the_requests_already_read_are_answered() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = refuse_every_hello(&socket);
    let bin = dir.path().join("archon");
    std::fs::write(&bin, "the version the bridge started as").unwrap();
    let mut mcp = Mcp::start(&socket, &bin);
    let listed = mcp.request(1, "tools/list", json!({})).await;
    assert!(listed["result"]["tools"].is_array());
    let upgraded = dir.path().join("archon.upgraded");
    std::fs::write(&upgraded, "the upgraded build").unwrap();
    std::fs::rename(&upgraded, &bin).unwrap();

    let call = |id: u64| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": "list_repos", "arguments": {}}})
    };
    mcp.send(call(2)).await;
    mcp.send(call(3)).await;
    let mut answered = [
        mcp.recv().await["id"].clone(),
        mcp.recv().await["id"].clone(),
    ];
    answered.sort_by_key(|id| id.as_u64());
    assert_eq!(
        answered,
        [json!(2), json!(3)],
        "no request read was dropped"
    );
    let served = tokio::time::timeout(Duration::from_secs(20), mcp.task)
        .await
        .expect("the bridge stops to restart")
        .expect("serve did not panic")
        .expect("serve ends cleanly");
    assert_eq!(served, archon_mcp::Served::Restart);
    daemon.abort();
}

#[tokio::test]
async fn a_protocol_refusal_without_an_upgrade_is_reported_and_the_bridge_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let daemon = refuse_every_hello(&socket);
    let bin = dir.path().join("archon");
    std::fs::write(&bin, "unchanged").unwrap();
    let mut mcp = Mcp::start(&socket, &bin);

    let refused = mcp.call(1, "list_repos", json!({})).await;
    assert_eq!(refused["isError"], true);
    assert!(text(&refused).contains("protocol"), "{refused}");
    assert!(!text(&refused).contains("restarts itself"), "{refused}");
    let again = mcp.call(2, "list_repos", json!({})).await;
    assert_eq!(again["isError"], true, "the bridge still answers");
    mcp.close().await;
    daemon.abort();
}
