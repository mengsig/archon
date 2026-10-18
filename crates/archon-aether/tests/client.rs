//! `AetherClient` against a scripted daemon: framing, reply routing,
//! timeouts, disconnects, subscription ordering and overflow.

mod common;

use std::time::Duration;

use archon_aether::ClientOptions;
use archon_aether::client::AetherClient;
use archon_aether::events::EventKind;
use archon_aether::fake::{self, Scripted};
use archon_aether::wire::EventQuery;
use archon_core::aether_ids::{AetherAgentId, AetherTaskId};
use archon_core::error::ArchonError;
use common::{AGENT, AGENT_2, TASK, WAIT, completed, connect, connect_with, fake_aether};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

fn agent_id(raw: &str) -> AetherAgentId {
    AetherAgentId::parse(raw).unwrap()
}

fn quick() -> ClientOptions {
    ClientOptions {
        request_timeout: Duration::from_millis(300),
        ..ClientOptions::default()
    }
}

#[tokio::test]
async fn the_handshake_speaks_v24_and_names_archon() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    assert_eq!(client.welcome().protocol_version, 24);
    assert_eq!(client.welcome().min_protocol_version, Some(23));
    assert_eq!(fake.connections_accepted(), 1, "no fallback needed");
    assert_eq!(client.welcome().instance_id, "fake-instance-1");
    assert!(client.is_connected());
    client.ping().await.unwrap();
    assert_eq!(fake.received_named("ping").len(), 1);
}

#[tokio::test]
async fn an_incompatible_daemon_is_reported_with_its_window() {
    let (_dir, fake) = fake_aether().await;
    fake.set_protocol_window(25, 26);
    let error = AetherClient::connect(&fake.endpoint()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "aether: Archon speaks Aether protocol 14..24; the daemon serves 25..26"
    );
    assert!(
        fake.received().is_empty(),
        "nothing is sent after a refused hello"
    );
    assert_eq!(
        fake.connections_accepted(),
        1,
        "a newer daemon is not retried"
    );
}

/// An aetherd not yet upgraded to 17 refuses the hello; Archon reconnects
/// at the newest protocol it serves (16, 15 or 14) rather than losing Aether.
#[tokio::test]
async fn an_older_aetherd_is_spoken_to_at_the_newest_protocol_it_serves() {
    for (window, spoken) in [((15, 16), 16), ((14, 15), 15), ((13, 14), 14)] {
        let (_dir, fake) = fake_aether().await;
        fake.set_protocol_window(window.0, window.1);
        let client = connect(&fake).await;
        assert_eq!(client.welcome().protocol_version, spoken, "{window:?}");
        assert_eq!(
            fake.connections_accepted(),
            2,
            "refused at 17, then {spoken}"
        );
        client.ping().await.unwrap();
        assert_eq!(fake.received_named("ping").len(), 1);
    }
}

#[tokio::test]
async fn an_aetherd_older_than_archon_speaks_is_refused_with_both_windows() {
    let (_dir, fake) = fake_aether().await;
    fake.set_protocol_window(12, 13);
    let error = AetherClient::connect(&fake.endpoint()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "aether: Archon speaks Aether protocol 14..24; the daemon serves 12..13"
    );
    assert_eq!(fake.connections_accepted(), 1, "nothing it serves to retry");
}

/// A hand-driven peer: answers hello, then lets the test read and write raw lines.
async fn raw_peer(
    dir: &std::path::Path,
) -> (std::path::PathBuf, tokio::task::JoinHandle<Vec<Value>>) {
    let path = dir.join("raw.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut read_frame = async || -> Value {
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
        };
        let hello = read_frame().await;
        let welcome = json!({"frame":"reply","id":hello["req"],"result":{"Ok":{"reply":"welcome",
            "data":{"protocol_version":24,"daemon_version":"x","instance_id":"raw","schema_version":1,"pid":1}}}});
        write
            .write_all(format!("{welcome}\n").as_bytes())
            .await
            .unwrap();
        let (first, second) = (read_frame().await, read_frame().await);
        // Answer in reverse order, with an unrelated notice-free event first.
        for frame in [&second, &first] {
            let id = frame["id"].as_str().unwrap();
            let reply = json!({"frame":"reply","id":frame["req"],"result":{"Ok":{"reply":"agent",
                "data": fake::agent(id, None)}}});
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        vec![hello, first, second]
    });
    (path, peer)
}

#[tokio::test]
async fn replies_route_by_request_id_even_out_of_order() {
    let dir = tempfile::tempdir().unwrap();
    let (path, peer) = raw_peer(dir.path()).await;
    let client = AetherClient::connect(&archon_aether::Endpoint::Unix(path))
        .await
        .unwrap();
    let (a, b) = (agent_id(AGENT), agent_id(AGENT_2));
    let (first, second) = tokio::join!(client.get_agent(&a), client.get_agent(&b));
    assert_eq!(first.unwrap().id, a);
    assert_eq!(second.unwrap().id, b);
    let frames = peer.await.unwrap();
    assert_eq!(frames[0]["cmd"], "hello");
    assert_eq!(frames[0]["client_name"], "archon");
    assert_eq!(frames[0]["features"], json!([]));
    let reqs: Vec<u64> = frames.iter().map(|f| f["req"].as_u64().unwrap()).collect();
    assert_eq!(reqs, [1, 2, 3], "request ids start at 1 and increase");
}

#[tokio::test]
async fn a_silent_daemon_times_out_the_request_but_not_the_connection() {
    let (_dir, fake) = fake_aether().await;
    fake.script("get_task", Scripted::Silent);
    let client = connect_with(&fake, quick()).await;
    let error = client
        .get_task(&AetherTaskId::parse(TASK).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ArchonError::Unavailable(m) if m.contains("did not answer `get_task`"))
    );
    assert!(client.is_connected());
    client.ping().await.expect("the connection still works");
}

#[tokio::test]
async fn refusals_keep_aethers_code() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "get_agent",
        Scripted::refuse("not_found", "agent agt_x not found"),
    );
    fake.script(
        "wake",
        Scripted::refuse("invalid_transition", "agent is running"),
    );
    let client = connect(&fake).await;
    let missing = client.get_agent(&agent_id(AGENT)).await.unwrap_err();
    assert!(matches!(missing, ArchonError::NotFound(_)));
    let refused = client.wake(&agent_id(AGENT)).await.unwrap_err();
    assert!(matches!(refused, ArchonError::InvalidTransition(_)));
    // A refusal, not a lost connection: retrying under the same key is wrong.
    fake.script(
        "cancel",
        Scripted::refuse("unavailable", "provider codex is not configured"),
    );
    let unconfigured = client.cancel(&agent_id(AGENT), "stop").await.unwrap_err();
    assert!(
        matches!(&unconfigured, ArchonError::Aether(m) if m.contains("not configured")),
        "{unconfigured:?}"
    );
    let unscripted = client.list_models().await.unwrap_err();
    assert!(
        unscripted
            .to_string()
            .contains("no reply scripted for `list_models`")
    );
}

#[tokio::test]
async fn an_unexpected_reply_kind_is_a_named_error() {
    let (_dir, fake) = fake_aether().await;
    fake.script("cancel", Scripted::reply("agent", fake::agent(AGENT, None)));
    let client = connect(&fake).await;
    let error = client.cancel(&agent_id(AGENT), "stop").await.unwrap_err();
    assert!(
        matches!(&error, ArchonError::Protocol(m) if m.contains("expected a `ok` reply, got `agent`"))
    );
}

#[tokio::test]
async fn a_disconnect_fails_every_waiter_with_the_notice() {
    let (_dir, fake) = fake_aether().await;
    fake.set_default("get_task", Scripted::Silent);
    let client = connect(&fake).await;
    let mut sub = client.subscribe(None, EventQuery::default()).await.unwrap();
    let task = AetherTaskId::parse(TASK).unwrap();
    let waiting = async { tokio::join!(client.get_task(&task), client.get_task(&task)) };
    let hangup = async {
        fake.wait_for("get_task", 2, WAIT).await;
        fake.hangup(Some(("unavailable", "closed after the idle timeout")));
    };
    let ((first, second), ()) = tokio::join!(waiting, hangup);
    for outcome in [first, second] {
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("closed after the idle timeout"), "{error}");
    }
    let closed = sub.next().await.unwrap_err().to_string();
    assert!(closed.contains("closed after the idle timeout"), "{closed}");
    assert!(!client.is_connected());
    let late = client.ping().await.unwrap_err().to_string();
    assert!(
        late.contains("closed after the idle timeout"),
        "later requests fail fast: {late}"
    );
}

#[tokio::test]
async fn a_bare_eof_is_still_a_named_disconnect() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    fake.hangup(None);
    let error = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(reason) = client.disconnect_reason() {
                return reason;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(error, "aetherd closed the connection");
    assert!(client.ping().await.is_err());
}

#[tokio::test]
async fn replayed_events_that_beat_the_reply_arrive_in_order() {
    let (_dir, fake) = fake_aether().await;
    for n in 1..=3 {
        fake.push_event(completed(&format!("r{n}")));
    }
    let client = connect(&fake).await;
    let mut sub = client
        .subscribe(Some(0), EventQuery::default())
        .await
        .unwrap();
    assert_eq!(sub.last_seq, 0);
    fake.push_event(completed("live"));
    let mut seqs = Vec::new();
    for _ in 0..4 {
        seqs.push(sub.next().await.unwrap().seq);
    }
    assert_eq!(seqs, [1, 2, 3, 4]);
    let subscribe = &fake.received_named("subscribe")[0];
    assert_eq!(subscribe["deltas"], false);
    assert_eq!(subscribe["after"], 0);
}

#[tokio::test]
async fn subscriptions_only_see_their_own_events() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    let only_failures = EventQuery {
        types: vec!["agent.failed".into()],
        ..EventQuery::default()
    };
    let mut failures = client.subscribe(None, only_failures).await.unwrap();
    let mut everything = client.subscribe(None, EventQuery::default()).await.unwrap();
    assert_ne!(failures.id, everything.id);
    fake.push_event(completed("done"));
    let failed = fake::event(
        Some(AGENT),
        None,
        json!({"type": "agent.failed", "error": "boom"}),
    );
    fake.push_event(failed);
    assert_eq!(everything.next().await.unwrap().seq, 1);
    assert_eq!(everything.next().await.unwrap().seq, 2);
    let only = failures.next().await.unwrap();
    assert_eq!(only.seq, 2);
    assert_eq!(
        only.kind,
        EventKind::AgentFailed {
            error: "boom".into()
        }
    );
}

#[tokio::test]
async fn overflow_closes_the_subscription_after_the_accepted_events() {
    let (_dir, fake) = fake_aether().await;
    let options = ClientOptions {
        event_capacity: 2,
        ..ClientOptions::default()
    };
    let client = connect_with(&fake, options).await;
    let mut sub = client.subscribe(None, EventQuery::default()).await.unwrap();
    for n in 1..=5 {
        fake.push_event(completed(&format!("e{n}")));
    }
    // Replies are routed in order behind the events, so this drains them.
    client.ping().await.unwrap();
    assert_eq!(sub.next().await.unwrap().seq, 1);
    assert_eq!(sub.next().await.unwrap().seq, 2);
    let error = sub.next().await.unwrap_err().to_string();
    assert!(error.contains("overflowed its 2-event buffer"), "{error}");
    let unsubscribed = fake.wait_for("unsubscribe", 1, WAIT).await;
    assert_eq!(unsubscribed[0]["id"], sub.id);
    assert!(client.is_connected(), "only the subscription closes");
}

#[tokio::test]
async fn a_replay_larger_than_the_buffer_overflows_before_the_reply() {
    let (_dir, fake) = fake_aether().await;
    for n in 1..=5 {
        fake.push_event(completed(&format!("e{n}")));
    }
    let options = ClientOptions {
        event_capacity: 2,
        ..ClientOptions::default()
    };
    let client = connect_with(&fake, options).await;
    let mut sub = client
        .subscribe(Some(0), EventQuery::default())
        .await
        .unwrap();
    // The events that fit are still handed out, so a pump resubscribing from
    // its cursor moves forward instead of replaying the same overflow forever.
    assert_eq!(sub.next().await.unwrap().seq, 1);
    assert_eq!(sub.next().await.unwrap().seq, 2);
    let error = sub.next().await.unwrap_err().to_string();
    assert!(error.contains("overflowed"), "{error}");
    assert_eq!(fake.wait_for("unsubscribe", 1, WAIT).await.len(), 1);
    let mut resumed = client
        .subscribe(Some(2), EventQuery::default())
        .await
        .unwrap();
    assert_eq!(resumed.next().await.unwrap().seq, 3);
}

#[tokio::test]
async fn a_malformed_known_event_closes_the_subscription_loudly() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    let mut sub = client.subscribe(None, EventQuery::default()).await.unwrap();
    fake.push_event(fake::event(
        Some(AGENT),
        None,
        json!({"type": "agent.failed"}),
    ));
    let error = sub.next().await.unwrap_err().to_string();
    assert!(error.contains("malformed event"), "{error}");
}

#[tokio::test]
async fn the_keepalive_pings_at_a_third_of_the_idle_timeout() {
    let (_dir, fake) = fake_aether().await;
    fake.set_idle_timeout(Some(1));
    let client = connect(&fake).await;
    assert_eq!(client.welcome().idle_timeout_secs, Some(1));
    let pings = fake.wait_for("ping", 2, Duration::from_secs(3)).await;
    assert!(pings.len() >= 2);
}

#[tokio::test]
async fn one_unanswered_ping_does_not_stop_the_keepalive() {
    let (_dir, fake) = fake_aether().await;
    fake.set_idle_timeout(Some(1));
    fake.script("ping", Scripted::Silent);
    let options = ClientOptions {
        request_timeout: Duration::from_millis(200),
        ..ClientOptions::default()
    };
    let client = connect_with(&fake, options).await;
    let pings = fake.wait_for("ping", 3, Duration::from_secs(3)).await;
    assert!(pings.len() >= 3, "pinging resumed after the timed-out ping");
    assert!(client.is_connected());
}

#[tokio::test]
async fn dropping_a_subscription_unsubscribes_it() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    let sub = client.subscribe(None, EventQuery::default()).await.unwrap();
    let id = sub.id;
    drop(sub);
    fake.push_event(completed("nobody listens"));
    let unsubscribed = fake.wait_for("unsubscribe", 1, WAIT).await;
    assert_eq!(unsubscribed[0]["id"], id);
    client.ping().await.expect("the connection is unaffected");
    assert_eq!(fake.live_subscriptions(), 0);
    assert_eq!(
        fake.received_named("unsubscribe").len(),
        1,
        "the late event does not unsubscribe twice"
    );
}

#[tokio::test]
async fn dropping_an_idle_subscription_unsubscribes_it() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    let sub = client.subscribe(None, EventQuery::default()).await.unwrap();
    let id = sub.id;
    assert_eq!(fake.live_subscriptions(), 1);
    drop(sub);
    let unsubscribed = fake.wait_for("unsubscribe", 1, WAIT).await;
    assert_eq!(unsubscribed[0]["id"], id, "no event was needed to notice");
    client.ping().await.expect("the connection is unaffected");
    assert_eq!(fake.live_subscriptions(), 0);
}

#[tokio::test]
async fn a_subscribe_cancelled_as_its_reply_arrives_is_unsubscribed() {
    let (_dir, fake) = fake_aether().await;
    let client = connect(&fake).await;
    let mut subscribing = Box::pin(client.subscribe(None, EventQuery::default()));
    tokio::select! {
        biased;
        _ = &mut subscribing => panic!("nothing has answered the subscribe yet"),
        () = std::future::ready(()) => {}
    }
    // Replies are routed in order, so the `subscribed` reply is already in
    // the caller's mailbox when the ping returns; nobody ever reads it.
    client.ping().await.unwrap();
    assert_eq!(fake.live_subscriptions(), 1);
    drop(subscribing);
    let unsubscribed = fake.wait_for("unsubscribe", 1, WAIT).await;
    assert!(unsubscribed[0]["id"].as_u64().is_some_and(|id| id > 0));
    client.ping().await.expect("the connection is unaffected");
    assert_eq!(fake.live_subscriptions(), 0);
}

/// A peer that answers hello, then never reads again, so writes back up.
async fn stalled_peer(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("stalled.sock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let hello = BufReader::new(read)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let hello: Value = serde_json::from_str(&hello).unwrap();
        let welcome = json!({"frame":"reply","id":hello["req"],"result":{"Ok":{"reply":"welcome",
            "data":{"protocol_version":24,"daemon_version":"x","instance_id":"stalled","schema_version":1,"pid":1}}}});
        write
            .write_all(format!("{welcome}\n").as_bytes())
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    path
}

#[tokio::test]
async fn a_request_queued_behind_a_stalled_writer_still_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = stalled_peer(dir.path()).await;
    let options = ClientOptions {
        request_timeout: Duration::from_millis(200),
        ..ClientOptions::default()
    };
    let endpoint = archon_aether::Endpoint::Unix(path);
    let client = std::sync::Arc::new(
        AetherClient::connect_with(&endpoint, options)
            .await
            .unwrap(),
    );
    let mut calls = tokio::task::JoinSet::new();
    let blocker = client.clone();
    // Far more than a socket buffer holds, so the writer blocks mid-frame.
    calls.spawn(async move {
        let huge = "x".repeat(8 * 1024 * 1024);
        blocker.send_message(&agent_id(AGENT), &huge, "k").await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // More pings than the write queue holds, so the last wait to be queued.
    for _ in 0..80 {
        let pinger = client.clone();
        calls.spawn(async move { pinger.ping().await });
    }
    let outcomes = tokio::time::timeout(Duration::from_secs(3), calls.join_all())
        .await
        .expect("every request gives up within its timeout");
    assert_eq!(outcomes.len(), 81);
    for outcome in outcomes {
        let error = outcome.unwrap_err();
        assert!(
            matches!(&error, ArchonError::Unavailable(m) if m.contains("did not answer")),
            "{error:?}"
        );
    }
    assert!(client.is_connected(), "a slow peer is not a dead one");
}

#[tokio::test]
async fn the_link_stops_handing_out_a_connection_that_died() {
    let (_dir, fake) = fake_aether().await;
    let link = archon_aether::AetherLink::new();
    link.replace(Some(std::sync::Arc::new(connect(&fake).await)))
        .unwrap();
    assert!(link.is_connected());
    fake.hangup(Some(("unavailable", "aetherd is shutting down")));
    let error = tokio::time::timeout(WAIT, async {
        loop {
            if let Err(error) = link.client() {
                return error;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the link notices the drop without a replace");
    assert!(
        matches!(&error, ArchonError::Unavailable(m) if m.contains("shutting down")),
        "{error:?}"
    );
    assert!(!link.is_connected(), "status must not report a dead link");
}
