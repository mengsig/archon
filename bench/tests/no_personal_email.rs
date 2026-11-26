//! Committed bench files must not contain personal email addresses; only
//! noreply and obviously fake fixture addresses are allowed.

use std::path::{Path, PathBuf};

const KEPT_DOMAINS: [&str; 7] = [
    "anthropic.com",
    "users.noreply.github.com",
    "example.com",
    "example.invalid",
    "example.test",
    "example.org",
    "example.net",
];
const KEPT_ADDRESSES: [&str; 1] = ["archon@openai.com"];
const SKIPPED_DIRS: [&str; 4] = [".work", "target", ".git", "__pycache__"];

fn is_local(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._%+-".contains(&byte)
}

fn is_domain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.'
}

fn is_kept(address: &str) -> bool {
    let address = address.to_ascii_lowercase();
    let (local, domain) = address.split_once('@').expect("address has an at sign");
    KEPT_ADDRESSES.contains(&address.as_str())
        || local == "noreply"
        || domain.starts_with("noreply.")
        || KEPT_DOMAINS
            .iter()
            .any(|kept| domain == *kept || domain.ends_with(&format!(".{kept}")))
}

fn addresses_in(data: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for (at, _) in data.iter().enumerate().filter(|(_, byte)| **byte == b'@') {
        let mut start = at;
        while start > 0 && is_local(data[start - 1]) {
            start -= 1;
        }
        while start < at && !data[start].is_ascii_alphanumeric() {
            start += 1;
        }
        let mut end = at + 1;
        while end < data.len() && is_domain(data[end]) {
            end += 1;
        }
        while end > at + 1 && data[end - 1] == b'.' {
            end -= 1;
        }
        let domain = &data[at + 1..end];
        let tld = domain
            .rsplit(|byte| *byte == b'.')
            .next()
            .unwrap_or_default();
        let has_tld =
            domain.contains(&b'.') && tld.len() >= 2 && tld.iter().all(u8::is_ascii_alphabetic);
        if start < at && has_tld {
            found.push(String::from_utf8_lossy(&data[start..end]).into_owned());
        }
    }
    found
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                collect_files(&path, files);
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
}

#[test]
fn scanner_flags_personal_but_not_noreply_or_fixture_addresses() {
    let personal = format!("{}@{}", "someone", "example-mail.test");
    let text = format!(
        "a {personal}, noreply@anthropic.com, 1+x@users.noreply.github.com, p@example.com, +@unittest.skip"
    );
    let flagged = addresses_in(text.as_bytes());
    assert_eq!(
        flagged.iter().filter(|a| !is_kept(a)).collect::<Vec<_>>(),
        vec![&personal]
    );
}

#[test]
fn committed_bench_files_contain_no_personal_email_address() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_files(root, &mut files);
    assert!(
        files.len() > 100,
        "bench tree not found under {}",
        root.display()
    );
    let leaks: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let data = std::fs::read(path).unwrap();
            let bad = addresses_in(&data)
                .into_iter()
                .filter(|a| !is_kept(a))
                .count();
            (bad > 0).then(|| format!("{} ({bad} address(es))", path.display()))
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "personal email addresses in bench files; redact them with bench/redact.py:\n{}",
        leaks.join("\n")
    );
}
