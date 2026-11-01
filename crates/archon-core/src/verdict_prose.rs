//! Prose after a pass that reports a problem the pass would deny:
//! `Changes needed in X`, `Issues found: …`, `Still broken on Y`.

use crate::run::DENIES;

const PROBLEMS: [&str; 10] = [
    "issue", "issues", "problem", "problems", "bug", "bugs", "error", "errors", "defect", "defects",
];
const CHANGES: [&str; 8] = [
    "change",
    "changes",
    "fix",
    "fixes",
    "work",
    "rework",
    "revisions",
    "edits",
];
/// Reports a finding; `noted` is left out so `non-blocking issues noted` stays a pass.
const FOUND: [&str; 4] = ["found", "detected", "identified", "discovered"];
const REMAIN: [&str; 7] = [
    "remain",
    "remains",
    "remaining",
    "persist",
    "persists",
    "exist",
    "exists",
];
/// Where a problem may also linger (`Bug still present`) that a change may not.
const LINGER: [&str; 4] = ["open", "present", "there", "unfixed"];
const NEEDED: [&str; 2] = ["needed", "necessary"];
const STILL: [&str; 4] = ["still", "remain", "remains", "remaining"];
const BROKEN: [&str; 10] = [
    "broken", "failing", "failed", "fails", "crashing", "crashes", "hangs", "panics", "hanging",
    "wrong",
];
const CHECKS: [&str; 8] = [
    "test", "tests", "check", "checks", "ci", "build", "lint", "suite",
];
const RED: [&str; 2] = ["red", "broken"];

/// Words that may open or modify a noun phrase (`Two new issues`, `Further work`).
const MODIFIERS: [&str; 31] = [
    "a",
    "an",
    "some",
    "several",
    "few",
    "many",
    "multiple",
    "more",
    "further",
    "additional",
    "new",
    "other",
    "minor",
    "major",
    "serious",
    "real",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "needed",
    "blocking",
    "critical",
    "remaining",
    "open",
    "known",
    "outstanding",
];
/// Words that make a noun after them refer to something already said or fixed.
const REFERENTIAL: [&str; 17] = [
    "the", "this", "that", "these", "those", "its", "their", "which", "of", "in", "from", "by",
    "with", "to", "fix", "fixes", "fixed",
];
/// Words that, opening a noun phrase, list what is left (`Remaining issues: …`).
const LISTING: [&str; 4] = ["remaining", "open", "known", "outstanding"];
/// Words allowed between a noun and the verb that reports it.
const FILLERS: [&str; 18] = [
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "have",
    "has",
    "also",
    "still",
    "currently",
    "now",
    "will",
    "would",
    "should",
    "may",
    "might",
    "could",
];
const MAX_FILLERS: usize = 3;
/// A report followed by one of these (`Issues found: none`) reports nothing.
const ZERO: [&str; 5] = ["none", "zero", "0", "nil", "nothing"];
/// A report directly followed by one of these (`issues found and fixed`) is closed.
const CLOSED: [&str; 5] = ["fixed", "resolved", "addressed", "corrected", "handled"];

/// Whether `explanation` reports, in any one clause, that changes are
/// needed, that issues were found or remain, or that something is still
/// broken, unless the words before deny it (`no issues found`) or after
/// zero it (`issues found: none`).
pub(crate) fn reports_problem(explanation: &str) -> bool {
    let clauses: Vec<Vec<String>> = explanation
        .split(['.', ';', ',', ':', '!', '?', '—', '–', '(', ')', '\n'])
        .map(|clause| {
            clause
                .to_lowercase()
                .replace("n/a", "none")
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect();
    assert!(!clauses.is_empty(), "a split yields a clause");
    clauses.iter().enumerate().any(|(at, clause)| {
        let words: Vec<&str> = clause.iter().map(String::as_str).collect();
        let Some(end) = clause_report(&words) else {
            return false;
        };
        let next: Vec<&str> = clauses
            .get(at + 1)
            .map_or(Vec::new(), |next| next.iter().map(String::as_str).collect());
        let after = if end < words.len() {
            &words[end..]
        } else {
            &next[..]
        };
        !zeroed(after) && !closed(&words[end..])
    })
}

/// Whether `after` a report names none of it (`none`, `0`) and says nothing was fixed.
fn zeroed(after: &[&str]) -> bool {
    let first = after.first();
    first.is_some_and(|w| ZERO.contains(w) || DENIES.contains(w))
        && !after.iter().any(|w| CLOSED.contains(w))
}

/// Whether the report is closed in its own words (`found and fixed`).
fn closed(after: &[&str]) -> bool {
    let skipped = after
        .iter()
        .take_while(|w| ["and", "then"].contains(w))
        .count();
    after.get(skipped).is_some_and(|w| CLOSED.contains(w))
}

/// Where in `words` the first report ends (just past its verb), if any.
fn clause_report(words: &[&str]) -> Option<usize> {
    let lead = words.iter().take_while(|w| is_modifier(w)).count();
    let first = words.get(lead).copied().unwrap_or_default();
    let rest = words.get(lead + 1..).unwrap_or_default();
    let listed = words[..lead].iter().any(|w| LISTING.contains(w));
    let opening = if PROBLEMS.contains(&first) && listed {
        Some(0)
    } else if PROBLEMS.contains(&first) {
        follows(rest, &[&FOUND, &REMAIN, &LINGER])
    } else if CHANGES.contains(&first) {
        follows(rest, &[&NEEDED, &REMAIN])
    } else {
        None
    };
    let end = opening
        .map(|end| lead + 1 + end)
        .or_else(|| (0..words.len()).find_map(|at| report_at(words, at)));
    assert!(
        end.is_none_or(|end| end <= words.len()),
        "a report ends in its clause"
    );
    end
}

/// The end of the report the word at `at` starts, if no denial directly precedes it.
fn report_at(words: &[&str], at: usize) -> Option<usize> {
    let word = words[at];
    let rest = &words[at + 1..];
    let end = if PROBLEMS.contains(&word) {
        let found: &[&str] = if starts_phrase(&words[..at]) {
            &FOUND
        } else {
            &[]
        };
        follows(rest, &[found, &REMAIN, &LINGER])
    } else if CHANGES.contains(&word) {
        follows(rest, &[&NEEDED, &REMAIN])
    } else if FOUND.contains(&word) {
        let lead = rest.iter().take_while(|w| is_modifier(w)).count();
        rest.get(lead)
            .filter(|w| PROBLEMS.contains(w))
            .map(|_| lead + 1)
    } else if STILL.contains(&word) {
        follows(rest, &[&BROKEN])
    } else if CHECKS.contains(&word) {
        follows(rest, &[&RED])
    } else {
        None
    }?;
    assert!(at + 1 + end <= words.len(), "a report ends in its clause");
    (!denied_before(&words[..at])).then_some(at + 1 + end)
}

/// Whether a noun after `before` opens its own phrase (`and bugs were found`),
/// not the object of an earlier verb (`fixes the bugs found`).
fn starts_phrase(before: &[&str]) -> bool {
    !before.iter().any(|w| REFERENTIAL.contains(w))
}

/// Whether the words just before a report (past modifiers and fillers) deny it.
fn denied_before(before: &[&str]) -> bool {
    let denial = before
        .iter()
        .rev()
        .find(|w| !is_modifier(w) && !FILLERS.contains(w));
    denial.is_some_and(|w| DENIES.contains(w) || ["nothing", "without"].contains(w))
}

fn is_modifier(word: &str) -> bool {
    MODIFIERS.contains(&word) || (word != "0" && word.bytes().all(|b| b.is_ascii_digit()))
}

/// The length of the prefix of `rest` that ends in a word of `verbs`, after
/// up to [`MAX_FILLERS`] [`FILLERS`].
fn follows(rest: &[&str], verbs: &[&[&str]]) -> Option<usize> {
    assert!(!verbs.is_empty(), "verb sets to look for");
    let skipped = rest
        .iter()
        .take(MAX_FILLERS)
        .take_while(|w| FILLERS.contains(w))
        .count();
    let word = rest.get(skipped)?;
    verbs
        .iter()
        .any(|set| set.contains(word))
        .then_some(skipped + 1)
}
