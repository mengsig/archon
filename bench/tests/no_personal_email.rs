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

fn has_tld_label(domain: &[u8]) -> bool {
    let starts_well = domain
        .first()
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-');
    starts_well
        && domain
            .windows(3)
            .any(|w| w[0] == b'.' && w[1].is_ascii_alphabetic() && w[2].is_ascii_alphabetic())
}

fn addresses_in(data: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for (at, _) in data.iter().enumerate().filter(|(_, byte)| **byte == b'@') {
        let mut start = at;
        while start > 0 && is_local(data[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < data.len() && is_domain(data[end]) {
            end += 1;
        }
        let has_name = data[start..at].iter().any(u8::is_ascii_alphanumeric);
        if has_name && has_tld_label(&data[at + 1..end]) {
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

#[test]
fn scanner_covers_leading_punctuation_and_trailing_digits() {
    let leading = format!("_{}@{}", "someone", "example-mail.test");
    let digits = format!("{}@{}1", "someone", "example-mail.test");
    for text in [leading, digits] {
        let flagged = addresses_in(text.as_bytes());
        assert!(flagged.iter().any(|a| !is_kept(a)), "missed {text}");
    }
}
