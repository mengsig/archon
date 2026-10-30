//! The JSON endpoints: health, pairing, and the device's own session.

use std::time::Instant;

use archon_core::protocol::PROTOCOL_VERSION;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};
use serde::Deserialize;

use super::{Body, COOKIE, HttpError, json};
use crate::Shared;
use crate::devices::{self, Device};
use crate::pairing::Refusal;

/// A year and a bit; the device stays paired until it is revoked.
const COOKIE_MAX_AGE_SECS: u64 = 400 * 24 * 3600;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    code: String,
    device_name: String,
}

pub fn health(shared: &Shared) -> Response<Body> {
    let pairing_open = shared
        .lock()
        .pairing
        .as_ref()
        .is_some_and(|p| !p.expired(Instant::now()));
    let body = serde_json::json!({
        "status": "ok",
        "pairing_open": pairing_open,
        "version": shared.daemon_version,
    });
    json(StatusCode::OK, &body)
}

/// Enrol a device on the open pairing's code, and hand it its credential.
pub fn claim(
    shared: &Shared,
    origin: &str,
    body: serde_json::Value,
) -> Result<Response<Body>, HttpError> {
    let claim: Claim = serde_json::from_value(body)
        .map_err(|e| HttpError::new(StatusCode::BAD_REQUEST, "bad_request", e.to_string()))?;
    let name = devices::normalize_name(&claim.device_name)
        .map_err(|e| HttpError::new(StatusCode::BAD_REQUEST, "bad_request", e))?;
    let mut state = shared.lock();
    let crate::State {
        pairing, guesses, ..
    } = &mut *state;
    let Some(open) = pairing.as_mut() else {
        return Err(closed(
            "no pairing is open; run `archon remote pair` on the computer",
        ));
    };
    match open.claim(&claim.code, Instant::now(), guesses) {
        Ok(()) => {}
        Err(refusal) => {
            if matches!(refusal, Refusal::Expired | Refusal::Closed) {
                *pairing = None;
            }
            return Err(refused(refusal));
        }
    }
    *pairing = None;
    let issued = Device::issue(name, archon_core::now()).map_err(internal)?;
    let device = issued.device.clone();
    state
        .update(&shared.path, |stored| {
            stored.devices.push(issued.device);
            Ok(())
        })
        .map_err(internal)?;
    drop(state);
    tracing::info!(device = %device.id, "a remote device was paired");
    let mut response = json(
        StatusCode::OK,
        &serde_json::json!({"device": device_json(&device)}),
    );
    let cookie = credential_cookie(&issued.credential, origin.starts_with("https://"));
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

/// Who this browser is paired as, and the protocol to greet with.
pub fn session(shared: &Shared, headers: &HeaderMap) -> Result<Response<Body>, HttpError> {
    let device = authenticate(shared, headers)?;
    let body = serde_json::json!({
        "device": device_json(&device),
        "protocol_version": PROTOCOL_VERSION,
        "daemon_version": shared.daemon_version,
    });
    Ok(json(StatusCode::OK, &body))
}

/// Unpair this device itself, as "Forget this device" in the app does.
pub fn forget(shared: &Shared, headers: &HeaderMap) -> Result<Response<Body>, HttpError> {
    let device = authenticate(shared, headers)?;
    shared.revoke(&device.id).map_err(internal)?;
    let mut response = json(StatusCode::OK, &serde_json::json!({}));
    response
        .headers_mut()
        .insert(header::SET_COOKIE, super::clear_cookie());
    Ok(response)
}

/// The paired device whose credential the request's cookie carries.
pub fn authenticate(shared: &Shared, headers: &HeaderMap) -> Result<Device, HttpError> {
    let credentials: Vec<&str> = headers
        .get_all(header::COOKIE)
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value)
        .collect();
    let state = shared.lock();
    let found = credentials
        .iter()
        .find_map(|credential| devices::authenticate(&state.stored.devices, credential));
    match found {
        Some(device) => Ok(device.clone()),
        None => {
            let mut error = HttpError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "this browser is not paired",
            );
            error.clear_cookie = !credentials.is_empty();
            Err(error)
        }
    }
}

fn device_json(device: &Device) -> serde_json::Value {
    serde_json::json!({"id": device.id, "name": device.name, "paired_at": device.paired_at})
}

fn credential_cookie(credential: &str, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE}={credential}; Path=/; HttpOnly; SameSite=Strict; Max-Age={COOKIE_MAX_AGE_SECS}{secure}"
    );
    HeaderValue::from_str(&cookie).expect("credentials are base64url")
}

fn closed(message: &str) -> HttpError {
    HttpError::new(StatusCode::FORBIDDEN, "pairing_closed", message)
}

fn refused(refusal: Refusal) -> HttpError {
    match refusal {
        Refusal::Closed => closed("too many wrong codes; run `archon remote pair` again"),
        Refusal::Expired => HttpError::new(
            StatusCode::GONE,
            "expired",
            "the code expired; run `archon remote pair` again",
        ),
        Refusal::InvalidCode => HttpError::new(
            StatusCode::FORBIDDEN,
            "invalid_code",
            "that code is not open for pairing",
        ),
        Refusal::RateLimited { retry_after } => {
            let mut error = HttpError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many codes tried; wait a moment",
            );
            error.retry_after_secs = Some(retry_after.as_secs().max(1));
            error
        }
    }
}

fn internal(error: archon_core::error::ArchonError) -> HttpError {
    tracing::error!(%error, "a remote request failed");
    HttpError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "archond could not complete that",
    )
}
