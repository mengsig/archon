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
use hyper::{Method, Request, Response, StatusCode};

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

    fn response(self) -> Response<Body> {
        let body = serde_json::json!({"error": {"code": self.code, "message": self.message}});
        let mut response = json(self.status, &body);
        let headers = response.headers_mut();
        if let Some(secs) = self.retry_after_secs {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        if self.clear_cookie {
            headers.insert(header::SET_COOKIE, clear_cookie());
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

pub fn clear_cookie() -> HeaderValue {
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
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
            error.response()
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
