//! Per-browser HTTP/1 proxy for the registered preview origin.

use std::fmt::Debug;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::uri::Authority;
use http_body_util::{BodyExt, Empty, combinators::UnsyncBoxBody};
use hyper::body::{Body, Incoming};
use hyper::client::conn::http1 as client;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1 as server;
use hyper::service::service_fn;
use hyper::upgrade::OnUpgrade;
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

use crate::BrowserError;
use crate::origin::{Origin, parse_url};

const MAX_CONNECTIONS: usize = 64;
const HEADER_BUFFER: usize = 64 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;
type ProxyError = Box<dyn std::error::Error + Send + Sync>;

/// Verify that the peer of a connected preview socket belongs to the item.
#[async_trait::async_trait]
pub trait UpstreamGuard: Send + Sync + Debug {
    /// Called before any HTTP bytes are sent on the socket.
    async fn verify_peer(&self, peer: &TcpStream) -> Result<(), BrowserError>;
}

#[derive(Debug)]
pub(crate) struct Policy {
    pub(crate) origin: Origin,
    guard: Arc<dyn UpstreamGuard>,
}

impl Policy {
    pub(crate) fn new(url: &str, guard: Arc<dyn UpstreamGuard>) -> Result<Self, BrowserError> {
        Ok(Self {
            origin: Origin::new(url)?,
            guard,
        })
    }

    async fn connect(&self) -> Result<TcpStream, ProxyError> {
        let address = self.origin.address;
        assert!(address.ip().is_loopback());
        let address6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, address.port()));
        let candidates = [address, address6];
        let addresses = if self.origin.authority.host() == "localhost" {
            &candidates[..]
        } else {
            &candidates[..1]
        };
        let peer = TcpStream::connect(addresses).await?;
        if !addresses.contains(&peer.peer_addr()?) {
            return Err(refused("unexpected preview socket address"));
        }
        self.guard.verify_peer(&peer).await?;
        Ok(peer)
    }

    pub(crate) async fn probe(&self, url: &str) -> bool {
        let Some(uri) = parse_url(url).filter(|uri| self.origin.matches_uri(uri, "http")) else {
            return false;
        };
        let attempt = async {
            let socket = self.connect().await?;
            let (mut sender, conn) = client::Builder::new()
                .max_buf_size(HEADER_BUFFER)
                .handshake(TokioIo::new(socket))
                .await?;
            let target = path(&uri);
            let target = if target.starts_with('?') {
                format!("/{target}")
            } else {
                target.to_owned()
            };
            let request = Request::head(target)
                .header(header::HOST, self.origin.authority.as_str())
                .header(header::CONNECTION, "close")
                .body(Empty::<Bytes>::new())?;
            let response = sender.send_request(request);
            tokio::pin!(response);
            tokio::select! {
                response = &mut response => { response?; }
                finished = conn => { finished?; response.await?; }
            }
            Ok::<_, ProxyError>(())
        };
        matches!(timeout(Duration::from_secs(2), attempt).await, Ok(Ok(())))
    }
}

#[derive(Debug)]
pub(crate) struct Proxy {
    pub(crate) addr: SocketAddr,
    _task: AbortOnDrop,
}

impl Proxy {
    pub(crate) async fn start(policy: Arc<Policy>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0);
        Ok(Self {
            addr,
            _task: AbortOnDrop(tokio::spawn(serve(listener, policy))),
        })
    }
}

#[derive(Debug)]
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(listener: TcpListener, policy: Arc<Policy>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "preview proxy task failed");
                }
            }
            incoming = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (socket, _) = match incoming {
                    Ok(peer) => peer,
                    Err(error) => {
                        tracing::error!(%error, "preview proxy listener failed");
                        return;
                    }
                };
                connections.spawn(serve_connection(socket, policy.clone()));
            }
        }
    }
}

enum Transfer {
    Http(AbortOnDrop),
    Inspect(OnUpgrade),
    WebSocket(AbortOnDrop, OnUpgrade, OnUpgrade),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestForm {
    Proxy,
    Tunnel,
}

async fn serve_connection(socket: TcpStream, policy: Arc<Policy>) {
    if let Err(error) = handle_connection(socket, &policy).await {
        tracing::debug!(%error, "preview proxy connection ended");
    }
}

async fn handle_connection(socket: TcpStream, policy: &Policy) -> Result<(), ProxyError> {
    let transfer = match serve_http(TokioIo::new(socket), policy, RequestForm::Proxy).await? {
        Transfer::Inspect(tunnel) => {
            let tunnel = timeout(HEADER_TIMEOUT, tunnel).await??;
            serve_http(tunnel, policy, RequestForm::Tunnel).await?
        }
        other => other,
    };
    match transfer {
        Transfer::WebSocket(_driver, downstream, upstream) => {
            relay_websocket(downstream, upstream).await
        }
        Transfer::Http(_driver) => Ok(()),
        Transfer::Inspect(_) => Err(refused("nested CONNECT is forbidden")),
    }
}

async fn serve_http<I>(
    socket: I,
    policy: &Policy,
    form: RequestForm,
) -> Result<Transfer, ProxyError>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let transfer = Mutex::new(None);
    let handler = service_fn(|request| forward(request, policy, &transfer, form));
    let connection = server::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .max_buf_size(HEADER_BUFFER)
        .keep_alive(false)
        .serve_connection(socket, handler)
        .with_upgrades();
    timeout(HTTP_TIMEOUT, connection).await??;
    transfer
        .into_inner()
        .unwrap()
        .ok_or_else(|| refused("preview proxy connection closed without a request"))
}

fn inspect_tunnel(
    mut request: Request<Incoming>,
    origin: &Origin,
    transfer: &Mutex<Option<Transfer>>,
) -> Result<Response<ProxyBody>, ProxyError> {
    let uri = request.uri();
    if request.version() != Version::HTTP_11
        || uri.scheme().is_some()
        || uri.path_and_query().is_some()
        || !uri
            .authority()
            .is_some_and(|a| a.port().is_some() && origin.matches_authority(a))
        || !matches_host(request.headers(), origin)
        || request.headers().contains_key(header::UPGRADE)
        || request.body().size_hint().exact() != Some(0)
    {
        return Err(refused("CONNECT is outside the registered preview origin"));
    }
    // Chromium sends plain WebSockets through CONNECT. Parse the next request
    // as HTTP too. No TCP socket to the preview is opened for CONNECT itself.
    let mut transfer = transfer.lock().unwrap();
    assert!(transfer.is_none());
    assert_eq!(request.method(), Method::CONNECT);
    *transfer = Some(Transfer::Inspect(hyper::upgrade::on(&mut request)));
    Ok(Response::new(
        Empty::<Bytes>::new().map_err(|e| match e {}).boxed_unsync(),
    ))
}

async fn forward(
    mut request: Request<Incoming>,
    policy: &Policy,
    transfer: &Mutex<Option<Transfer>>,
    form: RequestForm,
) -> Result<Response<ProxyBody>, ProxyError> {
    if form == RequestForm::Proxy && request.method() == Method::CONNECT {
        return inspect_tunnel(request, &policy.origin, transfer);
    }
    let websocket = check_request(&request, &policy.origin, form)?;
    let downstream = websocket.then(|| hyper::upgrade::on(&mut request));
    let socket = timeout(CONNECT_TIMEOUT, policy.connect()).await??;
    let (mut sender, conn) = client::Builder::new()
        .max_buf_size(HEADER_BUFFER)
        .handshake(TokioIo::new(socket))
        .await?;
    let driver = AbortOnDrop(tokio::spawn(async move {
        if let Err(error) = conn.with_upgrades().await {
            tracing::debug!(%error, "preview upstream HTTP connection failed");
        }
    }));
    let target = path(request.uri()).parse()?;
    *request.uri_mut() = target;
    strip_hop_headers(request.headers_mut())?;
    request.headers_mut().insert(
        header::HOST,
        HeaderValue::from_str(policy.origin.authority.as_str())?,
    );
    if websocket {
        set_upgrade(request.headers_mut());
    } else {
        request
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    let mut response = sender.send_request(request).await?;
    let upgrade = prepare_response(&mut response, &policy.origin, downstream)?;
    let result = match upgrade {
        Some((downstream, upstream)) => Transfer::WebSocket(driver, downstream, upstream),
        None => Transfer::Http(driver),
    };
    let mut transfer = transfer.lock().unwrap();
    assert!(transfer.is_none());
    assert_eq!(
        response.status() == StatusCode::SWITCHING_PROTOCOLS,
        matches!(result, Transfer::WebSocket(..))
    );
    *transfer = Some(result);
    Ok(response.map(BodyExt::boxed_unsync))
}

fn check_request(
    request: &Request<Incoming>,
    origin: &Origin,
    form: RequestForm,
) -> Result<bool, ProxyError> {
    let uri = request.uri();
    let headers = request.headers();
    let websocket = headers.contains_key(header::UPGRADE);
    let valid_target = match form {
        RequestForm::Proxy => {
            origin.matches_uri(uri, "http") || (websocket && origin.matches_uri(uri, "ws"))
        }
        RequestForm::Tunnel => websocket && uri.scheme().is_none() && uri.authority().is_none(),
    };
    if request.version() != Version::HTTP_11
        || request.method() == Method::CONNECT
        || !valid_target
        || !path(uri).starts_with('/')
        || !matches_host(headers, origin)
    {
        return Err(refused("request is outside the registered preview origin"));
    }
    if websocket
        && (request.method() != Method::GET
            || request.body().size_hint().exact() != Some(0)
            || !is_websocket(headers)?
            || headers.get("sec-websocket-key").is_none()
            || headers.get("sec-websocket-version") != Some(&HeaderValue::from_static("13")))
    {
        return Err(refused("unrecognized HTTP upgrade"));
    }
    Ok(websocket)
}

fn matches_host(headers: &HeaderMap, origin: &Origin) -> bool {
    let mut values = headers.get_all(header::HOST).iter();
    let authority = values
        .next()
        .and_then(|value| Authority::try_from(value.as_bytes()).ok());
    values.next().is_none() && authority.is_some_and(|host| origin.matches_authority(&host))
}

fn prepare_response(
    response: &mut Response<Incoming>,
    origin: &Origin,
    downstream: Option<OnUpgrade>,
) -> Result<Option<(OnUpgrade, OnUpgrade)>, ProxyError> {
    let switching = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    let upgrade = if switching {
        let downstream = downstream.ok_or_else(|| refused("unsolicited HTTP upgrade"))?;
        if !is_websocket(response.headers())?
            || !response.headers().contains_key("sec-websocket-accept")
        {
            return Err(refused("unrecognized upstream HTTP upgrade"));
        }
        Some((downstream, hyper::upgrade::on(&mut *response)))
    } else {
        None
    };
    strip_hop_headers(response.headers_mut())?;
    if switching {
        set_upgrade(response.headers_mut());
    }
    if !switching {
        for location in response.headers().get_all(header::LOCATION) {
            if response.status().is_redirection()
                && !location
                    .to_str()
                    .is_ok_and(|location| origin.allows_redirect(location))
            {
                return Err(refused("redirect is outside the registered preview origin"));
            }
        }
        response.headers_mut().append(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("frame-src 'self'; object-src 'none'"),
        );
    }
    Ok(upgrade)
}

fn path(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

fn connection_tokens(headers: &HeaderMap) -> Result<Vec<HeaderName>, ProxyError> {
    let mut tokens = Vec::new();
    for value in headers.get_all(header::CONNECTION) {
        for token in value.to_str()?.split(',') {
            tokens.push(HeaderName::from_bytes(token.trim().as_bytes())?);
        }
    }
    Ok(tokens)
}

fn strip_hop_headers(headers: &mut HeaderMap) -> Result<(), ProxyError> {
    for name in connection_tokens(headers)? {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    Ok(())
}

fn is_websocket(headers: &HeaderMap) -> Result<bool, ProxyError> {
    Ok(headers.get_all(header::UPGRADE).iter().count() == 1
        && headers
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        && connection_tokens(headers)?.contains(&header::UPGRADE))
}

fn set_upgrade(headers: &mut HeaderMap) {
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
}

async fn relay_websocket(downstream: OnUpgrade, upstream: OnUpgrade) -> Result<(), ProxyError> {
    let (downstream, upstream) = timeout(HEADER_TIMEOUT, async {
        tokio::try_join!(downstream, upstream)
    })
    .await??;
    tokio::io::copy_bidirectional(&mut TokioIo::new(downstream), &mut TokioIo::new(upstream))
        .await?;
    Ok(())
}

fn refused(message: &'static str) -> ProxyError {
    io::Error::new(io::ErrorKind::PermissionDenied, message).into()
}
