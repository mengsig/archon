//! The checks that precede any handler: Host against DNS rebinding, Origin
//! and `Sec-Fetch-Site` against cross-site requests, and bounded JSON bodies.

use std::net::SocketAddr;
use std::time::Duration;

use http_body_util::{BodyExt as _, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName};
use hyper::{Method, Request, StatusCode, Uri};

use super::HttpError;
use crate::Shared;

pub const MAX_BODY: usize = 16 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// The origins this listener answers now: the public allowlist, and
/// loopback on the bound port, and the bound address itself.
#[derive(Debug)]
pub struct Allowed {
    origins: Vec<String>,
}

impl Allowed {
    pub fn now(shared: &Shared, bound: SocketAddr) -> Self {
        let port = bound.port();
        assert!(port > 0, "the listener is bound");
        let mut origins = shared.lock().stored.public_origins.clone();
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            origins.push(format!("http://{host}:{port}"));
        }
        let local = archon_core::remote::local_origin(bound);
        if !origins.contains(&local) {
            origins.push(local);
        }
        Self { origins }
    }

    /// Exactly one `Host`, naming an allowed origin's authority.
    pub fn check_host(&self, headers: &HeaderMap, uri: &Uri) -> Result<(), HttpError> {
        let forbidden = || {
            HttpError::new(
                StatusCode::FORBIDDEN,
                "forbidden_host",
                "this Host is not served here",
            )
        };
        let host = single(headers, &header::HOST).ok_or_else(forbidden)?;
        let known = self.origins.iter().any(|o| authority(o) == host);
        if !known {
            return Err(forbidden());
        }
        match uri.authority() {
            Some(target) if target.as_str() != host => Err(forbidden()),
            _ => Ok(()),
        }
    }

    /// Exactly one allowed `Origin`, from the same site. Returns it.
    pub fn check_origin(&self, headers: &HeaderMap) -> Result<String, HttpError> {
        let forbidden = || {
            HttpError::new(
                StatusCode::FORBIDDEN,
                "forbidden_origin",
                "this origin may not call archond",
            )
        };
        let origin = single(headers, &header::ORIGIN).ok_or_else(forbidden)?;
        if !self.origins.iter().any(|o| o == origin) {
            return Err(forbidden());
        }
        match single(headers, &HeaderName::from_static("sec-fetch-site")) {
            None | Some("same-origin") => Ok(origin.to_owned()),
            Some(_) => Err(forbidden()),
        }
    }
}

fn authority(origin: &str) -> &str {
    origin.split_once("://").map_or(origin, |(_, rest)| rest)
}

/// The value of a header present exactly once, when it is text.
pub fn single<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    let mut values = headers.get_all(name).into_iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
}

/// A same-site read, when the browser says where it comes from at all.
pub fn check_fetch_site(headers: &HeaderMap) -> Result<(), HttpError> {
    match single(headers, &HeaderName::from_static("sec-fetch-site")) {
        None | Some("same-origin" | "none") => Ok(()),
        Some(_) => Err(HttpError::new(
            StatusCode::FORBIDDEN,
            "forbidden_origin",
            "cross-site reads are refused",
        )),
    }
}

/// Refuse paths with empty, `.` or `..` segments, backslashes or escapes.
pub fn check_path(path: &str) -> Result<(), HttpError> {
    let bad = || HttpError::new(StatusCode::BAD_REQUEST, "bad_request", "malformed path");
    if !path.starts_with('/') || path.contains(['\\', '%']) {
        return Err(bad());
    }
    if path == "/" {
        return Ok(());
    }
    let clean = path[1..]
        .split('/')
        .all(|segment| !matches!(segment, "" | "." | ".."));
    if clean { Ok(()) } else { Err(bad()) }
}

pub fn only(method: &Method, allowed: &[Method]) -> Result<(), HttpError> {
    if allowed.contains(method) {
        return Ok(());
    }
    Err(HttpError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!("{method} is not served here"),
    ))
}

/// The body as JSON: `application/json`, at most [`MAX_BODY`] bytes, all
/// of it within [`BODY_TIMEOUT`].
pub async fn read_json(request: Request<Incoming>) -> Result<serde_json::Value, HttpError> {
    let json_type = single(request.headers(), &header::CONTENT_TYPE).is_some_and(|value| {
        let essence = value.split(';').next().unwrap_or_default().trim();
        essence.eq_ignore_ascii_case("application/json")
    });
    if !json_type {
        return Err(HttpError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "send Content-Type: application/json",
        ));
    }
    let limited = Limited::new(request.into_body(), MAX_BODY);
    let collected = tokio::time::timeout(BODY_TIMEOUT, limited.collect())
        .await
        .map_err(|_| {
            HttpError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the body did not arrive in time",
            )
        })?;
    let bytes = match collected {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.is::<http_body_util::LengthLimitError>() => {
            return Err(HttpError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!("bodies are at most {MAX_BODY} bytes"),
            ));
        }
        Err(_) => {
            return Err(HttpError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "the body was cut short",
            ));
        }
    };
    assert!(bytes.len() <= MAX_BODY, "the limit held");
    serde_json::from_slice(&bytes).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("not JSON: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Allowed {
        Allowed {
            origins: vec!["https://box.ts.net".into(), "http://127.0.0.1:8378".into()],
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn hosts_must_be_ours_and_single() {
        let uri: Uri = "/".parse().unwrap();
        let ok = allowed();
        assert!(
            ok.check_host(&headers(&[("host", "box.ts.net")]), &uri)
                .is_ok()
        );
        assert!(
            ok.check_host(&headers(&[("host", "127.0.0.1:8378")]), &uri)
                .is_ok()
        );
        for bad in [
            vec![("host", "evil.example")],
            vec![],
            vec![("host", "box.ts.net"), ("host", "box.ts.net")],
        ] {
            assert!(ok.check_host(&headers(&bad), &uri).is_err(), "{bad:?}");
        }
        let absolute: Uri = "http://evil.example/".parse().unwrap();
        assert!(
            ok.check_host(&headers(&[("host", "box.ts.net")]), &absolute)
                .is_err()
        );
    }

    #[test]
    fn origins_must_be_allowed_and_same_site() {
        let ok = allowed();
        let good = headers(&[
            ("origin", "https://box.ts.net"),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert_eq!(ok.check_origin(&good).unwrap(), "https://box.ts.net");
        for bad in [
            headers(&[("origin", "https://evil.example")]),
            headers(&[
                ("origin", "https://box.ts.net"),
                ("sec-fetch-site", "cross-site"),
            ]),
            headers(&[("origin", "null")]),
            headers(&[]),
        ] {
            assert!(ok.check_origin(&bad).is_err(), "{bad:?}");
        }
        assert!(check_fetch_site(&headers(&[("sec-fetch-site", "cross-site")])).is_err());
    }

    #[test]
    fn traversal_and_escapes_are_refused() {
        for good in ["/", "/index.html", "/pair", "/core/app.js"] {
            assert!(check_path(good).is_ok(), "{good}");
        }
        for bad in [
            "/../etc/passwd",
            "/a//b",
            "/a/./b",
            "/%2e%2e/x",
            "/a\\b",
            "/pair/",
        ] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }
}
