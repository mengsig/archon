//! `subscribe_design` over the remote listener: it reaches `Backend::design`
//! with the session and the device, under the subscription cap.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_core::design::DesignSessionId;
use archon_core::error::ArchonError;
use archon_core::protocol::{
    DesignEvent, DeviceRole, PROTOCOL_VERSION, Reply, Request, ServerFrame, WelcomeDevice,
};
use archon_core::remote::{RemoteChange, RemoteConfig};
use archon_remote::{Backend, DesignSubscription, FeedSubscription, Remote};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

type Socket = tokio_tungstenite::WebSocketStream<TcpStream>;

#[derive(Default)]
struct Fake {
    subscribed: Mutex<Vec<DesignSubscription>>,
}

#[async_trait::async_trait]
impl Backend for Fake {
    async fn handle(&self, _: &WelcomeDevice, _: Request) -> Result<Reply, ArchonError> {
        Ok(Reply::Ok)
    }

    async fn feed(&self, _: FeedSubscription, _: mpsc::Sender<ServerFrame>) {}

    async fn design(&self, subscription: DesignSubscription, frames: mpsc::Sender<ServerFrame>) {
        self.subscribed.lock().unwrap().push(subscription.clone());
        let reply = ServerFrame::Reply {
            id: subscription.request,
            result: Ok(Reply::DesignDone),
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
        vec![archon_core::protocol::feature::DESIGN_SESSIONS]
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
            "req": 1, "cmd": "hello", "protocol_version": PROTOCOL_VERSION,
            "client": "archon-web", "client_version": "test",
            "features": [],
        });
        let welcome = exchange(&mut socket, hello).await;
        assert_eq!(welcome["result"]["Ok"]["reply"], "welcome", "{welcome}");
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
    assert_eq!(reply["result"]["Ok"]["reply"], "design_done", "{reply}");
    let event = next_frame(&mut socket).await;
    assert_eq!(event["frame"], "design", "{event}");
    assert_eq!(event["event"]["event"], "toast", "{event}");
    assert_eq!(event["sub"], 1, "{event}");
    let seen = harness.backend.subscribed.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].session, session);
    assert_eq!(seen[0].request, 2);
    assert_eq!(seen[0].id, 1);
    assert_eq!(seen[0].device.role, DeviceRole::Operator);
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
        assert_eq!(reply["result"]["Ok"]["reply"], "design_done", "{reply}");
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
