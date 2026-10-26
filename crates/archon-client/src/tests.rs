//! Client tests against an in-test fake `archond` built on `framing`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use archon_core::id::FeedItemId;
use archon_core::notify::{FeedEntry, FeedItem};
use archon_core::protocol::{
    ClientFrame, ClientKind, ErrorCode, ErrorReply, PROTOCOL_VERSION, Reply, Request, ServerFrame,
    Welcome,
};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

use super::*;
use crate::framing::{read_frame, write_frame};

/// The daemon's side of one accepted connection.
struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Conn {
    fn new(stream: UnixStream) -> Self {
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    async fn recv(&mut self) -> ClientFrame {
        read_frame(&mut self.reader)
            .await
            .expect("a well-formed client frame")
            .expect("the client is still connected")
    }

    async fn reply(&mut self, id: u64, result: Result<Reply, ErrorReply>) {
        let frame = ServerFrame::Reply { id, result };
        write_frame(&mut self.writer, &frame).await.expect("reply");
    }

    async fn feed(&mut self, sub: u64, seq: u64) {
        let frame = ServerFrame::Feed {
            sub,
            item: feed_item(seq),
        };
        write_frame(&mut self.writer, &frame).await.expect("feed");
    }

    /// Answer the client's `hello`, checking it is the first frame.
    async fn welcome(&mut self) {
        let hello = self.recv().await;
        assert_eq!(hello.id, 1, "hello is the first request");
        let Request::Hello {
            protocol_version,
            client,
            ..
        } = hello.request
        else {
            panic!("expected hello first, got {:?}", hello.request);
        };
        assert_eq!(protocol_version, PROTOCOL_VERSION);
        assert_eq!(client, ClientKind::ArchonCli);
        self.reply(1, Ok(Reply::Welcome(welcome()))).await;
    }
}

fn welcome() -> Welcome {
    Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: "fake".into(),
        pid: 42,
    }
}

fn feed_item(seq: u64) -> FeedItem {
    FeedItem {
        id: FeedItemId::new(),
        seq,
        at: archon_core::now(),
        entry: FeedEntry::System {
            text: format!("item {seq}"),
        },
    }
}

fn listen(dir: &Path) -> (PathBuf, UnixListener) {
    let socket = dir.join("archond.sock");
    let listener = UnixListener::bind(&socket).expect("bind the fake daemon");
    (socket, listener)
}

async fn accept(listener: &UnixListener) -> Conn {
    let (stream, _) = listener.accept().await.expect("accept");
    Conn::new(stream)
}

/// A connected client and the daemon's end of its connection.
async fn pair(dir: &Path) -> (ArchonClient, Conn, UnixListener) {
    let (socket, listener) = listen(dir);
    let server = async {
        let mut conn = accept(&listener).await;
        conn.welcome().await;
        conn
    };
    let (client, conn) = tokio::join!(
        ArchonClient::connect(&socket, ClientKind::ArchonCli),
        server
    );
    (client.expect("connect"), conn, listener)
}

/// A client acting for an agent forwards each request for it; one acting
/// for an unnamed agent sends only ordinary reads.
#[tokio::test]
async fn a_client_acting_for_an_agent_forwards_its_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let agent = AetherAgentId::parse("agt_01STAGE").unwrap();
    let client = client.acting_for(Principal::Agent(agent.clone()));
    let server = async {
        let frame = conn.recv().await;
        conn.reply(frame.id, Ok(Reply::Ok)).await;
        frame.request
    };
    let (reply, sent) = tokio::join!(client.request(Request::Shutdown), server);
    assert_eq!(reply.unwrap(), Reply::Ok);
    let forwarded = Request::ForAgent {
        caller: agent,
        request: Box::new(Request::Shutdown),
    };
    assert_eq!(sent, forwarded);

    let client = client.acting_for(Principal::UnnamedAgent);
    let refused = client.call(Request::CheckUpdates).await.unwrap();
    assert_eq!(refused.unwrap_err().code, ErrorCode::Forbidden);
    let refused = client.request(Request::Shutdown).await.unwrap_err();
    assert!(refused.to_string().contains("not named"), "{refused}");
    let server = async {
        let frame = conn.recv().await;
        conn.reply(frame.id, Ok(Reply::Repos(vec![]))).await;
        frame.request
    };
    let (reply, sent) = tokio::join!(client.request(Request::RepoList), server);
    assert_eq!(reply.unwrap(), Reply::Repos(vec![]));
    assert_eq!(sent, Request::RepoList, "a read goes as it is");
}

#[tokio::test]
async fn replies_are_routed_by_id_even_out_of_order() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    assert_eq!(client.welcome(), &welcome());
    let server = async {
        let first = conn.recv().await;
        let second = conn.recv().await;
        assert_ne!(first.id, second.id);
        // Answer in reverse order; each caller must still get its own reply.
        for frame in [second, first] {
            let reply = match frame.request {
                Request::RepoList => Reply::Repos(vec![]),
                Request::ListQuestions { .. } => Reply::Questions(vec![]),
                other => panic!("unexpected {other:?}"),
            };
            conn.reply(frame.id, Ok(reply)).await;
        }
    };
    let (repos, questions, ()) = tokio::join!(
        client.request(Request::RepoList),
        client.request(Request::ListQuestions { open_only: true }),
        server
    );
    assert_eq!(repos.unwrap(), Reply::Repos(vec![]));
    assert_eq!(questions.unwrap(), Reply::Questions(vec![]));
}

#[tokio::test]
async fn refusals_keep_their_code_and_message() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let refusal = ErrorReply {
        code: ErrorCode::Policy,
        message: "model policy: model x/y is not allowed".into(),
    };
    let server = async {
        for _ in 0..2 {
            let frame = conn.recv().await;
            conn.reply(frame.id, Err(refusal.clone())).await;
        }
    };
    let (raw, flattened, ()) = tokio::join!(
        client.call(Request::Status(Default::default())),
        client.request(Request::Status(Default::default())),
        server
    );
    assert_eq!(raw.expect("the transport is fine"), Err(refusal));
    let error = flattened.expect_err("a refusal is an error");
    assert!(matches!(error, ArchonError::Policy(_)), "{error:?}");
    assert_eq!(error.to_string(), "model policy: model x/y is not allowed");
}

#[test]
fn a_refusal_reads_as_the_daemons_own_error() {
    let daemon_errors = [
        ArchonError::Invalid("repo name is empty".into()),
        ArchonError::NotFound("queue item qi_1".into()),
        ArchonError::Forbidden("operator only".into()),
        ArchonError::InvalidTransition("item is done".into()),
        ArchonError::Unavailable("not connected to aetherd".into()),
    ];
    for daemon_error in daemon_errors {
        let local = refusal_error(ErrorReply::from(&daemon_error));
        assert_eq!(local.to_string(), daemon_error.to_string());
        assert_eq!(
            std::mem::discriminant(&local),
            std::mem::discriminant(&daemon_error),
            "{local:?}"
        );
    }
    let internal = refusal_error(ErrorReply::from(&ArchonError::Store("disk full".into())));
    assert_eq!(
        internal.to_string(),
        "protocol: archond failed: store: disk full"
    );
}

#[tokio::test]
async fn nothing_listening_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("absent.sock");
    let error = ArchonClient::connect(&socket, ClientKind::ArchonCli)
        .await
        .expect_err("nothing listens");
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("absent.sock"), "{error}");
}

#[tokio::test]
async fn a_disconnect_fails_waiters_with_the_daemons_notice() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let server = async move {
        let _unanswered = conn.recv().await;
        let notice = ErrorReply {
            code: ErrorCode::Unavailable,
            message: "archond is shutting down".into(),
        };
        conn.reply(0, Err(notice)).await;
        drop(conn);
    };
    let (answer, ()) = tokio::join!(client.request(Request::Status(Default::default())), server);
    let error = answer.expect_err("the connection dropped");
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("shutting down"), "{error}");
    assert!(client.is_closed());
    let later = client
        .request(Request::Status(Default::default()))
        .await
        .expect_err("still dead");
    assert!(later.to_string().contains("shutting down"), "{later}");
}

/// Subscribe as feed `sub`, answering on the fake daemon's side.
async fn subscribe(client: &ArchonClient, conn: &mut Conn, sub: u64) -> FeedSubscription {
    let server = async {
        let frame = conn.recv().await;
        let reply = Reply::Subscribed {
            id: sub,
            last_seq: 0,
        };
        conn.reply(frame.id, Ok(reply)).await;
    };
    let (subscription, ()) = tokio::join!(client.subscribe_feed(None, false), server);
    subscription.expect("subscribed")
}

#[tokio::test]
async fn a_notice_on_an_open_socket_ends_every_subscription_and_waiter() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let mut first = subscribe(&client, &mut conn, 1).await;
    let mut second = subscribe(&client, &mut conn, 2).await;
    conn.feed(1, 1).await;
    let notice = "feed subscription 1 ended: store failed";
    let server = async {
        let _unanswered = conn.recv().await;
        let refusal = ErrorReply {
            code: ErrorCode::Internal,
            message: notice.into(),
        };
        conn.reply(0, Err(refusal)).await;
    };
    // `conn` stays open: archond ends the feed without closing the socket.
    let within = Duration::from_secs(5);
    let (answer, ()) = tokio::join!(
        tokio::time::timeout(within, client.request(Request::Status(Default::default()))),
        server
    );
    let error = answer
        .expect("not left waiting")
        .expect_err("the notice fails it");
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains(notice), "{error}");
    assert_eq!(
        first.items.recv().await.unwrap().seq,
        1,
        "accepted items drain"
    );
    for subscription in [&mut first, &mut second] {
        let end = tokio::time::timeout(within, subscription.items.recv()).await;
        assert!(end.expect("not left waiting").is_none(), "the stream ends");
        let reason = subscription.close_reason();
        assert!(reason.contains(notice), "{reason}");
    }
    assert!(client.is_closed(), "callers see they must reconnect");
    let later = client
        .request(Request::Status(Default::default()))
        .await
        .expect_err("still dead");
    assert!(later.to_string().contains(notice), "{later}");
    drop(conn);
}

#[tokio::test]
async fn an_incompatible_daemon_is_explained() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, listener) = listen(dir.path());
    let server = async {
        let mut conn = accept(&listener).await;
        let hello = conn.recv().await;
        let refusal = ErrorReply {
            code: ErrorCode::Incompatible,
            message: format!("archond speaks protocol {}", PROTOCOL_VERSION + 1),
        };
        conn.reply(hello.id, Err(refusal)).await;
    };
    let (client, ()) = tokio::join!(
        ArchonClient::connect(&socket, ClientKind::ArchonCli),
        server
    );
    let error = client.expect_err("refused");
    let text = error.to_string();
    let ours = format!("protocol {PROTOCOL_VERSION}");
    let theirs = format!("protocol {}", PROTOCOL_VERSION + 1);
    assert!(text.contains(&ours) && text.contains(&theirs), "{text}");
    assert!(text.contains("archon daemon stop"), "{text}");
}

#[tokio::test]
async fn feed_items_arrive_in_order_including_those_ahead_of_the_reply() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let server = async move {
        let frame = conn.recv().await;
        assert_eq!(
            frame.request,
            Request::SubscribeFeed {
                after: Some(4),
                verbose: true
            }
        );
        conn.feed(9, 5).await;
        conn.reply(frame.id, Ok(Reply::Subscribed { id: 9, last_seq: 6 }))
            .await;
        conn.feed(9, 6).await;
        conn
    };
    let (subscription, conn) = tokio::join!(client.subscribe_feed(Some(4), true), server);
    let mut subscription = subscription.expect("subscribed");
    assert_eq!((subscription.id, subscription.last_seq), (9, 6));
    assert_eq!(subscription.items.recv().await.unwrap().seq, 5);
    assert_eq!(subscription.items.recv().await.unwrap().seq, 6);
    drop(conn);
    assert!(
        subscription.items.recv().await.is_none(),
        "closed with the connection"
    );
    assert!(
        subscription.close_reason().contains("closed"),
        "{}",
        subscription.close_reason()
    );
}

#[tokio::test]
async fn a_feed_that_goes_backwards_is_closed_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let server = async {
        let frame = conn.recv().await;
        conn.reply(frame.id, Ok(Reply::Subscribed { id: 1, last_seq: 0 }))
            .await;
        conn.feed(1, 3).await;
        conn.feed(1, 2).await;
    };
    let (subscription, ()) = tokio::join!(client.subscribe_feed(None, false), server);
    let mut subscription = subscription.expect("subscribed");
    assert_eq!(subscription.items.recv().await.unwrap().seq, 3);
    assert!(subscription.items.recv().await.is_none());
    assert!(subscription.close_reason().contains("backwards"));
    assert!(
        !client.is_closed(),
        "one bad stream does not kill the connection"
    );
}

#[tokio::test]
async fn a_silent_daemon_times_out_the_request_not_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let (client, mut conn, _listener) = pair(dir.path()).await;
    let quick = Duration::from_millis(50);
    let answer = client
        .shared
        .call(Request::Status(Default::default()), None, quick)
        .await;
    let error = answer.expect_err("nobody answered");
    assert!(error.to_string().contains("`status`"), "{error}");
    // The late reply is dropped as an orphan; the next request still works.
    let late = conn.recv().await;
    conn.reply(late.id, Ok(Reply::Ok)).await;
    let server = async {
        let frame = conn.recv().await;
        conn.reply(frame.id, Ok(Reply::Ok)).await;
    };
    let (answer, ()) = tokio::join!(client.request(Request::Status(Default::default())), server);
    assert_eq!(answer.unwrap(), Reply::Ok);
}

#[tokio::test]
async fn a_daemon_that_stops_reading_times_out_queued_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (client, _unread, _listener) = pair(dir.path()).await;
    let patient = Duration::from_secs(60);
    let mut stalled = tokio::task::JoinSet::new();
    // Far over the socket buffer, so the writer blocks on it...
    let big = Request::Say {
        text: "x".repeat(8 << 20),
        images: Vec::new(),
    };
    let shared = client.shared.clone();
    stalled.spawn(async move { shared.call(big, None, patient).await });
    // ...and these fill the write queue behind it.
    for _ in 0..=connection::WRITE_QUEUE {
        let shared = client.shared.clone();
        stalled.spawn(async move {
            shared
                .call(Request::Status(Default::default()), None, patient)
                .await
        });
    }
    // Every task, the writer included, runs until it blocks.
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }
    let quick = Duration::from_millis(100);
    let queued = client
        .shared
        .call(Request::Status(Default::default()), None, quick);
    let answer = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("the request's own timeout covers queueing");
    let error = answer.expect_err("nothing was sent or answered");
    assert!(error.to_string().contains("`status`"), "{error}");
    assert!(!client.is_closed(), "a slow daemon is not a dead one");
}

/// A stand-in `archon` binary.
fn fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    archon_core::testing::write_script(&path, &format!("#!/bin/sh\n{body}\n"));
    path
}

#[tokio::test]
async fn a_failed_start_reports_only_its_own_part_of_the_startup_log() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("archond.sock");
    let log = archon_core::paths::startup_log_for(&socket);
    std::fs::write(&log, "an earlier session's stale error\n").unwrap();
    let mark = archon_core::paths::STARTUP_LOG_MARK;
    let failing = fake_bin(
        dir.path(),
        "archon-failing",
        &format!("echo '{mark} at now ---' >> '{}'\nexit 3", log.display()),
    );
    let quick = Duration::from_millis(400);

    let error = start::connect_or_start_within(&socket, ClientKind::ArchonCli, &failing, quick)
        .await
        .expect_err("the launcher failed");
    let text = error.to_string();
    assert!(
        text.contains("nothing since this start"),
        "a start that wrote nothing says so: {text}"
    );
    assert!(
        !text.contains("stale error"),
        "an earlier session's output is not this start's cause: {text}"
    );

    let more = format!(
        "echo 'config.toml: unknown key [bogus]' >> '{}'",
        log.display()
    );
    let failing = fake_bin(
        dir.path(),
        "archon-failing-again",
        &format!("{more}\nexit 3"),
    );
    let error = start::connect_or_start_within(&socket, ClientKind::ArchonCli, &failing, quick)
        .await
        .expect_err("the launcher failed");
    assert!(error.to_string().contains("unknown key [bogus]"), "{error}");
}

#[tokio::test]
async fn connect_or_start_spawns_the_daemon_and_reports_its_startup_log() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let failing = fake_bin(
        dir.path(),
        "archon-failing",
        "echo 'config.toml: unknown key [bogus]' > \"${ARCHON_SOCKET%.sock}.log\"\nexit 3",
    );
    let idle = fake_bin(dir.path(), "archon-idle", "exit 0");
    let lost = dir.path().join("lost");
    let losing = fake_bin(
        dir.path(),
        "archon-losing",
        &format!("touch '{}'\nexit 1", lost.display()),
    );
    let starting = fake_bin(
        dir.path(),
        "archon-starting",
        &format!("echo \"$* $ARCHON_SOCKET\" > '{}'", marker.display()),
    );
    let quick = Duration::from_millis(400);
    let socket = dir.path().join("archond.sock");

    let error = start::connect_or_start_within(&socket, ClientKind::ArchonCli, &failing, quick)
        .await
        .expect_err("the launcher failed");
    let text = error.to_string();
    assert!(
        text.contains("exit status: 3") && text.contains("unknown key [bogus]"),
        "{text}"
    );

    let error = start::connect_or_start_within(&socket, ClientKind::ArchonCli, &idle, quick)
        .await
        .expect_err("nothing ever listens");
    assert!(
        error.to_string().contains("did not listen within"),
        "{error}"
    );

    // Two clients started a daemon at once: this one's launcher lost and
    // exited, and the winner listens a little later. That one serves.
    let race_socket = dir.path().join("race.sock");
    let (lost_flag, race) = (lost.clone(), race_socket.clone());
    let winner = tokio::spawn(async move {
        while !lost_flag.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let listener = UnixListener::bind(&race).unwrap();
        accept(&listener).await.welcome().await;
        listener
    });
    let wait = Duration::from_secs(5);
    let started =
        start::connect_or_start_within(&race_socket, ClientKind::ArchonCli, &losing, wait).await;
    let client = started.expect("the winning daemon serves the losing client");
    assert_eq!(client.welcome().pid, 42);
    let _winner = winner.await.expect("the winner accepted");

    let daemon = async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let listener = UnixListener::bind(&socket).unwrap();
        accept(&listener).await.welcome().await;
        listener
    };
    let started = start::connect_or_start_within(&socket, ClientKind::ArchonCli, &starting, wait);
    let (client, _listener) = tokio::join!(started, daemon);
    assert_eq!(client.expect("started").welcome().pid, 42);
    let spawned = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(
        spawned.trim(),
        format!("daemon run --detach {}", socket.display())
    );
}

#[tokio::test]
async fn a_launcher_whose_daemon_exited_fails_fast_with_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let exited = fake_bin(
        dir.path(),
        "archon-exited",
        "echo 'archond (pid 7) exited with exit status: 1 before it listened' >&2\nexit 1",
    );
    let socket = dir.path().join("archond.sock");
    let started = tokio::time::Instant::now();
    let error = start::connect_or_start_within(
        &socket,
        ClientKind::ArchonCli,
        &exited,
        Duration::from_secs(30),
    )
    .await
    .expect_err("the daemon exited");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited {:?} for a daemon that had exited",
        started.elapsed()
    );
    let text = error.to_string();
    assert!(
        text.contains("exit status: 1") && text.contains("exited with exit status: 1 before it"),
        "{text}"
    );
}
