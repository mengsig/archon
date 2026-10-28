//! `GET /ws`: archond's own protocol over a WebSocket, one JSON frame per
//! text message, for a device the cookie authenticated. `hello` first; then
//! each allowed request goes to the [`Backend`](crate::Backend).

mod session;

use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use crate::Shared;
use crate::http::{Body, HttpError};

pub const SUBPROTOCOL: &str = "archon";

/// Check the upgrade, take a session slot and answer `101`; the session
/// runs on its own task once hyper hands the connection over.
pub fn upgrade(
    shared: &Arc<Shared>,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, HttpError> {
    let device = crate::http::api::authenticate(shared, request.headers())?;
    let accept = accept_key(request.headers())
        .map_err(|message| HttpError::new(StatusCode::BAD_REQUEST, "bad_request", message))?;
    let Some(backend) = shared.backend.get().cloned() else {
        return Err(unavailable("archond is still starting"));
    };
    let Some(slot) = shared.sessions.slot() else {
        return Err(unavailable(
            "every remote session is taken; close one and retry",
        ));
    };
    let registration = shared.sessions.register(&device.id, slot);
    let on_upgrade = hyper::upgrade::on(&mut request);
    let shared = shared.clone();
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                shared.note_seen(&device.id);
                session::carry(TokioIo::new(upgraded), backend, registration).await;
            }
            Err(error) => tracing::debug!(%error, "a WebSocket upgrade did not complete"),
        }
    });
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let accept = HeaderValue::from_str(&accept).expect("an accept key is base64");
    headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    Ok(response)
}

fn unavailable(message: &str) -> HttpError {
    HttpError::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
}

/// The comma-separated tokens of every `name` header.
fn tokens<'a>(headers: &'a HeaderMap, name: &HeaderName) -> impl Iterator<Item = &'a str> {
    headers
        .get_all(name)
        .into_iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
}

/// The `Sec-WebSocket-Accept` for a valid version-13 upgrade that offers
/// the `archon` subprotocol, or what is wrong with it.
fn accept_key(headers: &HeaderMap) -> Result<String, &'static str> {
    let upgrading = tokens(headers, &header::CONNECTION).any(|t| t.eq_ignore_ascii_case("upgrade"))
        && tokens(headers, &header::UPGRADE).any(|t| t.eq_ignore_ascii_case("websocket"));
    if !upgrading {
        return Err("`/ws` answers only a WebSocket upgrade");
    }
    if !tokens(headers, &header::SEC_WEBSOCKET_VERSION).eq(["13"]) {
        return Err("only WebSocket version 13 is served");
    }
    if !tokens(headers, &header::SEC_WEBSOCKET_PROTOCOL).any(|t| t == SUBPROTOCOL) {
        return Err("offer the `archon` subprotocol in Sec-WebSocket-Protocol");
    }
    let key = crate::http::checks::single(headers, &header::SEC_WEBSOCKET_KEY)
        .ok_or("exactly one Sec-WebSocket-Key is required")?;
    let nonce = base64::engine::general_purpose::STANDARD.decode(key.as_bytes());
    if !nonce.is_ok_and(|nonce| nonce.len() == 16) {
        return Err("Sec-WebSocket-Key must be 16 bytes of base64");
    }
    Ok(tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn a_valid_upgrade_is_accepted_with_the_rfc_key() {
        let good = headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "archon"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        // RFC 6455's own example.
        assert_eq!(accept_key(&good).unwrap(), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let mut other = good.clone();
        other.insert("sec-websocket-protocol", "aether".parse().unwrap());
        assert!(accept_key(&other).is_err());
        let mut short = good.clone();
        short.insert("sec-websocket-key", "c2hvcnQ=".parse().unwrap());
        assert!(accept_key(&short).is_err());
        assert!(accept_key(&headers(&[("upgrade", "websocket")])).is_err());
    }
}
