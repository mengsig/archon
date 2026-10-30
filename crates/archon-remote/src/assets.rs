//! The embedded web app, served as written with a strong `ETag`.

use std::sync::OnceLock;

use sha2::{Digest as _, Sha256};

/// `(route, bytes)` for every file `build.rs` embedded, sorted by route.
static FILES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/assets.rs"));

#[derive(Debug)]
pub struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    /// The quoted SHA-256 of the bytes.
    pub etag: String,
}

/// The asset `path` names: an exact file, or `index.html` for an app route
/// (a last segment with no `.`). `None` otherwise.
pub fn find(path: &str) -> Option<&'static Asset> {
    assert!(path.starts_with('/'), "request paths are absolute");
    let assets = table();
    let found = assets.binary_search_by(|(route, _)| (*route).cmp(path));
    if let Ok(index) = found {
        return Some(&assets[index].1);
    }
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        return None;
    }
    let index = assets
        .binary_search_by(|(route, _)| (*route).cmp("/index.html"))
        .expect("build.rs requires index.html");
    Some(&assets[index].1)
}

fn table() -> &'static [(&'static str, Asset)] {
    static TABLE: OnceLock<Vec<(&'static str, Asset)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let table: Vec<_> = FILES
            .iter()
            .map(|(route, bytes)| (*route, asset(route, bytes)))
            .collect();
        assert!(
            table.windows(2).all(|w| w[0].0 < w[1].0),
            "build.rs sorts the routes, so they can be searched"
        );
        table
    })
}

fn asset(route: &str, bytes: &'static [u8]) -> Asset {
    let hash = Sha256::digest(bytes);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    Asset {
        bytes,
        content_type: content_type(route),
        etag: format!("\"{hex}\""),
    }
}

fn content_type(route: &str) -> &'static str {
    let extension = route.rsplit_once('.').map_or("", |(_, ext)| ext);
    match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_serves_app_routes_but_not_missing_files() {
        let index = find("/index.html").expect("index.html is embedded");
        assert!(index.content_type.starts_with("text/html"));
        assert_eq!(
            find("/pair").map(|a| a.etag.as_str()),
            Some(index.etag.as_str())
        );
        assert!(find("/missing.js").is_none());
        assert!(index.etag.starts_with('"') && index.etag.len() == 66);
    }

    #[test]
    fn tooling_is_not_embedded() {
        for (route, _) in table() {
            assert!(!route.contains("/node_modules/"), "{route}");
            assert!(
                !route.contains("/tests/") && !route.ends_with(".spec.js"),
                "{route}"
            );
            assert!(!route.ends_with("package.json"), "{route}");
        }
        assert_eq!(content_type("/a.webmanifest"), "application/manifest+json");
    }
}
