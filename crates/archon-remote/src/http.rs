//! Every HTTP request, checked in order: Host, path, method, Origin, body.
//! `/api/*` answers JSON; anything else is the embedded web app.

pub mod api;
pub mod checks;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri};

use crate::Shared;

pub type Body = Full<Bytes>;

/// The HttpOnly cookie a paired device authenticates with.
pub const COOKIE: &str = "archon_device";

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
    font-src 'self'; manifest-src 'self'; worker-src 'self'; connect-src 'self'; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// A refused request: its status, a stable `code` and a message.
#[derive(Debug)]
pub struct HttpError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after_secs: Option<u64>,
    /// Tell the browser to drop a credential that no longer works.
    pub clear_cookie: bool,
}

impl HttpError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        assert!(!status.is_success(), "errors carry an error status");
        Self {
            status,
            code,
            message: message.into(),
            retry_after_secs: None,
            clear_cookie: false,
        }
    }

    /// The response; a cleared cookie names `cookie_path`, as it was set.
    fn response(self, cookie_path: &str) -> Response<Body> {
        let body = serde_json::json!({"error": {"code": self.code, "message": self.message}});
        let mut response = json(self.status, &body);
        let headers = response.headers_mut();
        if let Some(secs) = self.retry_after_secs {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        if self.clear_cookie {
            headers.insert(header::SET_COOKIE, clear_cookie(cookie_path));
        }
        response
    }
}

pub fn json(status: StatusCode, body: &serde_json::Value) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Drop the credential cookie set at `path`, `/` or the mount prefix.
pub fn clear_cookie(path: &str) -> HeaderValue {
    assert!(path.starts_with('/'), "a cookie path is absolute: {path:?}");
    let cookie = format!("{COOKIE}=; Path={path}; HttpOnly; SameSite=Strict; Max-Age=0");
    HeaderValue::from_str(&cookie).expect("the cookie is ASCII")
}

/// Answer one request; every response carries the security headers.
pub async fn respond(
    shared: &Arc<Shared>,
    bound: SocketAddr,
    request: Request<Incoming>,
) -> Response<Body> {
    let mut response = match route(shared, bound, request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(status = %error.status, code = error.code, "remote request refused");
            error.response(shared.config.cookie_path())
        }
    };
    secure(response.headers_mut());
    response
}

async fn route(
    shared: &Arc<Shared>,
    bound: SocketAddr,
    request: Request<Incoming>,
) -> Result<Response<Body>, HttpError> {
    let allowed = checks::Allowed::now(shared, bound);
    allowed.check_host(request.headers(), request.uri())?;
    let path = request.uri().path().to_owned();
    checks::check_path(&path)?;
    let method = request.method().clone();
    match path.as_str() {
        "/api/health" => {
            checks::only(&method, &[Method::GET, Method::HEAD])?;
            Ok(api::health(shared))
        }
        "/api/pair/claim" => {
            checks::only(&method, &[Method::POST])?;
            let origin = allowed.check_origin(request.headers())?;
            let body = checks::read_json(request).await?;
            api::claim(shared, &origin, body)
        }
        "/api/session" => {
            checks::only(&method, &[Method::GET])?;
            checks::check_fetch_site(request.headers())?;
            api::session(shared, request.headers())
        }
        "/api/session/forget" => {
            checks::only(&method, &[Method::POST])?;
            allowed.check_origin(request.headers())?;
            let headers = request.headers().clone();
            checks::read_json(request).await?;
            api::forget(shared, &headers)
        }
        "/ws" => {
            checks::only(&method, &[Method::GET])?;
            allowed.check_origin(request.headers())?;
            crate::ws::upgrade(shared, bound, request)
        }
        api if api == "/api" || api.starts_with("/api/") => Err(HttpError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such endpoint",
        )),
        _ => {
            checks::only(&method, &[Method::GET, Method::HEAD])?;
            asset(&path, &method, request.headers())
        }
    }
}

/// Seconds an API client refused for being busy is told to wait.
pub const BUSY_RETRY_SECS: u64 = 2;
/// The first page retry waits this long, before jitter; each next one doubles.
pub const BUSY_BASE_SECS: u64 = 2;
/// A retry waits at most this long before jitter, which adds up to half again.
pub const BUSY_CAP_SECS: u64 = 20;
/// After this many automatic retries the page stops reloading itself.
pub const BUSY_MAX_ATTEMPTS: u32 = 6;

/// The seconds the page waits before retry number `attempt` (from 0): an
/// exponential base, capped, plus `jitter` (0..=255) of up to half the base.
pub fn busy_wait_secs(attempt: u32, jitter: u8) -> u64 {
    let base = (BUSY_BASE_SECS << attempt.min(BUSY_MAX_ATTEMPTS)).min(BUSY_CAP_SECS);
    let wait = base + (base / 2 * u64::from(jitter) + 127) / 255;
    assert!(
        (BUSY_BASE_SECS..=BUSY_CAP_SECS + BUSY_CAP_SECS / 2).contains(&wait),
        "a wait stays in its documented range: {wait}"
    );
    wait
}

/// The retries already made, from `?busy=N`; anything unusable counts as none.
fn busy_attempt(query: Option<&str>) -> u32 {
    let given = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("busy="))
        .and_then(|n| n.parse::<u32>().ok());
    given.unwrap_or(0).min(BUSY_MAX_ATTEMPTS)
}

fn busy_jitter() -> u8 {
    let mut byte = [0u8; 1];
    if let Err(error) = getrandom::fill(&mut byte) {
        tracing::warn!(%error, "no randomness for the busy page's jitter; retrying without it");
        return 0;
    }
    byte[0]
}

/// What the app shows when the navigation itself is refused. The CSP allows
/// no script, so it reloads through a `<meta>` refresh, with a growing,
/// jittered wait and a counter in the query, and gives up after
/// [`BUSY_MAX_ATTEMPTS`] with a link to try again by hand. The refresh names
/// `./`, the app's root: one to the same URL with a `#/route` fragment would
/// only scroll to it.
fn busy_page(attempt: u32, wait: u64) -> String {
    let (refresh, status) = if attempt < BUSY_MAX_ATTEMPTS {
        let next = attempt + 1;
        let refresh =
            format!("<meta http-equiv=\"refresh\" content=\"{wait}; url=./?busy={next}\">");
        (refresh, "Retrying in a few seconds.")
    } else {
        (
            String::new(),
            "Archon stopped retrying. <a href=\".\">Try again</a>",
        )
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
        <meta name=\"color-scheme\" content=\"light dark\">{refresh}<title>Archon</title></head>\
        <body><h1>Can\u{2019}t connect to Archon</h1><p role=\"status\">Archon is busy: every \
        connection is in use. {status}</p></body></html>"
    )
}

/// The answer to a request on a connection beyond `max_connections`: `503`
/// with `Retry-After`, JSON for the API and the socket, a page otherwise.
/// A module the page cannot load sends the app to its own retry screen.
pub fn busy(uri: &Uri) -> Response<Body> {
    let path = uri.path();
    assert!(
        path.starts_with('/'),
        "request paths are absolute: {path:?}"
    );
    let api = path == "/ws" || path == "/api" || path.starts_with("/api/");
    let mut retry_after = BUSY_RETRY_SECS;
    let mut response = if api {
        let message = "Archon is busy: every connection is in use. Retrying shortly.";
        let body = serde_json::json!({"error": {"code": "busy", "message": message}});
        json(StatusCode::SERVICE_UNAVAILABLE, &body)
    } else {
        let attempt = busy_attempt(uri.query());
        retry_after = busy_wait_secs(attempt, busy_jitter());
        let mut page = Response::new(Full::new(Bytes::from(busy_page(attempt, retry_after))));
        *page.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        let headers = page.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        page
    };
    let headers = response.headers_mut();
    headers.insert(header::RETRY_AFTER, HeaderValue::from(retry_after));
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    secure(headers);
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a refusal is a 503"
    );
    response
}

fn asset(path: &str, method: &Method, headers: &HeaderMap) -> Result<Response<Body>, HttpError> {
    let Some(asset) = crate::assets::find(path) else {
        return Err(HttpError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such file",
        ));
    };
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|tags| tags.split(',').any(|t| t.trim() == asset.etag));
    let (status, body) = match (unchanged, method == Method::HEAD) {
        (true, _) => (StatusCode::NOT_MODIFIED, Bytes::new()),
        (false, true) => (StatusCode::OK, Bytes::new()),
        (false, false) => (StatusCode::OK, Bytes::from_static(asset.bytes)),
    };
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    let etag = HeaderValue::from_str(&asset.etag).expect("a hex ETag is ASCII");
    headers.insert(header::ETAG, etag);
    Ok(response)
}

fn secure(headers: &mut HeaderMap) {
    let fixed = [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
    ];
    for (name, value) in fixed {
        headers.insert(name, HeaderValue::from_static(value));
    }
    let named = [
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "permissions-policy",
            "camera=(), geolocation=(), microphone=()",
        ),
    ];
    for (name, value) in named {
        headers.insert(name, HeaderValue::from_static(value));
    }
}
