//! The "Risk" section at the top of a pull request body: the level, what it
//! means, why it was chosen and the evidence. Archon owns only what lies
//! between its markers; the rest of the body is left as it is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Assessment;

const START: &str = "<!-- archon-risk -->";
const END: &str = "<!-- /archon-risk -->";
/// Files named one by one; the rest are counted.
const MAX_FILES: usize = 25;
/// Areas named one by one; the rest are counted.
const MAX_AREAS: usize = 12;

/// One file a diff touches, as `git diff --numstat` counts it; `None`
/// counts for a binary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// What a delivered change touches, against the commit it starts after.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
}

impl DiffStat {
    /// Read `git diff --numstat -z` output: per file `added\tremoved\tpath\0`,
    /// or `added\tremoved\t\0from\0to\0` for a rename. `Err` on anything else.
    pub fn from_numstat(raw: &str) -> Result<Self, String> {
        let mut fields = raw.split('\0');
        let mut files = Vec::new();
        while let Some(head) = fields.next().filter(|head| !head.is_empty()) {
            let mut parts = head.splitn(3, '\t');
            let (Some(added), Some(removed), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return Err(format!("unexpected `git diff --numstat` entry {head:?}"));
            };
            let path = match path {
                "" => {
                    let _from = fields.next();
                    fields.next().unwrap_or("").to_owned()
                }
                path => path.to_owned(),
            };
            if path.is_empty() {
                return Err(format!(
                    "a `git diff --numstat` entry names no path: {head:?}"
                ));
            }
            files.push(FileStat {
                path,
                added: added.parse().ok(),
                removed: removed.parse().ok(),
            });
        }
        Ok(Self { files })
    }

    fn lines(&self) -> (u64, u64) {
        let sum = |count: fn(&FileStat) -> Option<u64>| self.files.iter().filter_map(count).sum();
        (sum(|f| f.added), sum(|f| f.removed))
    }

    /// Each area the diff touches with how many of its files: the first two
    /// components of a deeper path, else its first, `(root)` for top files.
    fn areas(&self) -> BTreeMap<String, usize> {
        let mut areas = BTreeMap::new();
        for file in &self.files {
            let parts: Vec<&str> = file.path.split('/').collect();
            let area = match parts.len() {
                1 => "(root)".to_owned(),
                2 => parts[0].to_owned(),
                _ => format!("{}/{}", parts[0], parts[1]),
            };
            *areas.entry(area).or_insert(0) += 1;
        }
        areas
    }

    /// Whether every file is a test or documentation, judged by its path.
    fn only_tests_and_docs(&self) -> bool {
        !self.files.is_empty()
            && self
                .files
                .iter()
                .all(|f| is_test(&f.path) || is_doc(&f.path))
    }
}

fn is_test(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|dir| matches!(dir, "tests" | "test" | "__tests__" | "testdata"))
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name == "tests.rs"
}

fn is_doc(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("docs/")
        || [".md", ".rst", ".adoc"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// What a delivery knows besides the agents' calls.
#[derive(Debug, Clone, Copy)]
pub struct Evidence<'a> {
    /// What the delivered commits change; `None` when it could not be read.
    pub diff: Option<&'a DiffStat>,
    /// What the diff is measured against, e.g. `` `main` ``.
    pub against: &'a str,
    /// The verification commands the record stage listed.
    pub checks: &'a [String],
    /// The line Archon wrote for the run's review, if it holds one.
    pub review: Option<&'a str>,
    /// How setting the label went, in one sentence.
    pub label: &'a str,
}

/// The risk section for `assessment` with `evidence`, between its markers.
pub fn section(assessment: &Assessment, evidence: Evidence<'_>) -> String {
    assert!(
        !evidence.label.trim().is_empty(),
        "the label's fate is told"
    );
    let level = assessment.level;
    let mut text = format!(
        "{START}\n## Risk: {level}\n\n**{}**\n\n**Why this level.** {}\n\n{}\n\n**Evidence**\n\n",
        level.meaning(),
        assessment.decided,
        assessment.why.trim()
    );
    for line in evidence_lines(evidence) {
        text.push_str(&format!("- {line}\n"));
    }
    if !assessment.captures.is_empty() {
        text.push_str("\n**Captures**\n\n");
        for (n, url) in assessment.captures.iter().enumerate() {
            text.push_str(&format!("![capture {}]({url})\n", n + 1));
        }
    }
    text.push_str(END);
    debug_assert_eq!(text.matches(START).count(), 1, "one section");
    text
}

fn evidence_lines(evidence: Evidence<'_>) -> Vec<String> {
    let mut lines = match evidence.diff {
        Some(diff) => diff_lines(diff, evidence.against),
        None => vec![format!(
            "Diff: could not be read against {}.",
            evidence.against
        )],
    };
    let checks = match evidence.checks {
        [] => "none listed by the agent".to_owned(),
        checks => checks
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    lines.push(format!(
        "Checks run: {checks}; their results are in the evidence above."
    ));
    let review = evidence
        .review
        .unwrap_or("no review is on record for this change.");
    lines.push(format!("Review: {review}"));
    lines.push(format!("Label: {}", evidence.label.trim()));
    lines
}

fn diff_lines(diff: &DiffStat, against: &str) -> Vec<String> {
    let (added, removed) = diff.lines();
    let count = diff.files.len();
    let mut lines = vec![format!(
        "Diff: {count} file(s), +{added} −{removed}, against {against}."
    )];
    let areas = diff.areas();
    let mut named: Vec<String> = areas
        .iter()
        .take(MAX_AREAS)
        .map(|(area, files)| format!("`{area}` ({files})"))
        .collect();
    if areas.len() > MAX_AREAS {
        named.push(format!("and {} more", areas.len() - MAX_AREAS));
    }
    lines.push(format!("Areas: {}.", named.join(", ")));
    let mut files: Vec<String> = diff
        .files
        .iter()
        .take(MAX_FILES)
        .map(|f| format!("`{}`", f.path))
        .collect();
    if count > MAX_FILES {
        files.push(format!("and {} more", count - MAX_FILES));
    }
    lines.push(format!("Files: {}.", files.join(", ")));
    let kind = match diff.only_tests_and_docs() {
        true => "only tests and docs, by path.",
        false => "touches code beyond tests and docs, by path.",
    };
    lines.push(format!("Kind: {kind}"));
    lines
}

/// `body` with `section` at its top, replacing the one Archon wrote before;
/// a start marker with no end is left in place, as the body's own text.
pub fn with_section(body: &str, section: &str) -> String {
    assert!(
        section.starts_with(START) && section.ends_with(END),
        "a risk section"
    );
    let (before, after) = body
        .split_once(START)
        .and_then(|(before, rest)| Some((before, rest.split_once(END)?.1)))
        .unwrap_or(("", body));
    let rest: Vec<&str> = [before.trim(), after.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
    let body = match rest.is_empty() {
        true => format!("{section}\n"),
        false => format!("{section}\n\n{}\n", rest.join("\n\n")),
    };
    debug_assert!(body.starts_with(START), "the section leads");
    body
}
