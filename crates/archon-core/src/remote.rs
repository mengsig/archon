//! Remote access from a phone or browser (docs/remote.md): the `[remote]`
//! configuration, and what `archon remote` reads and changes on archond.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::error::ArchonError;

/// Where the listener binds when the operator enables it.
pub const DEFAULT_ADDRESS: &str = "127.0.0.1:8378";
/// Most public origins the allowlist holds.
pub const MAX_PUBLIC_ORIGINS: usize = 16;

/// `[remote]` in config.toml: static settings, read when archond starts.
/// Whether the listener is open is runtime state (`archon remote enable`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    pub address: String,
    /// Required to bind anything but a loopback address.
    pub allow_remote: bool,
    /// HTTP connections served at once, and device sessions open at once.
    pub max_connections: u32,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDRESS.to_owned(),
            allow_remote: false,
            max_connections: 16,
        }
    }
}

impl RemoteConfig {
    /// The parsed bind address; a non-loopback one needs `allow_remote`.
    pub fn socket_address(&self) -> Result<SocketAddr, ArchonError> {
        let address: SocketAddr = self.address.parse().map_err(|_| {
            ArchonError::Config(format!(
                "[remote] address {:?} must be host:port, e.g. {DEFAULT_ADDRESS}",
                self.address
            ))
        })?;
        if !address.ip().is_loopback() && !self.allow_remote {
            return Err(ArchonError::Config(format!(
                "[remote] address {address} is not loopback; set allow_remote = true to bind it, \
                 or keep it on 127.0.0.1 behind tailscale serve"
            )));
        }
        Ok(address)
    }

    pub fn validate(&self) -> Result<(), ArchonError> {
        self.socket_address()?;
        if !(1..=1024).contains(&self.max_connections) {
            return Err(ArchonError::Config(
                "[remote] max_connections must be 1 to 1024".into(),
            ));
        }
        Ok(())
    }
}

/// What `remote_status` and every remote change answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteView {
    pub enabled: bool,
    /// The configured bind address.
    pub address: String,
    /// The address the listener is bound to now; `None` while closed.
    pub listening: Option<String>,
    /// Why an enabled listener is not open.
    pub error: Option<String>,
    /// The `https://` origins browsers may use besides loopback.
    pub public_origins: Vec<String>,
    pub devices: Vec<DeviceView>,
    /// When the open pairing code expires; `None` with no pairing open.
    pub pairing_expires_at: Option<Timestamp>,
}

/// A paired device, as the operator sees it. Never carries its credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub paired_at: Timestamp,
    pub last_seen_at: Option<Timestamp>,
    /// Its WebSocket sessions open now.
    pub sessions: u32,
}

/// A change to the runtime settings; `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteChange {
    pub enabled: Option<bool>,
    /// Added to the allowlist, after [`normalize_origin`].
    pub add_origins: Vec<String>,
    /// Dropped from the allowlist.
    pub remove_origins: Vec<String>,
}

/// A pairing just opened: the one-time code and where to use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    /// Eight Crockford base32 symbols shown as `ABCD-EFGH`.
    pub code: String,
    pub expires_at: Timestamp,
    /// `<origin>/pair` for each origin the app is reachable at.
    pub urls: Vec<String>,
}

/// The `http://` origin this machine reaches a listener bound to `bound` at:
/// the bound address itself, or loopback for an unspecified one.
pub fn local_origin(bound: SocketAddr) -> String {
    assert!(bound.port() > 0, "a bound listener has a port");
    let ip = match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let origin = format!("http://{}", SocketAddr::new(ip, bound.port()));
    debug_assert!(!origin.ends_with('/'), "an origin has no path");
    origin
}

/// `origin` as a browser writes it in the `Origin` header: `https://host`
/// or `https://host:port`, lower case, no default port, nothing after.
pub fn normalize_origin(origin: &str) -> Result<String, ArchonError> {
    let bad = || {
        ArchonError::Invalid(format!(
            "{origin:?} is not an origin; give it as https://host or https://host:port"
        ))
    };
    let rest = origin.trim().strip_prefix("https://").ok_or_else(bad)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', '\\', ' ']) {
        return Err(bad());
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    // A DNS name: the public origin is a tailnet or proxy name, never a literal.
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    if !host_ok {
        return Err(bad());
    }
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    let Some(port) = port else {
        return Ok(format!("https://{host}"));
    };
    // Written as browsers do: digits only, no leading zeros, 443 left out.
    match port.parse::<u16>() {
        _ if !port.bytes().all(|b| b.is_ascii_digit()) => Err(bad()),
        Ok(443) => Ok(format!("https://{host}")),
        Ok(port) if port > 0 => Ok(format!("https://{host}:{port}")),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_origin_follows_the_bound_address() {
        let cases = [
            ("127.0.0.1:8378", "http://127.0.0.1:8378"),
            ("127.0.0.2:8378", "http://127.0.0.2:8378"),
            ("[::1]:8378", "http://[::1]:8378"),
            ("0.0.0.0:8378", "http://127.0.0.1:8378"),
            ("[::]:8378", "http://[::1]:8378"),
        ];
        for (bound, expected) in cases {
            assert_eq!(local_origin(bound.parse().unwrap()), expected, "{bound}");
        }
    }

    #[test]
    fn origins_are_written_as_browsers_send_them() {
        let cases = [
            ("https://Box.Tail1234.ts.net", "https://box.tail1234.ts.net"),
            ("https://box.ts.net/", "https://box.ts.net"),
            ("https://box.ts.net:443", "https://box.ts.net"),
            ("https://box.ts.net:8443", "https://box.ts.net:8443"),
            ("https://box.ts.net.", "https://box.ts.net"),
            ("https://box.ts.net:08443", "https://box.ts.net:8443"),
            ("https://box.ts.net:0443", "https://box.ts.net"),
        ];
        for (given, expected) in cases {
            assert_eq!(normalize_origin(given).unwrap(), expected, "{given}");
        }
    }

    #[test]
    fn anything_but_a_bare_https_origin_is_refused() {
        for bad in [
            "http://box.ts.net",
            "https://",
            "https://box.ts.net/app",
            "https://box.ts.net?x=1",
            "https://user@box.ts.net",
            "https://box.ts.net:0",
            "https://box.ts.net:99999",
            "https://box.ts.net:+8443",
            "https://box.ts.net:",
            "box.ts.net",
            "https://bo x.ts.net",
            "https://[::1]:8443",
        ] {
            assert!(normalize_origin(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_non_loopback_address_needs_allow_remote() {
        let mut config = RemoteConfig {
            address: "0.0.0.0:8378".into(),
            ..RemoteConfig::default()
        };
        let refused = config.validate().unwrap_err().to_string();
        assert!(refused.contains("allow_remote"), "{refused}");
        config.allow_remote = true;
        config.validate().unwrap();
        assert!(RemoteConfig::default().validate().is_ok());
        config.address = "nonsense".into();
        assert!(config.validate().is_err());
    }
}
