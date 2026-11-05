//! The socket server over a real Unix socket in a temp directory: hello,
//! requests, feed subscription with catch-up and live items, MCP refusals,
//! a second daemon refused (also mid-start), and `Shutdown`.

use crate::common;

use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use archon_client::framing::{read_frame, write_frame};
use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::notify::{FeedEntry, FeedItem, NotificationKind};
use archon_core::protocol::{
    Clearable, ClientFrame, ClientKind, ErrorCode, ErrorReply, MIN_WEB_PROTOCOL_VERSION,
    PROTOCOL_VERSION, Reply, RepoPatch, RepoRef, Request, ServerFrame, feature,
};
use archon_core::repo::{Repo, ShipMode};
use archon_daemon::server;
use archon_queue::store::keys;
use common::{Harness, PIPELINE, REPO_NAME};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const WAIT: Duration = Duration::from_secs(5);

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next: u64,
}

impl Client {
    async fn connect(socket: &std::path::Path) -> Self {
        let stream = UnixStream::connect(socket).await.expect("connect");
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
            next: 1,
        }
    }

    /// Connect and complete the handshake as `kind`.
    async fn hello(socket: &std::path::Path, kind: ClientKind) -> Self {
        let mut client = Self::connect(socket).await;
        let hello = Request::Hello {
            protocol_version: PROTOCOL_VERSION,
            client: kind,
            client_version: "test".into(),
            features: Vec::new(),
        };
        let id = client.send(hello).await;
        match client.frame().await {
            Some(ServerFrame::Reply {
                id: got,
                result: Ok(Reply::Welcome(welcome)),
            }) => {
                assert_eq!(got, id);
                assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
                assert_eq!(welcome.pid, std::process::id());
            }
            other => panic!("expected welcome, got {other:?}"),
        }
        client
    }

    async fn send(&mut self, request: Request) -> u64 {
        let id = self.next;
        self.next += 1;
        write_frame(&mut self.writer, &ClientFrame { id, request })
            .await
            .expect("send");
        id
    }

    /// The next frame, or `None` once the server closes the connection.
    async fn frame(&mut self) -> Option<ServerFrame> {
        tokio::time::timeout(WAIT, read_frame(&mut self.reader))
            .await
            .expect("a frame in time")
            .expect("a well-formed frame")
    }

    /// Send `request` and wait for its reply, collecting feed items meanwhile.
    async fn call(
        &mut self,
        request: Request,
        feed: &mut Vec<FeedItem>,
    ) -> Result<Reply, ErrorReply> {
        let id = self.send(request).await;
        loop {
            match self.frame().await.expect("connection open") {
                ServerFrame::Reply { id: got, result } if got == id => return result,
                ServerFrame::Reply { id: got, result } => panic!("stray reply {got}: {result:?}"),
                ServerFrame::Feed { item, .. } => feed.push(item),
                ServerFrame::Change { change, .. } => panic!("stray change {change:?}"),
            }
        }
    }

    async fn feed_until(&mut self, count: usize, feed: &mut Vec<FeedItem>) {
        while feed.len() < count {
            match self.frame().await.expect("connection open") {
                ServerFrame::Feed { item, .. } => feed.push(item),
                other => panic!("expected a feed item, got {other:?}"),
            }
        }
    }
}

fn notify(text: &str) -> Request {
    Request::Notify {
        kind: NotificationKind::Decision,
        text: text.into(),
        item: None,
    }
}

fn text_of(item: &FeedItem) -> &str {
    match &item.entry {
        FeedEntry::Notification(n) => &n.text,
        other => panic!("unexpected feed entry {other:?}"),
    }
}

#[tokio::test]
async fn bind_tightens_a_loose_socket_directory_and_keeps_the_socket_private() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let parent = socket.parent().unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_listener, _lock) = server::bind(&h.ctx).expect("bind");
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(parent), 0o700);
    assert_eq!(mode(&socket), 0o600);
}

#[tokio::test]
async fn hello_requests_feed_and_shutdown_over_a_socket() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    let second = server::bind(&h.ctx).expect_err("a live daemon keeps its socket");
    assert!(matches!(second, ArchonError::Unavailable(_)), "{second:?}");

    let mut cli = Client::hello(&socket, ClientKind::ArchonCli).await;
    let mut feed = Vec::new();
    let status = cli
        .call(Request::Status(Default::default()), &mut feed)
        .await;
    assert!(matches!(status, Ok(Reply::Status(_))), "{status:?}");
    cli.call(notify("before subscribing"), &mut feed)
        .await
        .expect("notify");

    let subscribe = Request::SubscribeFeed {
        after: Some(0),
        verbose: false,
    };
    match cli.call(subscribe, &mut feed).await {
        Ok(Reply::Subscribed { last_seq, .. }) => assert_eq!(last_seq, 1),
        other => panic!("subscribe: {other:?}"),
    }
    let mut watcher = Client::hello(&socket, ClientKind::ArchonMcp).await;
    let unnamed = watcher
        .call(notify("from nobody"), &mut Vec::new())
        .await
        .expect_err("a change for an agent names the agent");
    assert_eq!(unnamed.code, ErrorCode::Forbidden);
    h.ctx
        .store
        .lock()
        .await
        .kv_set(keys::ORCHESTRATOR_AGENT, common::ORCHESTRATOR)
        .expect("kv");
    let forwarded = Request::ForAgent {
        caller: AetherAgentId::parse(common::ORCHESTRATOR).expect("an agent id"),
        request: Box::new(notify("while subscribed")),
    };
    watcher
        .call(forwarded, &mut Vec::new())
        .await
        .expect("notify");
    cli.feed_until(4, &mut feed).await;
    let seqs: Vec<u64> = feed.iter().map(|item| item.seq).collect();
    assert_eq!(
        seqs,
        [1, 2, 3, 4],
        "catch-up then live, in order, once each"
    );
    assert_eq!(text_of(&feed[0]), "before subscribing");
    let unnamed_notice = text_of(&feed[1]);
    assert!(
        unnamed_notice.starts_with("Aether is not naming the agents"),
        "the operator is told of the unnamed call: {unnamed_notice}"
    );
    assert!(
        matches!(&feed[2].entry, FeedEntry::System { text } if text.contains("again")),
        "and of the named one that follows: {:?}",
        feed[2].entry
    );
    assert_eq!(text_of(&feed[3]), "while subscribed");

    let refused = watcher
        .call(Request::Shutdown, &mut Vec::new())
        .await
        .expect_err("mcp");
    assert_eq!(refused.code, ErrorCode::Forbidden);
    assert!(!serving.is_finished(), "a refused shutdown keeps serving");

    cli.call(Request::Shutdown, &mut feed)
        .await
        .expect("shutdown");
    let served = tokio::time::timeout(WAIT, serving)
        .await
        .expect("server stops");
    served.expect("no panic").expect("a clean stop");
}

#[tokio::test]
async fn first_frame_must_be_hello_at_our_version() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));

    let mut early = Client::connect(&socket).await;
    early.send(Request::Status(Default::default())).await;
    match early.frame().await {
        Some(ServerFrame::Reply {
            id: 0,
            result: Err(e),
        }) => {
            assert_eq!(e.code, ErrorCode::InvalidArgument);
        }
        other => panic!("expected an id-0 refusal, got {other:?}"),
    }
    assert!(early.frame().await.is_none(), "the connection is closed");

    let mut old = Client::connect(&socket).await;
    let hello = Request::Hello {
        protocol_version: PROTOCOL_VERSION + 1,
        client: ClientKind::ArchonCli,
        client_version: "future".into(),
        features: Vec::new(),
    };
    let hello_id = old.send(hello).await;
    assert_ne!(hello_id, 0, "a request id is never the notice id");
    // Under the hello's id, not as an id-0 notice: a 1.4.x client reads only
    // that as `incompatible`, so its updater knows the new archond answered.
    match old.frame().await {
        Some(ServerFrame::Reply { id, result: Err(e) }) if id == hello_id => {
            assert_eq!(e.code, ErrorCode::Incompatible);
        }
        other => panic!("expected incompatible under id {hello_id}, got {other:?}"),
    }
    assert!(old.frame().await.is_none(), "the connection is closed");

    let mut cli = Client::hello(&socket, ClientKind::ArchonCli).await;
    let hello_again = Request::Hello {
        protocol_version: PROTOCOL_VERSION,
        client: ClientKind::ArchonCli,
        client_version: "test".into(),
        features: Vec::new(),
    };
    let again = cli
        .call(hello_again, &mut Vec::new())
        .await
        .expect_err("one hello");
    assert_eq!(again.code, ErrorCode::InvalidArgument);
    // Not JSON for a request: an error under its `req`, and the connection stays.
    tokio::io::AsyncWriteExt::write_all(&mut cli.writer, b"{\"req\":42,\"cmd\":\"nope\"}\n")
        .await
        .expect("write");
    match cli.frame().await {
        Some(ServerFrame::Reply {
            id: 42,
            result: Err(e),
        }) => assert_eq!(e.code, ErrorCode::InvalidArgument),
        other => panic!("expected an error for req 42, got {other:?}"),
    }
    let status = cli
        .call(Request::Status(Default::default()), &mut Vec::new())
        .await;
    assert!(status.is_ok(), "{status:?}");
    serving.abort();
}

/// The first frame of a fresh connection: `hello` as `client` at `version`.
async fn first_reply(socket: &Path, client: ClientKind, version: u32) -> Result<Reply, ErrorReply> {
    let mut connection = Client::connect(socket).await;
    let hello = Request::Hello {
        protocol_version: version,
        client,
        client_version: "test".into(),
        features: Vec::new(),
    };
    let id = connection.send(hello).await;
    match connection.frame().await {
        Some(ServerFrame::Reply { id: got, result }) if got == id || got == 0 => result,
        other => panic!("expected a reply to hello, got {other:?}"),
    }
}

/// `archon-web` may speak 23 or 24 and is told the range when it speaks
/// 25, and is offered features only at 24; the CLI still needs an exact
/// match, as one binary.
#[tokio::test]
async fn the_web_app_speaks_23_or_24_and_the_cli_only_24() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    for version in [MIN_WEB_PROTOCOL_VERSION, PROTOCOL_VERSION] {
        match first_reply(&socket, ClientKind::ArchonWeb, version).await {
            Ok(Reply::Welcome(welcome)) => {
                assert_eq!(welcome.protocol_version, version);
                assert_eq!(welcome.min_protocol_version, MIN_WEB_PROTOCOL_VERSION);
                let roles = welcome.features.iter().any(|f| f == feature::DEVICE_ROLES);
                assert_eq!(roles, version == PROTOCOL_VERSION, "{:?}", welcome.features);
            }
            other => panic!("archon-web {version}: {other:?}"),
        }
    }
    let refused = first_reply(&socket, ClientKind::ArchonWeb, PROTOCOL_VERSION + 1)
        .await
        .expect_err("25 is refused");
    assert_eq!(refused.code, ErrorCode::Incompatible);
    let range = format!("accepts {MIN_WEB_PROTOCOL_VERSION} to {PROTOCOL_VERSION}");
    assert!(refused.message.contains(&range), "{}", refused.message);
    let cli = first_reply(&socket, ClientKind::ArchonCli, MIN_WEB_PROTOCOL_VERSION)
        .await
        .expect_err("the CLI needs its own version");
    assert_eq!(cli.code, ErrorCode::Incompatible);
    match first_reply(&socket, ClientKind::ArchonCli, PROTOCOL_VERSION).await {
        Ok(Reply::Welcome(welcome)) => {
            assert_eq!(welcome.min_protocol_version, PROTOCOL_VERSION);
            let served = &welcome.features;
            for served_feature in [
                feature::DEVICE_ROLES,
                feature::GITHUB_LINKS,
                feature::GITHUB_IMPORT,
                feature::TASK_GRAPH,
                feature::RUN_PLAN,
                feature::ITEM_FILTERS,
            ] {
                assert!(served.iter().any(|f| f == served_feature), "{served:?}");
            }
        }
        other => panic!("the CLI at {PROTOCOL_VERSION}: {other:?}"),
    }
    serving.abort();
}

/// Every protocol 24 request is served: none answers `unavailable`. The
/// import names an unregistered repository, so it is refused before any `gh` call.
#[tokio::test]
async fn every_protocol_24_request_is_served() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    let mut cli = Client::hello(&socket, ClientKind::ArchonCli).await;
    let issue =
        serde_json::json!({"host": "github.com", "repo": "o/r", "number": 1, "kind": "issue"});
    let requests = [
        serde_json::json!({"cmd": "task_graph", "filter": {}}),
        serde_json::json!({"cmd": "run_plan", "run": archon_core::id::PipelineRunId::new()}),
        serde_json::json!({"cmd": "list_items", "repo": null, "pipeline": null, "tag": null, "limit": null, "text": "x"}),
        serde_json::json!({"cmd": "list_github_objects", "repo": "o/r"}),
        serde_json::json!({"cmd": "import_issue", "ref": issue}),
    ];
    for json in requests {
        let request: Request = serde_json::from_value(json.clone()).expect("a request");
        if let Err(refusal) = cli.call(request, &mut Vec::new()).await {
            assert_ne!(refusal.code, ErrorCode::Unavailable, "{json}: {refusal:?}");
            assert!(
                !refusal.message.contains("not implemented yet"),
                "{json}: {refusal:?}"
            );
        }
    }
    serving.abort();
}

/// Two daemons starting at once: while one holds the lock but does not
/// listen yet, the other must neither bind nor remove the socket file.
#[tokio::test]
async fn a_starting_daemon_keeps_its_socket_from_a_racing_one() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let starting = StartingDaemon::start(&socket);

    let raced = server::bind(&h.ctx).expect_err("the lock is held");
    assert!(matches!(raced, ArchonError::Unavailable(_)), "{raced:?}");
    assert!(socket.exists(), "the socket file was left alone");

    starting.exit();
    let (_listener, _lock) = server::bind(&h.ctx).expect("a stale socket is replaced");
    UnixStream::connect(&socket)
        .await
        .expect("the new daemon listens");
}

/// The socket path [`starting_daemon`] runs for, set only in its process.
const STARTING_DAEMON_SOCKET: &str = "ARCHON_TEST_STARTING_DAEMON_SOCKET";
const STARTING_DAEMON_LOCKED: &str = "starting daemon: locked";

/// The first daemon of the race, run as [`starting_daemon`] in a process of
/// its own: a lock or listener held here could leak into another test's
/// `git` fork and outlive its release, failing the second `bind`.
struct StartingDaemon {
    child: Child,
    stdout: std::io::BufReader<ChildStdout>,
}

impl StartingDaemon {
    /// Returns once the stale socket exists and the lock is taken.
    fn start(socket: &Path) -> Self {
        let test_binary = std::env::current_exe().expect("the test binary");
        let mut child = Command::new(test_binary)
            .args([
                &common::exact_test_name(module_path!(), "starting_daemon"),
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(STARTING_DAEMON_SOCKET, socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the starting daemon");
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut starting = Self {
            child,
            stdout: std::io::BufReader::new(stdout),
        };
        let mut line = String::new();
        while line.trim_end() != STARTING_DAEMON_LOCKED {
            line.clear();
            let read = starting.stdout.read_line(&mut line).expect("its output");
            assert_ne!(read, 0, "the starting daemon exited before locking");
        }
        starting
    }

    /// Returns once its process, and with it the lock, is gone.
    fn exit(mut self) {
        drop(self.child.stdin.take());
        std::io::copy(&mut self.stdout, &mut std::io::sink()).expect("its output");
        let status = self.child.wait().expect("the starting daemon exits");
        assert!(status.success(), "the starting daemon failed: {status}");
    }
}

/// Leaves a stale socket, takes the daemon lock, and holds it until stdin
/// closes. Only [`StartingDaemon`] runs it; elsewhere it does nothing.
#[test]
#[ignore = "a helper process of a_starting_daemon_keeps_its_socket_from_a_racing_one"]
fn starting_daemon() {
    let Some(socket) = std::env::var_os(STARTING_DAEMON_SOCKET).map(PathBuf::from) else {
        return;
    };
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("stale socket"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(socket.with_extension("lock"))
        .expect("lock file");
    lock.try_lock().expect("the first daemon takes the lock");
    println!("{STARTING_DAEMON_LOCKED}");
    std::io::stdin()
        .read_to_end(&mut Vec::new())
        .expect("wait for stdin to close");
    drop(lock);
}

async fn update_repo(cli: &mut Client, patch: RepoPatch) -> Repo {
    let update = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    match cli.call(update, &mut Vec::new()).await {
        Ok(Reply::Repo(repo)) => repo,
        other => panic!("repo update: {other:?}"),
    }
}

/// Clearing, keeping and setting a repository's optional settings each
/// survive the socket and reach the registry.
#[tokio::test]
async fn repo_settings_clear_keep_and_set_over_a_socket() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    let mut cli = Client::hello(&socket, ClientKind::ArchonCli).await;

    let cleared = update_repo(
        &mut cli,
        RepoPatch {
            base_branch: Clearable::Clear,
            default_pipeline: Clearable::Clear,
            ..RepoPatch::default()
        },
    )
    .await;
    assert_eq!(cleared.base_branch, None);
    assert_eq!(cleared.default_pipeline, None);

    let kept = update_repo(
        &mut cli,
        RepoPatch {
            ship: Some(ShipMode::Local),
            default_pipeline: Clearable::Set(PIPELINE.into()),
            ..RepoPatch::default()
        },
    )
    .await;
    assert_eq!(kept.ship, ShipMode::Local);
    assert_eq!(kept.base_branch, None, "an unchanged field stays cleared");
    assert_eq!(kept.default_pipeline.as_deref(), Some(PIPELINE));

    let set = RepoPatch {
        base_branch: Clearable::Set("main".into()),
        ..RepoPatch::default()
    };
    assert_eq!(
        update_repo(&mut cli, set).await.base_branch.as_deref(),
        Some("main")
    );
    let stored = h.ctx.store.lock().await.repo(&RepoRef(REPO_NAME.into()));
    let stored = stored.expect("registered");
    assert_eq!(stored.base_branch.as_deref(), Some("main"));
    assert_eq!(stored.default_pipeline.as_deref(), Some(PIPELINE));
    serving.abort();
}

/// A client that leaves takes its feed subscription with it, even when no
/// feed item arrives afterwards to find the connection gone.
#[tokio::test]
async fn a_subscription_ends_when_its_client_leaves() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    let mut cli = Client::hello(&socket, ClientKind::ArchonCli).await;
    let subscribe = Request::SubscribeFeed {
        after: None,
        verbose: false,
    };
    let subscribed = cli.call(subscribe, &mut Vec::new()).await;
    assert!(
        matches!(subscribed, Ok(Reply::Subscribed { .. })),
        "{subscribed:?}"
    );
    assert_eq!(h.ctx.feed.receiver_count(), 1);

    drop(cli);
    let gone = async {
        while h.ctx.feed.receiver_count() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, gone)
        .await
        .expect("the subscription ended with its connection");
    serving.abort();
}

/// A socket's `subscribe_changes` answers `subscribed`, then streams each
/// committed change as a `change` frame under its subscription id.
#[tokio::test]
async fn a_socket_subscription_streams_committed_changes() {
    let h = Harness::new().await;
    let socket = h.ctx.paths.socket.clone();
    let (listener, _lock) = server::bind(&h.ctx).expect("bind");
    let serving = tokio::spawn(server::serve(h.ctx.clone(), listener));
    let pump = tokio::spawn(archon_daemon::handlers::changes::pump(h.ctx.clone()));
    let mut cli = Client::hello(&socket, ClientKind::ArchonCli).await;
    let id = cli.send(Request::SubscribeChanges { after: None }).await;
    let sub = match cli.frame().await {
        Some(ServerFrame::Reply {
            id: got,
            result:
                Ok(Reply::Subscribed {
                    id: sub,
                    last_seq: 0,
                }),
        }) if got == id => sub,
        other => panic!("expected subscribed at 0, got {other:?}"),
    };
    let item = h.enqueue("streamed").await;
    match cli.frame().await {
        Some(ServerFrame::Change { sub: got, change }) => {
            assert_eq!((got, change.seq), (sub, 1));
            let told = matches!(&change.subject,
                archon_core::protocol::ChangeSubject::Item { item: id, .. } if *id == item.id);
            assert!(told, "{change:?}");
        }
        other => panic!("expected the item's change, got {other:?}"),
    }
    pump.abort();
    serving.abort();
}
