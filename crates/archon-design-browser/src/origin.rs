//! URL validation shared by browser navigation and its proxy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use http::uri::Authority;
use hyper::Uri;

use crate::BrowserError;

#[derive(Debug)]
pub(crate) struct Origin {
    pub(crate) authority: Authority,
    pub(crate) address: SocketAddr,
}

impl Origin {
    pub(crate) fn new(url: &str) -> Result<Self, BrowserError> {
        let invalid =
            || BrowserError::Launch("expected a literal loopback HTTP preview URL".into());
        let uri = parse_url(url).ok_or_else(invalid)?;
        let authority = uri.authority().ok_or_else(invalid)?.clone();
        let ip = match authority.host() {
            "localhost" | "127.0.0.1" => IpAddr::V4(Ipv4Addr::LOCALHOST),
            "[::1]" => IpAddr::V6(Ipv6Addr::LOCALHOST),
            _ => return Err(invalid()),
        };
        let port = http_port(&authority)
            .filter(|port| *port != 0)
            .ok_or_else(invalid)?;
        if uri.scheme_str() != Some("http") || !valid_authority(&authority) {
            return Err(invalid());
        }
        let address = SocketAddr::new(ip, port);
        debug_assert!(address.ip().is_loopback());
        debug_assert_ne!(address.port(), 0);
        Ok(Self { authority, address })
    }

    pub(crate) fn matches_authority(&self, authority: &Authority) -> bool {
        valid_authority(authority)
            && self.authority.host().eq_ignore_ascii_case(authority.host())
            && http_port(authority) == Some(self.address.port())
    }

    pub(crate) fn matches_uri(&self, uri: &Uri, scheme: &str) -> bool {
        uri.scheme_str() == Some(scheme)
            && uri.authority().is_some_and(|a| self.matches_authority(a))
    }

    pub(crate) fn allows_navigation(&self, url: &str) -> bool {
        parse_url(url).is_some_and(|uri| self.matches_uri(&uri, "http"))
    }

    pub(crate) fn allows_network_url(&self, url: &str) -> bool {
        parse_url(url)
            .is_some_and(|uri| self.matches_uri(&uri, "http") || self.matches_uri(&uri, "ws"))
    }

    pub(crate) fn allows_redirect(&self, location: &str) -> bool {
        if !valid_url_text(location) {
            return false;
        }
        if let Some(authority_path) = location.strip_prefix("//") {
            return !authority_path.starts_with('/')
                && self.allows_navigation(&format!("http:{location}"));
        }
        let first_segment = location.split(['/', '?', '#']).next().unwrap_or("");
        !first_segment.contains(':') || self.allows_navigation(location)
    }
}

fn valid_url_text(url: &str) -> bool {
    !url.chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
}

pub(crate) fn parse_url(url: &str) -> Option<Uri> {
    if !valid_url_text(url) {
        return None;
    }
    let network_url = url.split('#').next()?;
    network_url.parse().ok()
}

fn valid_authority(authority: &Authority) -> bool {
    authority.as_str() == authority.host()
        || authority.port().is_some_and(|port| {
            authority.as_str().len() == authority.host().len() + 1 + port.as_str().len()
                && authority.as_str().starts_with(authority.host())
                && port.as_str().bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn http_port(authority: &Authority) -> Option<u16> {
    if authority.as_str() == authority.host() {
        Some(80)
    } else {
        authority.port_u16()
    }
}
