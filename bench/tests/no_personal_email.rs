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

/// Length of the domain at the start of `text`: dot-separated labels of
/// `[A-Za-z0-9-]`, the last one starting with two letters (longest match).
fn domain_len(text: &[u8]) -> usize {
    let is_label = |b: &u8| b.is_ascii_alphanumeric() || *b == b'-';
    let mut longest = 0;
    let mut pos = 0;
    let mut index = 0;
    loop {
        let label = text[pos..].iter().take_while(|b| is_label(b)).count();
        if label == 0 {
            break;
        }
        let starts_alpha = text[pos..pos + label]
            .iter()
            .take(2)
            .all(u8::is_ascii_alphabetic)
            && label >= 2;
        if index > 0 && starts_alpha {
            longest = pos + label;
        }
        pos += label;
        index += 1;
        if text.get(pos) != Some(&b'.') {
            break;
        }
        pos += 1;
    }
    longest
}

fn addresses_in(data: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for (at, _) in data.iter().enumerate().filter(|(_, byte)| **byte == b'@') {
        let mut start = at;
        while start > 0 && is_local(data[start - 1]) {
            start -= 1;
        }
        let domain = domain_len(&data[at + 1..]);
        let has_name = data[start..at].iter().any(u8::is_ascii_alphanumeric);
        if has_name && domain > 0 {
            found.push(String::from_utf8_lossy(&data[start..at + 1 + domain]).into_owned());
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

#[test]
fn scanner_keeps_fixture_address_before_sentence_punctuation() {
    let text = "mail p@example.com. then q@example.org, and r@example.net;";
    let flagged = addresses_in(text.as_bytes());
    assert_eq!(flagged, ["p@example.com", "q@example.org", "r@example.net"]);
    assert!(flagged.iter().all(|a| is_kept(a)));
}
