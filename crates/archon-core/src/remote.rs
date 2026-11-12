//! Remote access from a phone or browser (docs/remote.md): the `[remote]`
//! configuration, and what `archon remote` reads and changes on archond.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::error::ArchonError;
use crate::protocol::DeviceRole;

/// Where the listener binds when the operator enables it.
pub const DEFAULT_ADDRESS: &str = "127.0.0.1:8378";
/// Most public origins the allowlist holds.
pub const MAX_PUBLIC_ORIGINS: usize = 16;
/// Most origins `[remote] trusted_origins` holds.
pub const MAX_TRUSTED_ORIGINS: usize = 16;
/// Longest `[remote] mounted_at` prefix segment, after its `/`.
const MAX_MOUNT_SEGMENT: usize = 32;

/// `[remote]` in config.toml: static settings, read when archond starts.
/// Whether the listener is open is runtime state (`archon remote enable`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    pub address: String,
    /// Required to bind anything but a loopback address.
    pub allow_remote: bool,
    /// HTTP connections served at once, and device sessions open at once.
    pub max_connections: u32,
    /// The path prefix an Aether listener serves archond under, such as
    /// `/archon`. The device cookie is scoped to it.
    pub mounted_at: Option<String>,
    /// The mounting listener's origins; while mounted, the only ones accepted.
    pub trusted_origins: Vec<String>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDRESS.to_owned(),
            allow_remote: false,
            max_connections: 16,
            mounted_at: None,
            trusted_origins: Vec::new(),
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
        self.trusted().map(|_| ())
    }

    /// The configuration with `trusted_origins` written as browsers send
    /// them, once it is valid.
    pub fn normalized(mut self) -> Result<Self, ArchonError> {
        self.validate()?;
        self.trusted_origins = self.trusted()?;
        Ok(self)
    }

    /// The path the device cookie is scoped to: the mount, or the root.
    pub fn cookie_path(&self) -> &str {
        self.mounted_at.as_deref().unwrap_or("/")
    }

    /// `trusted_origins` normalized and deduplicated. They come with a mount
    /// and only with one, since nothing but a mount would send them.
    fn trusted(&self) -> Result<Vec<String>, ArchonError> {
        let config = |message: String| ArchonError::Config(format!("[remote] {message}"));
        let Some(prefix) = &self.mounted_at else {
            return match self.trusted_origins.is_empty() {
                true => Ok(Vec::new()),
                false => Err(config(
                    "trusted_origins needs mounted_at; both come from `archon remote setup`".into(),
                )),
            };
        };
        check_mount_prefix(prefix).map_err(config)?;
        if self.trusted_origins.is_empty() {
            return Err(config(format!(
                "mounted_at = {prefix:?} needs trusted_origins, the mounting listener's origins"
            )));
        }
        let mut trusted: Vec<String> = Vec::with_capacity(self.trusted_origins.len());
        for origin in &self.trusted_origins {
            let origin = normalize_trusted_origin(origin)
                .map_err(|e| config(format!("trusted_origins: {e}")))?;
            if !trusted.contains(&origin) {
                trusted.push(origin);
            }
        }
        if trusted.len() > MAX_TRUSTED_ORIGINS {
            return Err(config(format!(
                "trusted_origins holds at most {MAX_TRUSTED_ORIGINS} origins"
            )));
        }
        assert!(!trusted.is_empty(), "a mount trusts at least one origin");
        Ok(trusted)
    }
}

/// A mount prefix as Aether writes it: `/` and one lowercase segment that
/// starts with a letter, such as `/archon`.
fn check_mount_prefix(prefix: &str) -> Result<(), String> {
    let bad =
        || format!("mounted_at {prefix:?} must be one lowercase segment, such as \"/archon\"");
    let segment = prefix.strip_prefix('/').ok_or_else(bad)?;
    let mut chars = segment.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !starts_with_letter || !rest_ok || segment.len() > MAX_MOUNT_SEGMENT {
        return Err(bad());
    }
    Ok(())
}

/// What `remote_status` and every remote change answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// The Aether mount archond is served under, from `[remote] mounted_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mounted_at: Option<String>,
    /// The only origins accepted while mounted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_origins: Vec<String>,
    pub devices: Vec<DeviceView>,
    /// When the open pairing code expires; `None` with no pairing open.
    pub pairing_expires_at: Option<Timestamp>,
}

/// A paired device, as the operator sees it. Never carries its credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    /// What it may do, chosen when it was paired.
    #[serde(default)]
    pub role: DeviceRole,
    pub paired_at: Timestamp,
    pub last_seen_at: Option<Timestamp>,
    /// Its WebSocket sessions open now.
    pub sessions: u32,
}

/// A change to the runtime settings; `None` keeps a field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteChange {
    pub enabled: Option<bool>,
    /// Added to the allowlist, after [`normalize_origin`].
    pub add_origins: Vec<String>,
    /// Dropped from the allowlist.
    pub remove_origins: Vec<String>,
}

/// A pairing just opened: the one-time code and where to use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PairingCode {
    /// Eight Crockford base32 symbols shown as `ABCD-EFGH`.
    pub code: String,
    pub expires_at: Timestamp,
    /// `<origin>/pair` for each origin the app is reachable at.
    pub urls: Vec<String>,
}

/// Where this machine reaches a listener bound to `bound`, port included: the
/// bound address itself, or loopback for an unspecified one. Proxies forward here.
pub fn local_target(bound: SocketAddr) -> String {
    format!("http://{}", reachable(bound))
}

/// The `Origin` a browser on this machine sends to the listener at `bound`:
/// [`local_target`] as browsers write it, without the default port 80.
pub fn local_origin(bound: SocketAddr) -> String {
    let at = reachable(bound);
    let host = match at.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    http_origin(&host, at.port())
}

/// `http://host:port` as browsers write it in `Origin` and `Host`: port 80
/// is the default and left out.
pub fn http_origin(host: &str, port: u16) -> String {
    assert!(port > 0, "a bound listener has a port");
    assert!(
        !host.is_empty() && !host.contains('/'),
        "a bare host, got {host:?}"
    );
    match port {
        80 => format!("http://{host}"),
        port => format!("http://{host}:{port}"),
    }
}

fn reachable(bound: SocketAddr) -> SocketAddr {
    assert!(bound.port() > 0, "a bound listener has a port");
    let ip = match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bound.port())
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
    // Written as browsers do: no leading zeros, 443 left out.
    match parse_port(port).ok_or_else(bad)? {
        443 => Ok(format!("https://{host}")),
        port => Ok(format!("https://{host}:{port}")),
    }
}

/// An origin `[remote] trusted_origins` may hold: an `https://` origin as
/// [`normalize_origin`] writes it, or `http://` on a loopback host.
pub fn normalize_trusted_origin(origin: &str) -> Result<String, ArchonError> {
    let Some(rest) = origin.trim().strip_prefix("http://") else {
        return normalize_origin(origin);
    };
    let bad = || {
        ArchonError::Invalid(format!(
            "{origin:?} is not a trusted origin; give https://host[:port], \
             or http://127.0.0.1, http://localhost or http://[::1] with a port"
        ))
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    // `[::1]:8377` keeps its colons inside the brackets.
    let split = match rest.find(']') {
        Some(end) => (&rest[..=end], rest[end + 1..].strip_prefix(':')),
        None => match rest.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        },
    };
    let (host, port) = split;
    let host = host.to_ascii_lowercase();
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return Err(bad());
    }
    let port = match port {
        Some(port) => parse_port(port).ok_or_else(bad)?,
        None if rest.len() == host.len() => 80,
        None => return Err(bad()),
    };
    Ok(http_origin(&host, port))
}

/// A port as `Origin` may carry it: decimal digits only, 1 to 65535.
fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port > 0)
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
            let bound = bound.parse().unwrap();
            assert_eq!(local_origin(bound), expected, "{bound}");
            assert_eq!(local_target(bound), expected, "{bound}");
        }
    }

    #[test]
    fn port_80_is_left_out_of_origins_but_not_of_proxy_targets() {
        let cases = [
            ("127.0.0.1:80", "http://127.0.0.1"),
            ("[::1]:80", "http://[::1]"),
            ("0.0.0.0:80", "http://127.0.0.1"),
        ];
        for (bound, origin) in cases {
            let bound: SocketAddr = bound.parse().unwrap();
            assert_eq!(local_origin(bound), origin, "{bound}");
            assert_eq!(local_target(bound), format!("{origin}:80"), "{bound}");
        }
        assert_eq!(http_origin("localhost", 80), "http://localhost");
        assert_eq!(http_origin("localhost", 8080), "http://localhost:8080");
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
    fn trusted_origins_are_https_or_loopback_http() {
        let cases = [
            ("https://Box.ts.net:443", "https://box.ts.net"),
            ("http://127.0.0.1:8377", "http://127.0.0.1:8377"),
            ("http://LOCALHOST:8377/", "http://localhost:8377"),
            ("http://[::1]:8377", "http://[::1]:8377"),
            ("http://127.0.0.1:80", "http://127.0.0.1"),
            ("http://[::1]", "http://[::1]"),
        ];
        for (given, expected) in cases {
            assert_eq!(
                normalize_trusted_origin(given).unwrap(),
                expected,
                "{given}"
            );
        }
        for bad in [
            "http://box.ts.net",
            "http://127.0.0.2:8377",
            "http://127.0.0.1:",
            "http://127.0.0.1:0",
            "http://[::1]x",
            "http://user@localhost:8377",
            "http://localhost:8377/app",
            "ftp://localhost",
        ] {
            assert!(normalize_trusted_origin(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_mount_needs_a_prefix_and_trusted_origins_together() {
        let mounted = RemoteConfig {
            mounted_at: Some("/archon".into()),
            trusted_origins: vec!["https://Box.ts.net".into(), "https://box.ts.net".into()],
            ..RemoteConfig::default()
        };
        let normalized = mounted.clone().normalized().unwrap();
        assert_eq!(normalized.trusted_origins, ["https://box.ts.net"]);
        assert_eq!(normalized.cookie_path(), "/archon");
        assert_eq!(RemoteConfig::default().cookie_path(), "/");
        let alone = RemoteConfig {
            mounted_at: None,
            ..mounted.clone()
        };
        assert!(
            alone
                .validate()
                .unwrap_err()
                .to_string()
                .contains("needs mounted_at")
        );
        let untrusting = RemoteConfig {
            trusted_origins: vec![],
            ..mounted.clone()
        };
        assert!(
            untrusting
                .validate()
                .unwrap_err()
                .to_string()
                .contains("needs trusted_origins")
        );
        for prefix in [
            "archon",
            "/",
            "/Archon",
            "/1archon",
            "/a/b",
            "/a_b",
            &format!("/{}", "a".repeat(33)),
        ] {
            let bad = RemoteConfig {
                mounted_at: Some(prefix.to_owned()),
                ..mounted.clone()
            };
            assert!(bad.validate().is_err(), "{prefix} was accepted");
        }
        let longest = RemoteConfig {
            mounted_at: Some(format!("/{}", "a".repeat(32))),
            ..mounted.clone()
        };
        assert!(longest.validate().is_ok());
        let crowded = RemoteConfig {
            trusted_origins: (1..=17).map(|n| format!("https://h{n}.ts.net")).collect(),
            ..mounted
        };
        assert!(crowded.validate().is_err());
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
