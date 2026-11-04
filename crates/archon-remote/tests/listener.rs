//! The remote listener on a real loopback port, driven by a raw HTTP/1.1
//! client and a WebSocket client, with a fake archond behind it.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::protocol::{
    DeviceRole, MIN_WEB_PROTOCOL_VERSION, PROTOCOL_VERSION, Reply, Request, ServerFrame, feature,
};
use archon_core::remote::{RemoteChange, RemoteConfig};
use archon_remote::{Backend, ChangeSubscription, FeedSubscription, Remote};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

type Socket = tokio_tungstenite::WebSocketStream<TcpStream>;

/// Answers `ok` to everything and remembers what it was asked.
#[derive(Default)]
struct FakeArchond {
    asked: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Backend for FakeArchond {
    async fn handle(&self, role: DeviceRole, request: Request) -> Result<Reply, ArchonError> {
        assert!(archon_remote::allow::allowed(role, &request), "{request:?}");
        let name = archon_remote::allow::command_name(&request);
        self.asked.lock().unwrap().push(name);
        Ok(Reply::Ok)
    }

    fn features(&self) -> Vec<&'static str> {
        vec![feature::CHANGES]
    }

    async fn feed(&self, subscription: FeedSubscription, frames: mpsc::Sender<ServerFrame>) {
        let subscribed = ServerFrame::Reply {
            id: subscription.request,
            result: Ok(Reply::Subscribed {
                id: subscription.id,
                last_seq: 0,
            }),
        };
        let _ = frames.send(subscribed).await;
    }

    /// Opens, then ends at once, as a lagging or refused stream does.
    async fn changes(&self, subscription: ChangeSubscription, frames: mpsc::Sender<ServerFrame>) {
        let subscribed = ServerFrame::Reply {
            id: subscription.request,
            result: Ok(Reply::Subscribed {
                id: subscription.id,
                last_seq: 0,
            }),
        };
        let _ = frames.send(subscribed).await;
    }
}

struct Listener {
    dir: tempfile::TempDir,
    remote: Remote,
    backend: Arc<FakeArchond>,
    port: u16,
    /// The origin the app is opened at; loopback unless mounted.
    opened_at: Option<String>,
}

/// The origin the Aether listener that mounts archond is reached at.
const AETHER: &str = "https://box.ts.net";

impl Listener {
    async fn start() -> Self {
        Self::start_with(RemoteConfig::default(), None).await
    }

    /// Served under `/archon` by an Aether listener at [`AETHER`].
    async fn mounted() -> Self {
        let config = RemoteConfig {
            mounted_at: Some("/archon".into()),
            trusted_origins: vec![AETHER.into()],
            ..RemoteConfig::default()
        };
        Self::start_with(config, Some(AETHER.into())).await
    }

    async fn start_with(config: RemoteConfig, opened_at: Option<String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = RemoteConfig {
            address: "127.0.0.1:0".into(),
            ..config
        };
        let remote = Remote::open(config, dir.path(), "9.9.9").unwrap();
        let backend = Arc::new(FakeArchond::default());
        remote.attach(backend.clone());
        let view = remote.configure(enable(true)).await.unwrap();
        let listening = view.listening.expect("the listener opened");
        let port = listening.rsplit_once(':').unwrap().1.parse().unwrap();
        Self {
            dir,
            remote,
            backend,
            port,
            opened_at,
        }
    }

    fn origin(&self) -> String {
        self.opened_at
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", self.port))
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// POST a claim from the app's own origin.
    async fn claim(&self, code: &str) -> Response {
        let body = serde_json::json!({"code": code, "device_name": "Test phone"}).to_string();
        let origin = self.origin();
        let headers = [
            ("Origin", origin.as_str()),
            ("Content-Type", "application/json"),
            ("Sec-Fetch-Site", "same-origin"),
        ];
        self.request("POST", "/api/pair/claim", &headers, &body)
            .await
    }

    /// Pair an operator device and return its `archon_device=…` cookie.
    async fn pair(&self) -> String {
        self.pair_as(DeviceRole::Operator).await
    }

    async fn pair_as(&self, role: DeviceRole) -> String {
        let code = self.remote.pair(role).unwrap().code;
        let response = self.claim(&code).await;
        assert_eq!(response.status, 200, "{}", response.body);
        let set = response.header("set-cookie").expect("a credential is set");
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict"),
            "{set}"
        );
        set.split(';').next().unwrap().to_owned()
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Response {
        let host = self.host();
        let mut all = vec![("Host", host.as_str())];
        all.extend_from_slice(headers);
        raw(self.port, method, path, &all, body).await
    }

    async fn socket(&self, cookie: &str) -> Socket {
        let mut socket = self.upgrade(cookie).await;
        let welcome = exchange(&mut socket, hello(PROTOCOL_VERSION)).await;
        assert_eq!(welcome["result"]["Ok"]["reply"], "welcome", "{welcome}");
        socket
    }

    /// An upgraded socket that has not said hello yet.
    async fn upgrade(&self, cookie: &str) -> Socket {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let mut request = format!("ws://{}/ws", self.host())
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert("Origin", self.origin().parse().unwrap());
        headers.insert("Cookie", cookie.parse().unwrap());
        headers.insert("Sec-WebSocket-Protocol", "archon".parse().unwrap());
        let (socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .expect("the upgrade is accepted");
        socket
    }
}

fn hello(protocol_version: u32) -> serde_json::Value {
    serde_json::json!({
        "req": 1, "cmd": "hello", "protocol_version": protocol_version,
        "client": "archon-web", "client_version": "test",
    })
}

fn enable(on: bool) -> RemoteChange {
    RemoteChange {
        enabled: Some(on),
        ..RemoteChange::default()
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP/1.1 request on its own connection, read to the end.
async fn raw(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .expect("the response arrives")
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete response");
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Response {
        status,
        headers,
        body: body.to_owned(),
    }
}

async fn exchange(socket: &mut Socket, frame: serde_json::Value) -> serde_json::Value {
    socket.send(Message::text(frame.to_string())).await.unwrap();
    next_frame(socket).await
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

/// The close code the server ends the session with.
async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("the session closes")
            .expect("a close frame comes first")
            .unwrap();
        if let Message::Close(frame) = message {
            return frame.map(|f| u16::from(f.code)).unwrap_or(0);
        }
    }
}

#[tokio::test]
async fn health_and_the_app_are_served_with_security_headers_to_known_hosts_only() {
    let listener = Listener::start().await;
    let health = listener.request("GET", "/api/health", &[], "").await;
    assert_eq!(health.status, 200, "{}", health.body);
    assert!(health.body.contains("\"status\":\"ok\""), "{}", health.body);
    let csp = health.header("content-security-policy").unwrap_or_default();
    assert!(
        csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"),
        "{csp}"
    );
    assert_eq!(health.header("x-frame-options"), Some("DENY"));
    let app = listener.request("GET", "/", &[], "").await;
    assert_eq!(app.status, 200);
    assert!(
        app.header("content-type")
            .unwrap_or_default()
            .starts_with("text/html")
    );
    let rebound = raw(
        listener.port,
        "GET",
        "/api/health",
        &[("Host", "evil.example")],
        "",
    )
    .await;
    assert_eq!(rebound.status, 403, "a DNS-rebound Host is refused");
    let missing = listener.request("GET", "/api/nothing", &[], "").await;
    assert_eq!(missing.status, 404);
    let traversal = listener.request("GET", "/../remote.json", &[], "").await;
    assert_eq!(traversal.status, 400);
}

#[tokio::test]
async fn a_code_pairs_one_device_once_and_cross_site_claims_are_refused() {
    let listener = Listener::start().await;
    let closed = listener.claim("ABCD-EFGH").await;
    assert!(
        closed.status >= 400,
        "no pairing is open: {}",
        closed.status
    );
    let code = listener.remote.pair(DeviceRole::Operator).unwrap().code;
    let cross = listener
        .request(
            "POST",
            "/api/pair/claim",
            &[
                ("Origin", "https://evil.example"),
                ("Content-Type", "application/json"),
            ],
            &serde_json::json!({"code": code, "device_name": "x"}).to_string(),
        )
        .await;
    assert_eq!(cross.status, 403, "{}", cross.body);
    let origin = listener.origin();
    let form = listener
        .request(
            "POST",
            "/api/pair/claim",
            &[("Origin", origin.as_str()), ("Content-Type", "text/plain")],
            "code=x",
        )
        .await;
    assert_eq!(form.status, 415, "a simple cross-site form body is refused");
    let paired = listener.claim(&code.to_lowercase()).await;
    assert_eq!(paired.status, 200, "{}", paired.body);
    assert!(
        paired.body.contains("\"name\":\"Test phone\""),
        "{}",
        paired.body
    );
    let again = listener.claim(&code).await;
    assert!(again.status >= 400, "a code works once");
    assert!(!again.body.contains(&code), "the code is never echoed");
    assert_eq!(listener.remote.view().devices.len(), 1);
}

#[tokio::test]
async fn guesses_are_rate_limited_and_the_store_is_private() {
    let listener = Listener::start().await;
    let code = listener.remote.pair(DeviceRole::Operator).unwrap().code;
    let wrong = if code.starts_with('Z') {
        "YYYY-YYYY"
    } else {
        "ZZZZ-ZZZZ"
    };
    for _ in 0..3 {
        let refused = listener.claim(wrong).await;
        assert_eq!(refused.status, 403, "{}", refused.body);
    }
    let limited = listener.claim(&code).await;
    assert_eq!(limited.status, 429, "{}", limited.body);
    assert!(limited.header("retry-after").is_some());
    listener.remote.configure(enable(true)).await.unwrap();
    let path = listener.dir.path().join("remote.json");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn a_session_needs_the_cookie_and_a_forged_one_is_cleared() {
    let listener = Listener::start().await;
    let anonymous = listener.request("GET", "/api/session", &[], "").await;
    assert_eq!(anonymous.status, 401);
    let cookie = listener.pair().await;
    let session = listener
        .request("GET", "/api/session", &[("Cookie", &cookie)], "")
        .await;
    assert_eq!(session.status, 200, "{}", session.body);
    assert!(session.body.contains("Test phone"), "{}", session.body);
    let (id, _) = cookie.split_once('.').unwrap();
    let forged = format!("{id}.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let refused = listener
        .request("GET", "/api/session", &[("Cookie", &forged)], "")
        .await;
    assert_eq!(refused.status, 401);
    let cleared = refused.header("set-cookie").unwrap_or_default();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    let stored = std::fs::read_to_string(listener.dir.path().join("remote.json")).unwrap();
    assert!(
        !stored.contains(cookie.split_once('.').unwrap().1),
        "only a hash is stored"
    );
}

/// The web app may speak 23 or 24; 25 is refused with the range accepted.
#[tokio::test]
async fn the_web_app_connects_at_23_and_24_and_25_is_refused_with_the_range() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    for (version, features) in [
        (23, serde_json::json!([])),
        (24, serde_json::json!(["changes"])),
    ] {
        let mut socket = listener.upgrade(&cookie).await;
        let welcome = exchange(&mut socket, hello(version)).await;
        let data = &welcome["result"]["Ok"]["data"];
        assert_eq!(data["protocol_version"], version, "{welcome}");
        assert_eq!(data["min_protocol_version"], MIN_WEB_PROTOCOL_VERSION);
        assert_eq!(data["features"], features, "{welcome}");
        let named = data.get("device").is_some();
        assert_eq!(
            named,
            version == PROTOCOL_VERSION,
            "only 24 knows devices: {welcome}"
        );
    }
    let mut socket = listener.upgrade(&cookie).await;
    let refusal = exchange(&mut socket, hello(PROTOCOL_VERSION + 1)).await;
    let error = &refusal["result"]["Err"];
    assert_eq!(error["code"], "incompatible", "{refusal}");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("accepts 23 to 24"), "{message}");
    assert_eq!(close_code(&mut socket).await, 1008);
}

#[tokio::test]
async fn the_socket_carries_allowed_requests_and_refuses_the_rest() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let mut socket = listener.socket(&cookie).await;
    let listed = exchange(
        &mut socket,
        serde_json::json!({"req": 2, "cmd": "repo_list"}),
    )
    .await;
    assert_eq!(listed["id"], 2);
    assert_eq!(listed["result"]["Ok"]["reply"], "ok", "{listed}");
    let refused = exchange(
        &mut socket,
        serde_json::json!({"req": 3, "cmd": "shutdown"}),
    )
    .await;
    assert_eq!(refused["id"], 3);
    assert_eq!(refused["result"]["Err"]["code"], "forbidden", "{refused}");
    let merge =
        serde_json::json!({"req": 4, "cmd": "merge_item", "item": "qi_01M621D9ZBJPTMDTV26KKQNBE1"});
    let refused = exchange(&mut socket, merge).await;
    assert_eq!(refused["result"]["Err"]["code"], "forbidden", "{refused}");
    let feed = exchange(
        &mut socket,
        serde_json::json!({"req": 5, "cmd": "subscribe_feed", "after": null, "verbose": false}),
    )
    .await;
    assert_eq!(feed["result"]["Ok"]["reply"], "subscribed", "{feed}");
    assert_eq!(*listener.backend.asked.lock().unwrap(), ["repo_list"]);
    assert_eq!(listener.remote.view().devices[0].sessions, 1);
}

#[tokio::test]
async fn an_upgrade_needs_the_cookie_the_origin_and_the_subprotocol() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let origin = listener.origin();
    let upgrade = |cookie: &'static str, origin: String, protocol: &'static str| {
        vec![
            ("Connection", "Upgrade".to_owned()),
            ("Upgrade", "websocket".to_owned()),
            ("Sec-WebSocket-Version", "13".to_owned()),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
            ("Sec-WebSocket-Protocol", protocol.to_owned()),
            ("Origin", origin),
            ("Cookie", cookie.to_owned()),
        ]
    };
    let cookie: &'static str = Box::leak(cookie.into_boxed_str());
    for (headers, status) in [
        (
            upgrade("archon_device=dev_x.y", origin.clone(), "archon"),
            401,
        ),
        (
            upgrade(cookie, "https://evil.example".into(), "archon"),
            403,
        ),
        (upgrade(cookie, origin.clone(), "chat"), 400),
    ] {
        let borrowed: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let response = listener.request("GET", "/ws", &borrowed, "").await;
        assert_eq!(response.status, status, "{headers:?}: {}", response.body);
    }
}

#[tokio::test]
async fn revoking_a_device_closes_its_session_at_once_and_disabling_closes_all() {
    let listener = Listener::start().await;
    let (phone, tablet) = (listener.pair().await, listener.pair().await);
    let mut phone_socket = listener.socket(&phone).await;
    let mut tablet_socket = listener.socket(&tablet).await;
    let phone_id = phone
        .trim_start_matches("archon_device=")
        .split_once('.')
        .unwrap()
        .0;
    let view = listener.remote.revoke(phone_id).unwrap();
    assert_eq!(view.devices.len(), 1);
    assert_eq!(close_code(&mut phone_socket).await, 4401);
    let session = listener
        .request("GET", "/api/session", &[("Cookie", &phone)], "")
        .await;
    assert_eq!(session.status, 401, "a revoked credential is refused");
    listener.remote.configure(enable(false)).await.unwrap();
    assert_eq!(close_code(&mut tablet_socket).await, 4403);
    let refused = TcpStream::connect(("127.0.0.1", listener.port)).await;
    assert!(refused.is_err(), "the port is closed once disabled");
    assert!(
        listener.remote.pair(DeviceRole::Operator).is_err(),
        "no pairing while off"
    );
}

#[tokio::test]
async fn withdrawing_an_allowed_origin_closes_open_sessions() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let origins = |add: &[&str], remove: &[&str]| RemoteChange {
        add_origins: add.iter().map(|o| (*o).to_owned()).collect(),
        remove_origins: remove.iter().map(|o| (*o).to_owned()).collect(),
        ..RemoteChange::default()
    };
    let change = origins(&["https://phone.example"], &["https://never.example"]);
    listener.remote.configure(change).await.unwrap();
    let mut socket = listener.socket(&cookie).await;
    let change = origins(&[], &["https://never.example"]);
    listener.remote.configure(change).await.unwrap();
    let alive = exchange(
        &mut socket,
        serde_json::json!({"req": 2, "cmd": "repo_list"}),
    )
    .await;
    assert!(
        alive["result"]["Ok"].is_object(),
        "nothing allowed was withdrawn: {alive}"
    );
    let view = listener
        .remote
        .configure(origins(&[], &["https://phone.example"]))
        .await
        .unwrap();
    assert!(view.public_origins.is_empty());
    assert_eq!(close_code(&mut socket).await, 4002);
}

/// Input the server never read must not reset the socket: a reset drops a
/// close frame still queued in the kernel, and the device never sees the code.
#[tokio::test]
async fn a_session_closed_with_unread_input_ends_without_a_reset() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let mut socket = listener.socket(&cookie).await;
    socket
        .send(Message::Ping(Default::default()))
        .await
        .unwrap();
    let id = cookie
        .trim_start_matches("archon_device=")
        .split_once('.')
        .unwrap()
        .0;
    listener.remote.revoke(id).unwrap();
    assert_eq!(close_code(&mut socket).await, 4401);
    let after = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the server ends the connection");
    assert!(after.is_none(), "the close handshake completes: {after:?}");
}

#[tokio::test]
async fn a_revoke_lands_while_the_device_floods_requests_it_does_not_read() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let mut socket = listener.socket(&cookie).await;
    for req in 2..1_000 {
        let frame = serde_json::json!({"req": req, "cmd": "repo_list"});
        socket.send(Message::text(frame.to_string())).await.unwrap();
    }
    let id = cookie
        .trim_start_matches("archon_device=")
        .split_once('.')
        .unwrap()
        .0;
    listener.remote.revoke(id).unwrap();
    assert_eq!(close_code(&mut socket).await, 4401);
    assert!(listener.remote.view().devices.is_empty());
}

/// What the device's own socket answers to `frame`: `ok` or an error code.
async fn outcome(socket: &mut Socket, frame: serde_json::Value) -> String {
    let answer = exchange(socket, frame).await;
    match answer["result"].get("Ok") {
        Some(ok) => ok["reply"].as_str().unwrap().to_owned(),
        None => answer["result"]["Err"]["code"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test]
async fn an_observer_reads_and_is_refused_every_change() {
    let listener = Listener::start().await;
    let cookie = listener.pair_as(DeviceRole::Observer).await;
    let session = listener
        .request("GET", "/api/session", &[("Cookie", cookie.as_str())], "")
        .await;
    assert!(
        session.body.contains("\"role\":\"observer\""),
        "{}",
        session.body
    );
    let mut socket = listener.upgrade(&cookie).await;
    let welcome = exchange(&mut socket, hello(PROTOCOL_VERSION)).await;
    let device = &welcome["result"]["Ok"]["data"]["device"];
    assert_eq!(device["role"], "observer", "{welcome}");
    assert_eq!(device["name"], "Test phone", "{welcome}");
    let read = serde_json::json!({"req": 2, "cmd": "repo_list"});
    assert_eq!(outcome(&mut socket, read).await, "ok");
    let changes = [
        serde_json::json!({"cmd": "say", "text": "hi"}),
        serde_json::json!({"cmd": "cancel_item", "item": "qi_01M621D9ZBJPTMDTV26KKQNBE1"}),
        serde_json::json!({"cmd": "pause", "scope": "global"}),
        serde_json::json!({"cmd": "answer", "question": "q_01M621D9ZBJPTMDTV26KKQNBE1", "text": "yes"}),
    ];
    for (n, mut change) in changes.into_iter().enumerate() {
        change["req"] = serde_json::json!(10 + n);
        let answer = exchange(&mut socket, change.clone()).await;
        let error = &answer["result"]["Err"];
        assert_eq!(error["code"], "forbidden", "{change}: {answer}");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("only observes"), "{message}");
    }
    assert_eq!(*listener.backend.asked.lock().unwrap(), ["repo_list"]);
    assert_eq!(listener.remote.view().devices[0].role, DeviceRole::Observer);
}

#[tokio::test]
async fn a_hard_pause_or_resume_is_refused_and_a_soft_one_reaches_archond() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let mut socket = listener.socket(&cookie).await;
    for cmd in ["pause", "resume"] {
        let hard = serde_json::json!({"req": 2, "cmd": cmd, "scope": "global", "hard": true});
        let answer = exchange(&mut socket, hard).await;
        assert_eq!(answer["result"]["Err"]["code"], "forbidden", "{answer}");
        let message = answer["result"]["Err"]["message"].as_str().unwrap();
        assert!(message.contains("with hard"), "{message}");
    }
    let soft = serde_json::json!({"req": 3, "cmd": "pause", "scope": "global", "hard": false});
    assert_eq!(outcome(&mut socket, soft).await, "ok");
    assert_eq!(*listener.backend.asked.lock().unwrap(), ["pause"]);
}

#[tokio::test]
async fn mounted_the_cookie_is_scoped_to_the_mount_and_only_trusted_origins_are_accepted() {
    let listener = Listener::mounted().await;
    let urls = listener.remote.pair(DeviceRole::Operator).unwrap().urls;
    assert_eq!(urls, [format!("{AETHER}/archon/pair")]);
    let view = listener.remote.view();
    assert_eq!(view.mounted_at.as_deref(), Some("/archon"));
    assert_eq!(view.trusted_origins, [AETHER]);
    let code = listener.remote.pair(DeviceRole::Operator).unwrap().code;
    let body = serde_json::json!({"code": code, "device_name": "Phone"}).to_string();
    let loopback = format!("http://127.0.0.1:{}", listener.port);
    for refused in [loopback.as_str(), "https://evil.example"] {
        let headers = [("Origin", refused), ("Content-Type", "application/json")];
        let claim = listener
            .request("POST", "/api/pair/claim", &headers, &body)
            .await;
        assert_eq!(claim.status, 403, "{refused}: {}", claim.body);
        assert!(claim.body.contains("forbidden_origin"), "{}", claim.body);
    }
    let claim = listener.claim(&code).await;
    assert_eq!(claim.status, 200, "{}", claim.body);
    let set = claim.header("set-cookie").unwrap();
    assert!(
        set.contains("; Path=/archon;") && set.contains("Secure"),
        "{set}"
    );
    let cookie = set.split(';').next().unwrap().to_owned();
    let welcome_socket = listener.socket(&cookie).await;
    drop(welcome_socket);
    let public = raw(
        listener.port,
        "GET",
        "/api/health",
        &[("Host", "box.ts.net")],
        "",
    )
    .await;
    assert_eq!(public.status, 403, "the mount names this listener as Host");
    let forget_headers = [
        ("Origin", AETHER),
        ("Content-Type", "application/json"),
        ("Cookie", cookie.as_str()),
    ];
    let forgot = listener
        .request("POST", "/api/session/forget", &forget_headers, "{}")
        .await;
    assert_eq!(forgot.status, 200, "{}", forgot.body);
    let cleared = forgot.header("set-cookie").unwrap();
    assert!(
        cleared.contains("Path=/archon;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );
}

#[tokio::test]
async fn mounted_an_upgrade_from_loopback_is_refused() {
    let listener = Listener::mounted().await;
    let cookie = listener.pair().await;
    let stream = TcpStream::connect(("127.0.0.1", listener.port))
        .await
        .unwrap();
    let mut request = format!("ws://{}/ws", listener.host())
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    let loopback = format!("http://127.0.0.1:{}", listener.port);
    headers.insert("Origin", loopback.parse().unwrap());
    headers.insert("Cookie", cookie.parse().unwrap());
    headers.insert("Sec-WebSocket-Protocol", "archon".parse().unwrap());
    let refused = tokio_tungstenite::client_async(request, stream).await;
    let Err(tokio_tungstenite::tungstenite::Error::Http(response)) = refused else {
        panic!("a loopback origin is refused while mounted: {refused:?}");
    };
    assert_eq!(response.status(), 403);
}

/// A change stream that ended gives its slot back, so a client that must
/// resubscribe (after a lag or a resync) always can; ids are never reused.
#[tokio::test]
async fn an_ended_change_subscription_frees_its_slot() {
    let listener = Listener::start().await;
    let cookie = listener.pair().await;
    let mut socket = listener.socket(&cookie).await;
    let mut ids = Vec::new();
    for req in 2..10 {
        let frame = serde_json::json!({"req": req, "cmd": "subscribe_changes", "after": 0});
        let opened = exchange(&mut socket, frame).await;
        assert_eq!(opened["id"], req, "{opened}");
        assert_eq!(opened["result"]["Ok"]["reply"], "subscribed", "{opened}");
        ids.push(
            opened["result"]["Ok"]["data"]["id"]
                .as_u64()
                .expect("an id"),
        );
    }
    assert_eq!(
        ids,
        (1..=8).collect::<Vec<u64>>(),
        "fresh ids, past the limit of 4"
    );
}
