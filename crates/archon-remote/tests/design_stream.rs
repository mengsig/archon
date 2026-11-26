//! `subscribe_design` over the remote listener: it reaches `Backend::design`
//! with the session and the device, under the subscription cap.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_core::design::DesignSessionId;
use archon_core::error::ArchonError;
use archon_core::protocol::{
    DesignEvent, DeviceRole, PROTOCOL_VERSION, Reply, Request, ServerFrame, WelcomeDevice,
};
use archon_core::remote::{RemoteChange, RemoteConfig};
use archon_remote::{Backend, ChangeSubscription, DesignSubscription, FeedSubscription, Remote};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

type Socket = tokio_tungstenite::WebSocketStream<TcpStream>;

#[derive(Default)]
struct Fake {
    subscribed: Mutex<Vec<(DesignSubscription, watch::Receiver<u64>)>>,
    handled: Mutex<Vec<Request>>,
    /// Requests refused unrun past the in-flight cap.
    refused: Mutex<Vec<Request>>,
    /// Admitted requests that have not ended yet.
    admitted: Arc<AtomicUsize>,
}

/// Counts an admitted request until it ends.
struct Admission(Arc<AtomicUsize>);

impl Drop for Admission {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Backend for Fake {
    async fn handle(&self, _: &WelcomeDevice, request: Request) -> Result<Reply, ArchonError> {
        let stalls = matches!(request, Request::DesignInput { .. });
        self.handled.lock().unwrap().push(request);
        if stalls {
            // A preview that never answers: each input holds its slot.
            std::future::pending::<()>().await;
        }
        Ok(Reply::Ok)
    }

    fn refused(&self, request: &Request) {
        self.refused.lock().unwrap().push(request.clone());
    }

    fn admitted(&self, _request: &Request) -> Option<Box<dyn Send>> {
        self.admitted.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Admission(Arc::clone(&self.admitted))))
    }

    async fn feed(&self, sub: FeedSubscription, frames: mpsc::Sender<ServerFrame>) {
        hold_subscription(sub.request, sub.id, frames).await;
    }

    async fn changes(&self, sub: ChangeSubscription, frames: mpsc::Sender<ServerFrame>) {
        hold_subscription(sub.request, sub.id, frames).await;
    }

    async fn design(
        &self,
        subscription: DesignSubscription,
        acked: watch::Receiver<u64>,
        frames: mpsc::Sender<ServerFrame>,
    ) {
        self.subscribed
            .lock()
            .unwrap()
            .push((subscription.clone(), acked));
        let reply = ServerFrame::Reply {
            id: subscription.request,
            result: Ok(Reply::Subscribed {
                id: subscription.id,
                last_seq: 0,
            }),
        };
        let _ = frames.send(reply).await;
        let event = ServerFrame::Design {
            sub: subscription.id,
            event: DesignEvent::Toast {
                message: "hello".into(),
            },
        };
        let _ = frames.send(event).await;
        // Hold the slot until the connection closes.
        frames.closed().await;
    }

    fn features(&self) -> Vec<&'static str> {
        use archon_core::protocol::feature;
        vec![
            feature::DESIGN_SESSIONS,
            feature::DESIGN_ITERATIONS,
            feature::DESIGN_UNSUBSCRIBE,
        ]
    }
}

async fn hold_subscription(request: u64, id: u64, frames: mpsc::Sender<ServerFrame>) {
    let reply = ServerFrame::Reply {
        id: request,
        result: Ok(Reply::Subscribed { id, last_seq: 0 }),
    };
    if frames.send(reply).await.is_ok() {
        frames.closed().await;
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    remote: Remote,
    backend: Arc<Fake>,
    port: u16,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = RemoteConfig {
            address: "127.0.0.1:0".into(),
            ..RemoteConfig::default()
        };
        let remote = Remote::open(config, dir.path(), "9.9.9").unwrap();
        let backend = Arc::new(Fake::default());
        remote.attach(backend.clone());
        let change = RemoteChange {
            enabled: Some(true),
            ..RemoteChange::default()
        };
        let view = remote.configure(change).await.unwrap();
        let port = view
            .listening
            .unwrap()
            .rsplit_once(':')
            .unwrap()
            .1
            .parse()
            .unwrap();
        Self {
            _dir: dir,
            remote,
            backend,
            port,
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    async fn cookie(&self, role: DeviceRole) -> String {
        let code = self.remote.pair(role).unwrap().code;
        let body = serde_json::json!({"code": code, "device_name": "Test phone"}).to_string();
        let origin = format!("http://{}", self.host());
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let head = format!(
            "POST /api/pair/claim HTTP/1.1\r\nConnection: close\r\nHost: {}\r\nOrigin: {origin}\r\n\
             Content-Type: application/json\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{body}",
            self.host(),
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let line = text
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
            .unwrap_or_else(|| panic!("no credential: {text}"));
        line.split_once(':')
            .unwrap()
            .1
            .trim()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn socket(&self, role: DeviceRole) -> Socket {
        let cookie = self.cookie(role).await;
        self.socket_with(&cookie, PROTOCOL_VERSION).await
    }

    async fn socket_with(&self, cookie: &str, version: u32) -> Socket {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let mut request = format!("ws://{}/ws", self.host())
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert("Origin", format!("http://{}", self.host()).parse().unwrap());
        headers.insert("Cookie", cookie.parse().unwrap());
        headers.insert("Sec-WebSocket-Protocol", "archon".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        let hello = serde_json::json!({
            "req": 1, "cmd": "hello", "protocol_version": version,
            "client": "archon-web", "client_version": "test",
            "features": [],
        });
        let welcome = exchange(&mut socket, hello).await;
        assert_eq!(welcome["result"]["Ok"]["reply"], "welcome", "{welcome}");
        let welcome = &welcome["result"]["Ok"]["data"];
        assert_eq!(welcome["protocol_version"], version);
        let offered = match version {
            23 | 24 => serde_json::json!([]),
            25 => serde_json::json!(["design_sessions", "design_unsubscribe"]),
            _ => serde_json::json!(["design_sessions", "design_iterations", "design_unsubscribe"]),
        };
        assert_eq!(welcome["features"], offered);
        socket
    }
}

async fn next_frame(socket: &mut Socket) -> serde_json::Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame arrives")
            .expect("the socket is open")
            .unwrap();
        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

async fn exchange(socket: &mut Socket, frame: serde_json::Value) -> serde_json::Value {
    socket.send(Message::text(frame.to_string())).await.unwrap();
    next_frame(socket).await
}

#[tokio::test]
async fn subscribe_design_reaches_the_backend_with_the_session() {
    let harness = Harness::start().await;
    let mut socket = harness.socket(DeviceRole::Operator).await;
    let session = DesignSessionId::new();
    let request = serde_json::json!({"req": 2, "cmd": "subscribe_design", "session": session});
    let reply = exchange(&mut socket, request).await;
    assert_eq!(reply["id"], 2, "{reply}");
    assert_eq!(reply["result"]["Ok"]["reply"], "subscribed", "{reply}");
    assert_eq!(reply["result"]["Ok"]["data"]["id"], 1, "{reply}");
    let event = next_frame(&mut socket).await;
    assert_eq!(event["frame"], "design", "{event}");
    assert_eq!(event["event"]["event"], "toast", "{event}");
    assert_eq!(event["sub"], 1, "{event}");
    let seen = harness.backend.subscribed.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (seen, _) = &seen[0];
    assert_eq!(seen.session, session);
    assert_eq!(seen.request, 2);
    assert_eq!(seen.id, 1);
    assert_eq!(seen.device.role, DeviceRole::Operator);
}

#[tokio::test]
async fn same_device_connections_acknowledge_their_own_subscriptions() {
    let harness = Harness::start().await;
    let cookie = harness.cookie(DeviceRole::Operator).await;
    let mut first = harness.socket_with(&cookie, PROTOCOL_VERSION).await;
    let mut second = harness.socket_with(&cookie, PROTOCOL_VERSION).await;
    let session = DesignSessionId::new();
    for socket in [&mut first, &mut second] {
        let subscribed = exchange(
            socket,
            serde_json::json!({
                "req": 2, "cmd": "subscribe_design", "session": session,
            }),
        )
        .await;
        assert_eq!(subscribed["result"]["Ok"]["data"]["id"], 1);
        assert_eq!(next_frame(socket).await["sub"], 1);
    }
    let subscriptions = harness.backend.subscribed.lock().unwrap().clone();
    let [(left, left_ack), (right, right_ack)] = subscriptions.as_slice() else {
        panic!("expected both subscriptions");
    };
    assert_eq!(left.device.id, right.device.id);
    assert_eq!(left.id, right.id);
    for (use_first, seq, expected) in [(true, 3, (3, 0)), (false, 5, (3, 5)), (true, 2, (3, 5))] {
        let socket = if use_first { &mut first } else { &mut second };
        let reply = exchange(
            socket,
            serde_json::json!({
                "req": 3, "cmd": "design_frame_ack", "sub": 1, "seq": seq,
            }),
        )
        .await;
        assert_eq!(reply["result"]["Ok"]["reply"], "design_done", "{reply}");
        assert_eq!((*left_ack.borrow(), *right_ack.borrow()), expected);
    }
}

#[tokio::test]
async fn design_requests_follow_the_accepted_protocol() {
    let harness = Harness::start().await;
    let cookie = harness.cookie(DeviceRole::Operator).await;
    let session = DesignSessionId::new();
    for version in 23..=26 {
        let mut socket = harness.socket_with(&cookie, version).await;
        let plain = serde_json::json!({
            "req": 2, "cmd": "design_note_add", "session": session,
            "client_key": "k", "text": "note", "send_picture": false,
        });
        let mut follow = plain.clone();
        follow["req"] = 3.into();
        follow["follows"] = "D1".into();
        let continued = serde_json::json!({
            "req": 4, "cmd": "design_note_continue_from", "id": "D1",
            "item": "qi_01M621D9ZBJPTMDTV26KKQNBE1",
        });
        for (request, required) in [(plain, 25), (follow, 26), (continued, 26)] {
            let reply = exchange(&mut socket, request).await;
            if version < required {
                assert_eq!(reply["result"]["Err"]["code"], "incompatible", "{reply}");
            } else {
                assert_eq!(reply["result"]["Ok"]["reply"], "ok", "{reply}");
            }
        }
    }
    assert_eq!(harness.backend.handled.lock().unwrap().len(), 4);
    assert!(harness.backend.subscribed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn design_streams_count_toward_the_subscription_cap_and_observers_get_none() {
    let harness = Harness::start().await;
    let mut socket = harness.socket(DeviceRole::Operator).await;
    for n in 0..4 {
        let request = serde_json::json!({
            "req": 10 + n, "cmd": "subscribe_design", "session": DesignSessionId::new(),
        });
        let reply = exchange(&mut socket, request).await;
        assert_eq!(reply["result"]["Ok"]["reply"], "subscribed", "{reply}");
        let _toast = next_frame(&mut socket).await;
    }
    let fifth = serde_json::json!({
        "req": 20, "cmd": "subscribe_design", "session": DesignSessionId::new(),
    });
    let refused = exchange(&mut socket, fifth).await;
    assert_eq!(
        refused["result"]["Err"]["code"], "invalid_argument",
        "{refused}"
    );

    let mut observer = harness.socket(DeviceRole::Observer).await;
    let request = serde_json::json!({
        "req": 2, "cmd": "subscribe_design", "session": DesignSessionId::new(),
    });
    let refused = exchange(&mut observer, request).await;
    assert_eq!(refused["result"]["Err"]["code"], "forbidden", "{refused}");
    assert_eq!(harness.backend.subscribed.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn leaving_the_view_frees_its_slot_with_feed_and_changes_still_live() {
    use serde_json::json;
    let harness = Harness::start().await;
    let mut socket = harness.socket(DeviceRole::Operator).await;
    for (req, cmd) in [(2, "subscribe_feed"), (3, "subscribe_changes")] {
        let reply = exchange(&mut socket, json!({"req":req,"cmd":cmd,"verbose":false})).await;
        assert_eq!(reply["result"]["Ok"]["reply"], "subscribed", "{reply}");
    }
    let session = DesignSessionId::new();
    for n in 0..8 {
        let reply = exchange(
            &mut socket,
            json!({"req":4+2*n,"cmd":"subscribe_design","session":session}),
        )
        .await;
        let sub = reply["result"]["Ok"]["data"]["id"]
            .as_u64()
            .expect("a slot for this visit");
        assert_eq!(next_frame(&mut socket).await["sub"], sub);
        let done = exchange(
            &mut socket,
            json!({"req":5+2*n,"cmd":"design_unsubscribe","sub":sub}),
        )
        .await;
        assert_eq!(done["result"]["Ok"]["reply"], "design_done");
    }
}

/// Input refused past the in-flight cap never ran, so whatever release it
/// carried is lost: the backend hears of it to let go of held input.
#[tokio::test]
async fn input_refused_past_the_in_flight_cap_is_reported_to_the_backend() {
    let harness = Harness::start().await;
    let mut socket = harness.socket(DeviceRole::Operator).await;
    let session = DesignSessionId::new();
    for n in 0..16 {
        let request = serde_json::json!({
            "req": 10 + n, "cmd": "design_input", "session": session, "events": [],
        });
        socket
            .send(Message::text(request.to_string()))
            .await
            .unwrap();
    }
    let over = serde_json::json!({
        "req": 40, "cmd": "design_input", "session": session, "events": [],
    });
    let refused = exchange(&mut socket, over).await;
    assert_eq!(refused["result"]["Err"]["code"], "unavailable", "{refused}");
    let told = harness.backend.refused.lock().unwrap().clone();
    assert!(
        matches!(told.as_slice(), [Request::DesignInput { session: s, .. }] if *s == session),
        "{told:?}"
    );
}

/// An admitted request is counted before the next frame is read, so a stream
/// ended by a later frame can wait for it, and it ends when its connection does.
#[tokio::test]
async fn an_admitted_request_is_held_until_it_ends_with_its_connection() {
    let harness = Harness::start().await;
    let mut socket = harness.socket(DeviceRole::Operator).await;
    let session = DesignSessionId::new();
    let input = serde_json::json!({
        "req": 10, "cmd": "design_input", "session": session, "events": [],
    });
    socket.send(Message::text(input.to_string())).await.unwrap();
    let unsubscribe = serde_json::json!({"req": 11, "cmd": "design_unsubscribe", "sub": 99});
    let done = exchange(&mut socket, unsubscribe).await;
    assert_eq!(done["id"], 11, "{done}");
    assert_eq!(done["result"]["Ok"]["reply"], "design_done", "{done}");
    assert_eq!(
        harness.backend.admitted.load(Ordering::SeqCst),
        1,
        "the stalled input was admitted before the later frame was answered"
    );
    socket.close(None).await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while harness.backend.admitted.load(Ordering::SeqCst) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the input never ended"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
