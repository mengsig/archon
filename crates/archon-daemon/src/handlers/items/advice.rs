use std::collections::BTreeSet;

use archon_core::protocol::EnqueueNotice;
use archon_core::queue::QueueItem;
use archon_pipeline::builtin::DEFAULT_PIPELINE;

const FIX_ACTIONS: &[&str] = &["address", "correct", "fix", "repair", "resolve"];
const CHANGE_ACTIONS: &[&str] = &[
    "add",
    "break",
    "breaking",
    "change",
    "changes",
    "enforce",
    "introduce",
    "migrate",
    "modify",
    "prevent",
    "refactor",
    "remove",
    "replace",
    "rewrite",
    "update",
];

pub(super) fn notices(item: &QueueItem) -> Vec<EnqueueNotice> {
    debug_assert!(!item.title.trim().is_empty(), "a stored item has a title");
    debug_assert!(!item.pipeline.is_empty(), "enqueue resolves the pipeline");
    let title = item.title.to_lowercase();
    let brief = item.brief.to_lowercase();
    let mut notices = Vec::new();
    if item.pipeline == DEFAULT_PIPELINE && (describes_risk(&title) || describes_risk(&brief)) {
        notices.push(EnqueueNotice::DirectRisk);
    }
    if bundles_fixes(&title, &brief) {
        notices.push(EnqueueNotice::BundledFixes);
    }
    notices
}

fn describes_risk(text: &str) -> bool {
    text.split(['.', '!', '?', ';', ',', '\n']).any(|clause| {
        let words: Vec<_> = words(clause).collect();
        words.iter().enumerate().any(|(index, word)| {
            if !FIX_ACTIONS.contains(word) && !CHANGE_ACTIONS.contains(word) {
                return false;
            }
            let start = index.saturating_sub(4);
            let end = (index + 9).min(words.len());
            assert!(
                start <= index && index < end,
                "the window includes its verb"
            );
            assert!(end <= words.len(), "the window stays in its clause");
            risky_change(&words[start..end])
        })
    })
}

fn risky_change(words: &[&str]) -> bool {
    if words.iter().any(|word| {
        matches!(
            *word,
            "no" | "not" | "unchanged" | "docs" | "documentation" | "typo" | "mention"
        )
    }) {
        return false;
    }
    let has = |word| words.contains(&word);
    let breaking_format = ["break", "breaking"].into_iter().any(has)
        && ["public", "wire"].into_iter().any(has)
        && ["api", "format", "schema", "serialization"]
            .into_iter()
            .any(has);
    let migrate_storage = has("migrate")
        && ["data", "database", "schema", "storage", "table"]
            .into_iter()
            .any(has);
    breaking_format
        || migrate_storage
        || words.iter().any(|word| {
            matches!(*word, "race" | "races" | "lock" | "locks" | "auth")
                || *word != "migrate"
                    && [
                        "concurren",
                        "deadlock",
                        "mutex",
                        "locking",
                        "migrat",
                        "security",
                        "authenticat",
                        "authoriz",
                        "vulnerab",
                    ]
                    .iter()
                    .any(|stem| word.starts_with(stem))
        })
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

fn bundles_fixes(title: &str, brief: &str) -> bool {
    let mut labels = BTreeSet::new();
    let mut fix_entries = BTreeSet::new();
    for line in std::iter::once(title).chain(brief.lines()) {
        let entry = list_entry(line);
        let body = entry
            .unwrap_or(line)
            .trim_start_matches([' ', '\t', '#', '*', '`', '_']);
        let mut body_words = words(body);
        let fix = body_words
            .next()
            .is_some_and(|word| FIX_ACTIONS.contains(&word) || word == "fixes");
        if let Some(label) = heading_label(body) {
            labels.insert(label);
        } else if fix && body_words.next().is_some() {
            let mut named = label_tokens(body)
                .filter(|token| is_label(token))
                .peekable();
            if entry.is_some() && named.peek().is_none() {
                fix_entries.insert(body.trim_end());
            }
            labels.extend(named);
        }
        if labels.len() + fix_entries.len() >= 2 {
            return true;
        }
    }
    false
}

fn heading_label(body: &str) -> Option<&str> {
    let label = label_tokens(body).next().filter(|token| is_label(token))?;
    let rest = body
        .strip_prefix(label)?
        .trim_start_matches(['*', '`', '_']);
    if !rest.starts_with(|c: char| c.is_whitespace() || matches!(c, ':' | '.' | ')' | '—' | '–'))
    {
        return None;
    }
    words(rest).next()?;
    Some(label)
}

fn list_entry(line: &str) -> Option<&str> {
    let (marker, body) = line.trim_start().split_once(char::is_whitespace)?;
    let numbered = marker.strip_suffix(['.', ')']).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    (numbered || matches!(marker, "-" | "*" | "+")).then(|| body.trim_start())
}

fn label_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .filter(|token| !token.is_empty())
}

fn is_label(token: &str) -> bool {
    let Some((prefix, number)) = token.split_once('-') else {
        return false;
    };
    prefix.starts_with(|c: char| c.is_ascii_alphabetic())
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}
