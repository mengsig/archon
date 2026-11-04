//! The one page archond's listener serves a browser: a short note that
//! Archon's screens live on the Aether origin (docs/remote.md). No script,
//! no file and no app is embedded; the page's only style is allowed by hash.

use std::sync::OnceLock;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// The page's whole styling: calm and readable, light or dark.
const STYLE: &str = "body{font:16px/1.6 system-ui,sans-serif;max-width:34rem;\
margin:4rem auto;padding:0 1.25rem;color:#1f2328;background:#fafafa}\
h1{font-size:1.25rem;font-weight:600}code{font-size:.95em}a{color:#0b57d0}\
@media (prefers-color-scheme:dark){body{color:#e6e6e6;background:#161616}\
a{color:#8ab4f8}}";

/// The frontend's Archon pairing screen, relative to the Aether origin.
const PAIR_ROUTE: &str = "/#/pair/archon";

/// Every response's Content-Security-Policy: nothing may load or run but
/// the pointer page's own inline style.
pub fn content_security_policy() -> &'static str {
    static CSP: OnceLock<String> = OnceLock::new();
    CSP.get_or_init(|| {
        let digest = Sha256::digest(STYLE.as_bytes());
        let hash = base64::engine::general_purpose::STANDARD.encode(digest);
        assert_eq!(hash.len(), 44, "a SHA-256 is 44 base64 characters");
        format!(
            "default-src 'none'; style-src 'sha256-{hash}'; base-uri 'none'; \
             form-action 'none'; frame-ancestors 'none'"
        )
    })
}

/// The page for a GET of `path`: it names the Aether origin archond is
/// mounted on (the first of `trusted`), or says how to set one up.
pub fn pointer(path: &str, mounted_at: Option<&str>, trusted: &[String]) -> String {
    assert!(path.starts_with('/'), "request paths are absolute");
    assert!(
        mounted_at.is_none() || !trusted.is_empty(),
        "a mount trusts at least one origin"
    );
    let pairing = path == "/pair" || path.ends_with("/pair");
    let body = match (mounted_at, trusted.first()) {
        (Some(_), Some(origin)) => mounted_body(&escape(origin), pairing),
        _ => UNMOUNTED_BODY.to_owned(),
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex\">\n<title>Archon</title>\n\
         <style>{STYLE}</style>\n</head>\n<body>\n<h1>Archon is served by Aether</h1>\n\
         {body}\n</body>\n</html>\n"
    )
}

const UNMOUNTED_BODY: &str = "<p>This is archond's own listener. Archon's screens are \
    part of the Aether app, which reaches archond through Aether's listener.</p>\n\
    <p>On the host, run <code>archon remote setup</code>; it mounts Archon on Aether \
    and prints the address to open.</p>";

fn mounted_body(origin: &str, pairing: bool) -> String {
    if pairing {
        return format!(
            "<p>Archon is paired in the Aether app. Open \
             <a href=\"{origin}{PAIR_ROUTE}\">{origin}{PAIR_ROUTE}</a> and type the \
             code <code>archon remote pair</code> printed.</p>"
        );
    }
    format!(
        "<p>Archon's screens are part of the Aether app at \
         <a href=\"{origin}/\">{origin}</a>.</p>"
    )
}

/// `text` safe inside an HTML attribute or element.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_points_to_the_aether_origin_and_runs_nothing() {
        let trusted = ["https://box.ts.net".to_owned()];
        let home = pointer("/", Some("/archon"), &trusted);
        assert!(home.contains("href=\"https://box.ts.net/\""), "{home}");
        let pair = pointer("/pair", Some("/archon"), &trusted);
        assert!(
            pair.contains("href=\"https://box.ts.net/#/pair/archon\""),
            "{pair}"
        );
        let unmounted = pointer("/queue", None, &[]);
        assert!(unmounted.contains("archon remote setup"), "{unmounted}");
        for page in [&home, &pair, &unmounted] {
            assert!(
                !page.contains("<script") && !page.contains("<link"),
                "{page}"
            );
            assert!(page.contains(&format!("<style>{STYLE}</style>")));
        }
    }

    #[test]
    fn the_policy_allows_the_page_style_alone() {
        let csp = content_security_policy();
        let digest = Sha256::digest(STYLE.as_bytes());
        let hash = base64::engine::general_purpose::STANDARD.encode(digest);
        assert!(csp.contains(&format!("style-src 'sha256-{hash}'")), "{csp}");
        assert!(csp.starts_with("default-src 'none'"), "{csp}");
        assert!(!csp.contains("script-src") && !csp.contains("connect-src"));
        assert_eq!(escape("a\"<b>&'"), "a&quot;&lt;b&gt;&amp;&#39;");
    }
}
