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
/// Other nouns a denial may list beside [`PROBLEMS`] (`no blockers or issues`).
const LISTED: [&str; 5] = [
    "blocker",
    "blockers",
    "regression",
    "regressions",
    "findings",
];
/// Words that join the nouns of a denial (`none of the`, `without any`, `issues or bugs`).
const LINKS: [&str; 8] = ["or", "nor", "any", "of", "the", "those", "these", "all"];
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

/// Whether `explanation` reports changes needed, issues found or remaining, or
/// something still broken, unless denied before it or zeroed after (or on `next_line`).
pub(crate) fn reports_problem(explanation: &str, next_line: &str) -> bool {
    let tokens = |text: &str| -> Vec<Vec<Vec<String>>> {
        text.split(['.', ';', ':', '!', '?', '—', '–', '(', ')', '\n'])
            .map(|sentence| sentence.split(',').map(words_of).collect::<Vec<_>>())
            .filter(|sentence| sentence.iter().any(|clause| !clause.is_empty()))
            .collect()
    };
    let sentences = tokens(explanation);
    let peek: Vec<String> = tokens(next_line)
        .into_iter()
        .flatten()
        .next()
        .unwrap_or_default();
    if sentences.is_empty() {
        return false;
    }
    let mut clauses: Vec<&[String]> = Vec::new();
    let mut carried: Vec<bool> = Vec::new();
    for sentence in &sentences {
        let mut denying = false;
        for clause in sentence {
            carried.push(denying);
            denying = denied_before(&as_strs(clause), denying);
            clauses.push(clause);
        }
    }
    let evaluated = clauses.len();
    clauses.push(&peek);
    (0..evaluated).any(|at| {
        let words = as_strs(clauses[at]);
        let Some(end) = clause_report(&words, carried[at]) else {
            return false;
        };
        let next = as_strs(clauses[at + 1]);
        let after = if end < words.len() {
            &words[end..]
        } else {
            &next[..]
        };
        !zeroed(after) && !closed(&words[end..])
    })
}

fn words_of(clause: &str) -> Vec<String> {
    clause
        .to_lowercase()
        .replace("n/a", "none")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn as_strs(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
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
fn clause_report(words: &[&str], carried: bool) -> Option<usize> {
    let lead = words.iter().take_while(|w| is_modifier(w)).count();
    let carried = carried && !words[..lead].iter().any(|w| counts(w));
    let first = words.get(lead).copied().unwrap_or_default();
    let rest = words.get(lead + 1..).unwrap_or_default();
    let listed = words[..lead].iter().any(|w| LISTING.contains(w));
    let opening = if PROBLEMS.contains(&first) && carried {
        None
    } else if PROBLEMS.contains(&first) && listed {
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
        .or_else(|| (0..words.len()).find_map(|at| report_at(words, at, carried)));
    assert!(
        end.is_none_or(|end| end <= words.len()),
        "a report ends in its clause"
    );
    end
}

/// The end of the report the word at `at` starts, if no denial directly precedes it.
fn report_at(words: &[&str], at: usize, carried: bool) -> Option<usize> {
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
    let carried = carried && PROBLEMS.contains(&word) && !words[..at].iter().any(|w| counts(w));
    let denied = denied_before(&words[..at], carried)
        || (STILL.contains(&word) && subject_denied(&words[..at]));
    (!denied).then_some(at + 1 + end)
}

/// Words that end a clause's denial: what follows them is a new claim.
const BREAKS: [&str; 8] = [
    "and", "but", "while", "yet", "though", "although", "however", "except",
];

/// Whether a denial leads the subject of a `still broken` (`none of the tests
/// are still broken`); a problem noun or a conjunction between them breaks it.
fn subject_denied(before: &[&str]) -> bool {
    for word in before.iter().rev() {
        if DENIES.contains(word) || ["nothing", "without"].contains(word) {
            return true;
        }
        if PROBLEMS.contains(word) || LISTED.contains(word) || BREAKS.contains(word) {
            return false;
        }
    }
    false
}

/// Whether a noun after `before` opens its own phrase (`and bugs were found`),
/// not the object of an earlier verb (`fixes the bugs found`).
fn starts_phrase(before: &[&str]) -> bool {
    !before.iter().any(|w| REFERENTIAL.contains(w))
}

/// Whether the words before a report, past modifiers, fillers and a noun list,
/// deny it; with none left, `carried` from the clauses before.
fn denied_before(before: &[&str], carried: bool) -> bool {
    let skipped = |w: &&str| {
        is_modifier(w)
            || FILLERS.contains(w)
            || LINKS.contains(w)
            || PROBLEMS.contains(w)
            || LISTED.contains(w)
    };
    match before.iter().rev().find(|w| !skipped(w)) {
        Some(w) => DENIES.contains(w) || ["nothing", "without"].contains(w),
        None => carried,
    }
}

/// Whether `word` counts or qualifies a noun (`3 bugs`): a new claim, not a list item.
fn counts(word: &str) -> bool {
    is_modifier(word) && word != "blocking" && word != "critical"
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
