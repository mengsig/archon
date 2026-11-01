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
const REMAIN: [&str; 6] = [
    "remain",
    "remains",
    "remaining",
    "persist",
    "persists",
    "exist",
];
const NEEDED: [&str; 1] = ["needed"];
const STILL: [&str; 4] = ["still", "remain", "remains", "remaining"];
const BROKEN: [&str; 6] = ["broken", "failing", "failed", "fails", "crashing", "wrong"];
const CHECKS: [&str; 8] = [
    "test", "tests", "check", "checks", "ci", "build", "lint", "suite",
];
const RED: [&str; 2] = ["red", "broken"];

/// Words that may open a noun phrase (`Two new issues`, `Further work`).
const QUANTIFIERS: [&str; 24] = [
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
];
/// Words allowed between a noun and the verb that reports it.
const FILLERS: [&str; 12] = [
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
];

/// Whether `explanation` reports, in any one clause, that changes are
/// needed, that issues were found or remain, or that something is still
/// broken; a clause that denies it first (`no issues found`) does not.
pub(crate) fn reports_problem(explanation: &str) -> bool {
    explanation
        .split(['.', ';', ',', ':', '!', '?', '—', '–', '(', ')', '\n'])
        .any(|clause| {
            let words: Vec<String> = clause
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_lowercase)
                .collect();
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            clause_reports(&words)
        })
}

fn clause_reports(words: &[&str]) -> bool {
    assert!(words.iter().all(|w| !w.is_empty()), "words are non-empty");
    let lead = words.iter().take_while(|w| is_quantifier(w)).count();
    assert!(lead <= words.len(), "the lead is inside the clause");
    let opens_with = |nouns: &[&str]| words.get(lead).is_some_and(|w| nouns.contains(w));
    let after_lead = &words[(lead + 1).min(words.len())..];
    let reported_first = opens_with(&PROBLEMS) && follows(after_lead, &FOUND_OR_REMAIN)
        || opens_with(&CHANGES) && follows(after_lead, &NEEDED);
    reported_first || (0..words.len()).any(|at| reported_at(words, at))
}

const FOUND_OR_REMAIN: [&str; 10] = [
    "found",
    "detected",
    "identified",
    "discovered",
    "remain",
    "remains",
    "remaining",
    "persist",
    "persists",
    "exist",
];

/// Whether the word at `at` starts a report not denied earlier in its clause.
fn reported_at(words: &[&str], at: usize) -> bool {
    let word = words[at];
    let rest = &words[at + 1..];
    if words[..at]
        .iter()
        .any(|w| DENIES.contains(w) || *w == "nothing")
    {
        return false;
    }
    if PROBLEMS.contains(&word) {
        return follows(rest, &REMAIN);
    }
    if FOUND.contains(&word) {
        let lead = rest.iter().take_while(|w| is_quantifier(w)).count();
        return rest.get(lead).is_some_and(|w| PROBLEMS.contains(w));
    }
    if STILL.contains(&word) {
        return follows(rest, &BROKEN);
    }
    CHECKS.contains(&word) && follows(rest, &RED)
}

fn is_quantifier(word: &str) -> bool {
    QUANTIFIERS.contains(&word) || word.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `verbs` come next in `rest`, after at most two [`FILLERS`].
fn follows(rest: &[&str], verbs: &[&str]) -> bool {
    let skipped = rest
        .iter()
        .take(2)
        .take_while(|w| FILLERS.contains(w))
        .count();
    rest.get(skipped).is_some_and(|w| verbs.contains(w))
}
