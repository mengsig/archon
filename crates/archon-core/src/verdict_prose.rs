//! Prose after a pass that reports a problem the pass would deny:
//! `Changes needed in X`, `Issues found: …`, `Still broken on Y`.

use crate::run::DENIES;

const PROBLEMS: [&str; 8] = [
    "issue", "issues", "problem", "problems", "bug", "bugs", "error", "errors",
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
const NEEDED: [&str; 1] = ["needed"];
const STILL: [&str; 4] = ["still", "remain", "remains", "remaining"];
const BROKEN: [&str; 6] = ["broken", "failing", "failed", "fails", "crashing", "wrong"];
const CHECKS: [&str; 8] = [
    "test", "tests", "check", "checks", "ci", "build", "lint", "suite",
];
const RED: [&str; 2] = ["red", "broken"];

/// Words that may open or modify a noun phrase (`Two new issues`, `Further work`).
const MODIFIERS: [&str; 27] = [
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
];
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
/// A report with one of these after it (`issues found and fixed`) is closed.
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
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_lowercase)
                .collect()
        })
        .collect();
    clauses.iter().enumerate().any(|(at, clause)| {
        let words: Vec<&str> = clause.iter().map(String::as_str).collect();
        let Some(end) = clause_report(&words) else {
            return false;
        };
        let next = clauses.get(at + 1).and_then(|next| next.first());
        let zeroed = words
            .get(end)
            .copied()
            .or(next.map(String::as_str))
            .is_some_and(|word| ZERO.contains(&word) || DENIES.contains(&word));
        !zeroed && !words[end..].iter().any(|w| CLOSED.contains(w))
    })
}

/// Where in `words` the first report ends (just past its verb), if any.
fn clause_report(words: &[&str]) -> Option<usize> {
    let lead = words.iter().take_while(|w| is_modifier(w)).count();
    let first = words.get(lead).copied().unwrap_or_default();
    let rest = words.get(lead + 1..).unwrap_or_default();
    let opening = if PROBLEMS.contains(&first) {
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
    let last = before.iter().rev().find(|w| !is_modifier(w));
    last.is_none_or(|w| ["and", "also", "then", "plus"].contains(w))
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
    MODIFIERS.contains(&word) || word.bytes().all(|b| b.is_ascii_digit())
}

/// The length of the prefix of `rest` that ends in a word of `verbs`, after
/// up to [`MAX_FILLERS`] [`FILLERS`].
fn follows(rest: &[&str], verbs: &[&[&str]]) -> Option<usize> {
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
