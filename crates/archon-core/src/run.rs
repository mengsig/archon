//! Pipeline runs and their stage runs: the durable record of what Archon
//! asked Aether to do for one queue item and what came back.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use crate::catalog::StageKind;
use crate::effort_sizing::RiskClass;
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QuestionId, QueueItemId, ReportId, StageRunId};
use crate::model::ModelChoice;
use crate::plan_origin::{PlanSource, SlotSizing};
use crate::verdict_prose::reports_problem;

mod restatement;
mod verdict_note;

pub use restatement::{CheckDecision, Restatement, Submission, VerdictCheck, VerdictReading};
pub use verdict_note::VerdictNote;

/// What a review/test stage concluded, as [`Verdict::extract`] reads it from
/// the stage agent's result; anything unreadable is `Unknown` and fails a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Unknown,
}

/// Finding severities that a passing verdict cannot carry.
const BLOCKING_SEVERITIES: [&str; 4] = ["blocking", "blocker", "critical", "high"];

/// `structured` keys a result may give its verdict under, in the order read.
const VERDICT_KEYS: [&str; 3] = ["verdict", "result", "status"];

/// The `fault` a finding gives when the environment, not the product, failed.
const ENVIRONMENT_FAULT: &str = "environment";

/// Whether `structured.findings` holds a finding that names the environment
/// as at fault (`"fault": "environment"`): what a stage that loops back only
/// to fix the environment (`on_fail.when`) loops for.
pub fn names_environment(structured: &serde_json::Value) -> bool {
    let Some(findings) = structured.get("findings").and_then(|f| f.as_array()) else {
        return false;
    };
    findings.iter().any(|finding| {
        let fault = finding.get("fault").and_then(|fault| fault.as_str());
        fault.is_some_and(|fault| fault.trim().eq_ignore_ascii_case(ENVIRONMENT_FAULT))
    })
}

impl Verdict {
    /// `structured.verdict`, else a readable `structured.result` or `status`,
    /// else the summary's last `VERDICT:` line. A pass counts only when
    /// nothing else in the result [disputes it](Self::pass_disputed).
    pub fn extract(summary: &str, structured: Option<&serde_json::Value>) -> Self {
        let verdict = structured
            .and_then(Self::from_structured)
            .or_else(|| Self::from_summary(summary))
            .unwrap_or(Self::Unknown);
        match verdict {
            Self::Pass if Self::pass_disputed(summary, structured) => Self::Unknown,
            verdict => verdict,
        }
    }

    /// Whether a result states a fail where it may give a verdict: a
    /// `structured` field or the summary's last `VERDICT:` line, either alone.
    pub fn states_fail(summary: &str, structured: Option<&serde_json::Value>) -> bool {
        let in_structured = structured.and_then(Self::as_worded) == Some(Self::Fail);
        in_structured || Self::from_summary(summary) == Some(Self::Fail)
    }

    /// Whether anything in a result takes a pass back: the summary's last
    /// `VERDICT:` line or [`summary_json`] verdict reading other than pass,
    /// or a blocking finding in `structured` or that JSON.
    pub fn pass_disputed(summary: &str, structured: Option<&serde_json::Value>) -> bool {
        Self::findings_dispute(summary, structured) || Self::words_dispute(summary)
    }

    /// Whether `structured`, the summary's JSON or a wrapped summary's JSON
    /// lists a finding that [blocks](Self::blocks) a pass.
    fn findings_dispute(summary: &str, structured: Option<&serde_json::Value>) -> bool {
        let json = summary_json(summary);
        // A summary written as `{summary, structured}` holds a summary of its own, read one level deep.
        let inner_json = wrapped_summary(summary).as_deref().and_then(summary_json);
        [structured, json.as_ref(), inner_json.as_ref()]
            .into_iter()
            .flatten()
            .any(Self::lists_blocking)
    }

    /// Whether the summary's words, or a wrapped summary's, read as anything but pass.
    fn words_dispute(summary: &str) -> bool {
        let inner = wrapped_summary(summary);
        let inner_disagrees = inner.as_deref().is_some_and(|inner| {
            let inner_json = summary_json(inner);
            Self::summary_disagrees(inner, inner_json.as_ref())
        });
        inner_disagrees || Self::summary_disagrees(summary, summary_json(summary).as_ref())
    }

    /// Whether the text after a JSON verdict opening the summary takes its
    /// pass back, as text after a `VERDICT:` line would. Only the last
    /// `VERDICT:` line is left out, as [`Self::from_summary`] judges it.
    /// A review gate reading that JSON as the verdict checks this too.
    pub fn json_taken_back(summary: &str) -> bool {
        let Some((data, rest)) = leading_json(summary) else {
            return false;
        };
        let data = match is_wrapper(&data) {
            true => &data["structured"],
            false => &data,
        };
        // JSON that states no verdict (a test report) is evidence; the lines decide.
        if data.get(VERDICT_KEYS[0]).is_none() {
            return false;
        }
        let last = last_verdict_line(rest).map(|(line, _)| line.as_ptr());
        let said: Vec<&str> = rest
            .lines()
            .filter(|line| Some(line.as_ptr()) != last)
            .collect();
        debug_assert!(said.len() + usize::from(last.is_some()) == rest.lines().count());
        trails_back(&said.join("\n"))
    }

    /// Whether the summary's last `VERDICT:` line or its JSON's verdict,
    /// `result` or `status` reads as anything but pass, in words: the JSON's
    /// findings are [`Self::findings_dispute`]'s to judge.
    fn summary_disagrees(summary: &str, json: Option<&serde_json::Value>) -> bool {
        let said = [Self::from_summary(summary), json.and_then(Self::as_worded)];
        said.into_iter().flatten().any(|said| said != Self::Pass)
    }

    /// The verdict `structured` states: its `verdict` key whenever it has
    /// one, `Unknown` unless it reads as a verdict, else the first `result`
    /// or `status` that opens with a verdict word or a `VERDICT:` label. A
    /// pass that [lists a blocking finding](Self::lists_blocking) is `Unknown`.
    pub fn from_structured(structured: &serde_json::Value) -> Option<Self> {
        match Self::as_worded(structured) {
            Some(Self::Pass) if Self::lists_blocking(structured) => Some(Self::Unknown),
            stated => stated,
        }
    }

    /// The verdict `structured`'s words state, before a pass is held to its
    /// findings: what tells a pass beside a blocking finding from an unreadable word.
    pub fn as_worded(structured: &serde_json::Value) -> Option<Self> {
        match structured.get(VERDICT_KEYS[0]) {
            Some(given) => {
                let text = given.as_str().unwrap_or_default();
                Some(Self::from_field(text).unwrap_or(Self::Unknown))
            }
            None => Self::other_field(structured).map(|(_, verdict)| verdict),
        }
    }

    /// Whether `structured.findings` holds one that [blocks](Self::blocks) a pass.
    pub fn lists_blocking(structured: &serde_json::Value) -> bool {
        let Some(findings) = structured.get("findings").and_then(|f| f.as_array()) else {
            return false;
        };
        findings.iter().any(Self::blocks)
    }

    /// Whether `finding` blocks a pass: its `"blocking"` mark when it has one,
    /// else a blocking `severity`, as findings were judged before the mark.
    pub fn blocks(finding: &serde_json::Value) -> bool {
        if let Some(marked) = finding.get("blocking").and_then(serde_json::Value::as_bool) {
            return marked;
        }
        let severity = finding.get("severity").and_then(|s| s.as_str());
        let severity = severity.unwrap_or_default().trim();
        BLOCKING_SEVERITIES
            .iter()
            .any(|b| b.eq_ignore_ascii_case(severity))
    }

    /// Whether `finding` is a follow-up that never fails a review: marked
    /// `"blocking": false`, whatever its `severity`.
    pub fn is_follow_up(finding: &serde_json::Value) -> bool {
        let marked = finding.get("blocking") == Some(&serde_json::Value::Bool(false));
        assert!(
            !(marked && Self::blocks(finding)),
            "a follow-up never blocks"
        );
        marked
    }

    /// The first `result` or `status` that states a verdict, and that verdict.
    fn other_field(structured: &serde_json::Value) -> Option<(&str, Self)> {
        VERDICT_KEYS[1..]
            .iter()
            .filter_map(|key| non_empty(structured.get(key)?.as_str()?))
            .find_map(|text| Some((text, Self::from_field(text)?)))
    }

    /// A field's verdict: its opening word, after a `VERDICT:` label if it
    /// opens with one, which is `Unknown` when unreadable. A verdict further
    /// in is an example or a quote. A whole field of [`PASS_WITH_FOLLOW_UPS`]
    /// is a pass, which [`Self::from_structured`] holds to its findings.
    fn from_field(text: &str) -> Option<Self> {
        let whole = text.trim();
        if PASS_WITH_FOLLOW_UPS
            .iter()
            .any(|word| word.eq_ignore_ascii_case(whole))
        {
            return Some(Self::Pass);
        }
        match strip_label(text, "verdict") {
            Some(value) => Some(Self::from_word(value)),
            None => Self::leading(text),
        }
    }

    /// What the last `VERDICT: <word>` line of `text` says, in any letter
    /// case and with light markdown (`**Verdict:** PASS`, `- verdict: fail`);
    /// `Unknown` when that line names no verdict, even if an earlier one did,
    /// or a pass is taken back on its line or the lines after it (a fail word,
    /// a negation or condition, an undenied blocker).
    pub fn from_summary(text: &str) -> Option<Self> {
        let (line, value) = last_verdict_line(text)?;
        let verdict = Self::from_word(value);
        let end = line.as_ptr() as usize - text.as_ptr() as usize + line.len();
        assert!(
            text.get(..end).is_some_and(|head| head.ends_with(line)),
            "a summary line"
        );
        let held = verdict == Self::Pass && trails_back(&text[end..]);
        Some(if held { Self::Unknown } else { verdict })
    }

    /// The text a result gives as its verdict, readable or not, to quote
    /// when it is refused: a structured field, else a `VERDICT:` line.
    pub fn stated(summary: &str, structured: Option<&serde_json::Value>) -> Option<String> {
        let line = || last_verdict_line(summary).map(|(line, _)| line.trim());
        let field = |key| structured?.get(key);
        let quote = |given: &serde_json::Value| match given.as_str() {
            Some(text) => text.trim().to_owned(),
            None => given.to_string(),
        };
        let given = structured.and_then(Self::from_structured);
        // The summary that takes a structured pass back is what refused it.
        let json = summary_json(summary);
        if given == Some(Self::Pass) && Self::summary_disagrees(summary, json.as_ref()) {
            let from_json = || VERDICT_KEYS.iter().find_map(|key| json.as_ref()?.get(key));
            return line().map(str::to_owned).or_else(|| from_json().map(quote));
        }
        if let Some(given) = field(VERDICT_KEYS[0]) {
            return Some(quote(given));
        }
        // The order `extract` reads in: a `result` or `status` it skipped is the last resort.
        let used = structured.and_then(Self::other_field).map(|(text, _)| text);
        let skipped = || {
            VERDICT_KEYS[1..]
                .iter()
                .find_map(|key| non_empty(field(key)?.as_str()?))
        };
        used.or_else(line).or_else(skipped).map(str::to_owned)
    }

    /// What the first word of `text` says, markdown and letter case aside.
    fn from_word(text: &str) -> Self {
        Self::leading(text).unwrap_or(Self::Unknown)
    }

    /// `None` when `text` does not open with a verdict word. A verdict word
    /// counts only when no word follows it before the first explanation
    /// separator (`—`, `:`, `;`, `.`, `!`, ` - `, a line break): any is a choice
    /// (`PASS or FAIL`) or a condition (`PASS, pending fixes`, `PASS (if CI agrees)`).
    /// A pass whose explanation [takes it back](holds_back) is `Unknown` too.
    fn leading(text: &str) -> Option<Self> {
        let (verdict, after) = Self::opening(text)?;
        let head = after
            .split(['—', '–', ':', ';', '.', '!', '\n'])
            .next()
            .and_then(|head| head.split(" - ").next())
            .unwrap_or_default();
        let qualified = head.chars().any(char::is_alphanumeric);
        let held = verdict == Self::Pass && holds_back(after);
        Some(if qualified || held {
            Self::Unknown
        } else {
            verdict
        })
    }

    /// The verdict phrase of [`PASS_WORDS`] or [`FAIL_WORDS`] that opens `text`,
    /// its longest if several do, and the text after it; `None` when none does.
    fn opening(text: &str) -> Option<(Self, &str)> {
        let start = text.find(char::is_alphanumeric)?;
        let mut end = start;
        let mut phrase = String::new();
        let mut found = None;
        for _ in 0..MAX_PHRASE_WORDS {
            let rest = &text[end..];
            let word = rest
                .find(|c: char| !c.is_alphanumeric())
                .unwrap_or(rest.len());
            if word == 0 {
                break;
            }
            phrase.push_str(&rest[..word]);
            end += word;
            if let Some(verdict) = Self::of_phrase(&phrase) {
                found = Some((verdict, end));
            }
            let gap = text[end..].len() - text[end..].trim_start_matches(WORD_JOINS).len();
            end += gap;
            phrase.push(' ');
        }
        let (verdict, at) = found?;
        assert!(at > start && text.is_char_boundary(at), "a phrase was read");
        Some((verdict, &text[at..]))
    }

    /// The verdict a whole `text` names in the vocabulary of [`PASS_WORDS`] and
    /// [`FAIL_WORDS`]: in any letter case, with `_`, `-` and spaces alike.
    pub fn of_phrase(text: &str) -> Option<Self> {
        let words: Vec<String> = text
            .split(WORD_JOINS)
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        let phrase = words.join(" ");
        if PASS_WORDS.contains(&phrase.as_str()) {
            return Some(Self::Pass);
        }
        FAIL_WORDS.contains(&phrase.as_str()).then_some(Self::Fail)
    }
}

/// Every word or phrase a verdict passes with. Archon's contracts ask for
/// `pass`; the rest are read, never asked for.
pub const PASS_WORDS: [&str; 8] = [
    "pass", "passed", "approve", "approved", "lgtm", "accept", "accepted", "ok",
];

/// Every word or phrase a verdict fails with, whatever findings it lists.
/// Archon's contracts ask for `fail`; the rest are read, never asked for.
pub const FAIL_WORDS: [&str; 11] = [
    "fail",
    "failed",
    "fails",
    "reject",
    "rejected",
    "changes requested",
    "request changes",
    "needs changes",
    "needs work",
    "blocking",
    "block",
];

/// What may join the words of a verdict phrase: `changes_requested`,
/// `changes-requested` and `changes requested` are one phrase.
const WORD_JOINS: [char; 3] = ['_', '-', ' '];

/// The most words a phrase of [`PASS_WORDS`] or [`FAIL_WORDS`] holds.
const MAX_PHRASE_WORDS: usize = 2;

/// Whole verdict fields that pass with follow-ups only: one listing a
/// blocking finding is no pass, and a summary line saying so stays unread.
const PASS_WITH_FOLLOW_UPS: [&str; 4] = [
    "approve_with_nonblocking_finding",
    "approve_with_nonblocking_findings",
    "pass_with_nonblocking_finding",
    "pass_with_nonblocking_findings",
];

/// Words that take a pass back wherever its statement holds them: the other
/// verdict, a second `verdict`, a negation (`don't` splits into `don`, `t`) or a condition.
const HOLDS_PASS: [&str; 43] = [
    "fail",
    "fails",
    "failed",
    "failing",
    "failure",
    "failures",
    "reject",
    "rejects",
    "rejected",
    "disapprove",
    "disapproved",
    "verdict",
    "not",
    "t",
    "never",
    "nor",
    "nope",
    "cannot",
    "dont",
    "cant",
    "wont",
    "until",
    "unless",
    "if",
    "pending",
    "but",
    "however",
    "except",
    "before",
    "after",
    "first",
    "only",
    "yet",
    "must",
    "required",
    "requires",
    "requested",
    "need",
    "needs",
    "missing",
    "unsafe",
    "unresolved",
    "outstanding",
];

/// What a pass may deny (`no blocking findings`, `non-blocking`) but never
/// report (`one blocker remains`).
const BLOCKERS: [&str; 7] = [
    "blocker",
    "blockers",
    "blocking",
    "blocked",
    "regression",
    "regressions",
    "critical",
];

/// Words that deny the blocker after them.
pub(crate) const DENIES: [&str; 6] = ["no", "non", "zero", "0", "without", "none"];

/// What a bare `no` may deny in a pass's explanation (`PASS. No findings`);
/// any other (`PASS — no tests`) says something the pass lacks.
const DENIABLE: [&str; 15] = [
    "critical",
    "bugs",
    "findings",
    "finding",
    "issues",
    "issue",
    "blocker",
    "blockers",
    "blocking",
    "regressions",
    "regression",
    "problems",
    "concerns",
    "defects",
    "objections",
];

/// Whether a pass's explanation takes it back: a [`HOLDS_PASS`] word, a
/// [`BLOCKERS`] word nothing [denies](DENIES), a `no` before anything but a
/// [`DENIABLE`] word, or [a reported problem](reports_problem). Any other
/// text after a plain pass word is explanation.
fn holds_back(explanation: &str) -> bool {
    holds_back_before(explanation, "")
}

/// [`holds_back`] for a summary line, whose `next_line` may zero a problem it reports.
fn holds_back_before(explanation: &str, next_line: &str) -> bool {
    let words: Vec<String> = explanation
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    let among = |set: &[&str], word: &str| set.contains(&word);
    words.iter().enumerate().any(|(at, word)| {
        let before = at.checked_sub(1).map_or("", |prev| words[prev].as_str());
        let after = words.get(at + 1).map_or("", String::as_str);
        let blocker = among(&BLOCKERS, word) && !among(&DENIES, before);
        let lacks = word == "no" && !among(&DENIABLE, after);
        among(&HOLDS_PASS, word) || blocker || lacks
    }) || reports_problem(explanation, next_line)
}

/// Whether the summary after a pass's verdict line takes it back: anything
/// there but blank lines and the `head: <hex>`/`base: <hex>` lines a review
/// names its commits in that [holds it back](holds_back).
fn trails_back(rest: &str) -> bool {
    let is_commit = |value: &str| {
        let value = value.trim().trim_matches(['`', '*']);
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit())
    };
    let lines: Vec<&str> = rest.lines().collect();
    lines.iter().enumerate().any(|(at, line)| {
        let commit = strip_label(line, "head").or_else(|| strip_label(line, "base"));
        let next = lines[at + 1..].iter().find(|l| !l.trim().is_empty());
        !commit.is_some_and(is_commit) && holds_back_before(line, next.copied().unwrap_or(""))
    })
}

/// The verdict JSON of `summary`: the whole summary, its one fenced block, or
/// else an object opening it on lines of its own (`leading_json`), unwrapped
/// from the `complete_task` arguments `{summary, structured}`.
pub fn summary_json(summary: &str) -> Option<serde_json::Value> {
    let data = raw_summary_json(summary)?;
    Some(match is_wrapper(&data) {
        true => data["structured"].clone(),
        false => data,
    })
}

/// The inner `summary` of a summary written as `{summary, structured}`.
fn wrapped_summary(summary: &str) -> Option<String> {
    let data = raw_summary_json(summary).filter(is_wrapper)?;
    Some(
        data["summary"]
            .as_str()
            .expect("a wrapper's string summary")
            .to_owned(),
    )
}

/// The whole summary, its one fenced block, or else the object opening it, as JSON.
fn raw_summary_json(summary: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(summary)
        .ok()
        .or_else(|| fenced_json(summary))
        .or_else(|| leading_json(summary).map(|(data, _)| data))
}

/// A JSON object on lines of its own that opens `summary` (blank lines aside)
/// and the text after it, as Codex reviewers write a verdict and then explain it.
/// `None` when that text holds a fenced block or any `{` or opens a `[`: a
/// second JSON value there could state another verdict, finding or commit.
fn leading_json(summary: &str) -> Option<(serde_json::Value, &str)> {
    let start = summary.find(|c: char| !c.is_whitespace())?;
    let at_line_start = start == 0 || summary[..start].ends_with('\n');
    let body = &summary[start..];
    if !at_line_start || !body.starts_with('{') {
        return None;
    }
    let mut values = serde_json::Deserializer::from_str(body).into_iter::<serde_json::Value>();
    let data = values.next()?.ok()?;
    let rest = &body[values.byte_offset()..];
    assert!(data.is_object(), "a value opened by `{{` is an object");
    let line_ends = rest.trim_start_matches([' ', '\t', '\r']).starts_with('\n');
    let another = rest.contains('{') || rest.trim_start().starts_with('[');
    let fenced = markdown_lines(rest)
        .any(|(kind, _)| matches!(kind, LineKind::FenceOpen | LineKind::NestedFenceOpen));
    if rest.trim().is_empty() || !line_ends || another || fenced {
        return None;
    }
    Some((data, rest))
}

/// Whether `data` is the `complete_task` arguments `{summary, structured}`.
fn is_wrapper(data: &serde_json::Value) -> bool {
    data.get("verdict").is_none()
        && data
            .get("summary")
            .is_some_and(serde_json::Value::is_string)
        && data
            .get("structured")
            .is_some_and(serde_json::Value::is_object)
}

/// The one fenced block of `text`, top level and untagged or tagged `json`,
/// as JSON; `None` for no block or several. A nested or quoted example is none.
fn fenced_json(text: &str) -> Option<serde_json::Value> {
    let mut blocks: Vec<(Option<&str>, String)> = Vec::new();
    for (kind, line) in markdown_lines(text) {
        match kind {
            LineKind::FenceOpen => {
                let info = line.trim_start_matches(['`', '~']).trim();
                blocks.push((Some(info), String::new()));
            }
            LineKind::NestedFenceOpen => blocks.push((None, String::new())),
            LineKind::Fenced => {
                let (_, body) = blocks
                    .last_mut()
                    .expect("a fenced line follows its opening");
                body.push_str(line);
                body.push('\n');
            }
            _ => {}
        }
    }
    let [(Some(info), body)] = blocks.as_slice() else {
        return None;
    };
    if !info.is_empty() && !info.eq_ignore_ascii_case("json") {
        return None;
    }
    serde_json::from_str(body.trim()).ok()
}

/// The last line of `text` that reads `VERDICT: …`, and its value. Lines in
/// fences, block quotes or indented under a list item or as code are examples.
fn last_verdict_line(text: &str) -> Option<(&str, &str)> {
    let last = markdown_lines(text)
        .filter(|(kind, _)| *kind == LineKind::Prose)
        .filter_map(|(_, line)| Some((line, strip_label(line, "verdict")?)))
        .last();
    debug_assert!(last.is_none_or(|(line, _)| text.contains(line)));
    last
}

/// What a markdown line is to a reader looking for a verdict in it. Only
/// what starts at column 0 is top level: list items nest by indentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Prose,
    /// A `>` block quote line, indented under four columns, or a line that
    /// lazily continues its paragraph.
    Quote,
    /// Indented outside a fence: nested in a list item, or code.
    Indented,
    /// A column-0 ``` or ~~~ line that opens a fenced block.
    FenceOpen,
    /// An indented fence opening, as in a list item: an example, never top level.
    NestedFenceOpen,
    /// A line inside a fenced block; an unclosed block runs to the end.
    Fenced,
    FenceClose,
}

/// Where [`markdown_lines`] is: in an open fence, or after a quote line.
#[derive(Default)]
struct Scan {
    fence: Option<(char, usize)>,
    quoted: bool,
}

/// Each line of `text` with its kind. A marker indented under four columns
/// opens a fence, and only a bare run of its character, at least as long, closes it.
pub fn markdown_lines(text: &str) -> impl Iterator<Item = (LineKind, &str)> {
    use LineKind::{FenceOpen, Fenced, NestedFenceOpen, Quote};
    text.lines().scan(Scan::default(), |scan, line| {
        let kind = line_kind(scan, line);
        debug_assert_eq!(
            scan.fence.is_some(),
            matches!(kind, FenceOpen | NestedFenceOpen | Fenced)
        );
        debug_assert!(!scan.quoted || kind == Quote || kind == LineKind::Indented);
        Some((kind, line))
    })
}

fn line_kind(scan: &mut Scan, line: &str) -> LineKind {
    let start = line.trim_start();
    let columns = indent_columns(line);
    if let Some(open) = scan.fence {
        if columns >= 4 || !closes(start, open) {
            return LineKind::Fenced;
        }
        scan.fence = None;
        return LineKind::FenceClose;
    }
    if columns < 4 {
        scan.fence = fence_marker(start);
    }
    // A quote's open paragraph runs on through any line but a blank one or one that interrupts it.
    scan.quoted &= !start.is_empty() && scan.fence.is_none() && !interrupts_paragraph(line);
    match (scan.fence.is_some(), columns) {
        (true, 0) => LineKind::FenceOpen,
        (true, _) => LineKind::NestedFenceOpen,
        (false, 0..4) if start.starts_with('>') => {
            scan.quoted = quote_holds_paragraph(start);
            LineKind::Quote
        }
        (false, 0) if scan.quoted => LineKind::Quote,
        (false, 0) => LineKind::Prose,
        (false, _) => LineKind::Indented,
    }
}

/// Whether `line` starts a block that interrupts a paragraph, as in
/// CommonMark: a heading, a thematic break, a bullet item or one numbered 1.
fn interrupts_paragraph(line: &str) -> bool {
    if indent_columns(line) >= 4 {
        return false;
    }
    let start = line.trim_start();
    let first_item = start.starts_with('1') && list_marker_len(start) == Some(2);
    let bullet = start.starts_with(['-', '*', '+']) && list_marker_len(start).is_some();
    atx_heading(start) || thematic_break(start) || bullet || first_item
}

/// Whether a `>` line `start` leaves a paragraph open, which the next line
/// may lazily continue: not after a heading, a fence, a break or code.
fn quote_holds_paragraph(start: &str) -> bool {
    assert!(start.starts_with('>'), "a quote line");
    let mut rest = start;
    loop {
        if thematic_break(rest) {
            return false;
        }
        if let Some(inner) = rest.strip_prefix('>') {
            rest = inner.strip_prefix([' ', '\t']).unwrap_or(inner);
        } else if let Some(marker) = list_marker_len(rest) {
            rest = &rest[marker..];
        } else {
            break;
        }
        if indent_columns(rest) >= 4 {
            return false;
        }
        rest = rest.trim_start();
    }
    let block = atx_heading(rest) || thematic_break(rest) || fence_marker(rest).is_some();
    !rest.is_empty() && !block && !empty_list_item(rest)
}

/// Whether `line` is a list marker with no text, an item with no paragraph.
fn empty_list_item(line: &str) -> bool {
    let line = line.trim_end();
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    match digits {
        0 => matches!(line, "-" | "*" | "+"),
        1..=9 => matches!(&line[digits..], "." | ")"),
        _ => false,
    }
}

/// The bytes of the list marker (`-`, `*`, `+`, `1.`, `12)`) opening `line`
/// before text, its following whitespace not included.
fn list_marker_len(line: &str) -> Option<usize> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let marker = match digits {
        0 if line.starts_with(['-', '*', '+']) => 1,
        1..=9 if line[digits..].starts_with(['.', ')']) => digits + 1,
        _ => return None,
    };
    let text = line[marker..].strip_prefix([' ', '\t'])?;
    (!text.trim().is_empty()).then_some(marker)
}

/// An ATX heading: one to six `#`, then whitespace or the end of the line.
fn atx_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    let rest = &line[hashes..];
    (1..=6).contains(&hashes) && (rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// A thematic break: three or more of one of `-`, `*`, `_`, spaces between.
fn thematic_break(line: &str) -> bool {
    let mut marks = line.chars().filter(|c| !matches!(c, ' ' | '\t'));
    let Some(mark) = marks.next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    let rest: Vec<char> = marks.collect();
    rest.len() >= 2 && rest.iter().all(|c| *c == mark)
}

/// The columns `line`'s leading spaces and tabs span, a tab reaching the next
/// multiple of four as in markdown.
fn indent_columns(line: &str) -> usize {
    let mut columns = 0;
    for c in line.chars() {
        columns = match c {
            ' ' => columns + 1,
            '\t' => columns + 4 - columns % 4,
            _ => break,
        };
    }
    debug_assert!(
        columns <= 4 * line.len(),
        "a tab spans at most four columns"
    );
    columns
}

/// The character and length of the code fence `line` opens or closes, if it is one.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = line.chars().take_while(|c| *c == mark).count();
    (run >= 3).then_some((mark, run))
}

/// Whether `line` closes a fence opened by `open`: its marker run, at least
/// as long, with nothing after it but whitespace.
fn closes(line: &str, open: (char, usize)) -> bool {
    let rest = line.trim_start_matches(open.0);
    let run = line.len() - rest.len();
    assert_eq!(run % open.0.len_utf8(), 0, "a run of one character");
    run / open.0.len_utf8() >= open.1 && rest.trim().is_empty()
}

fn non_empty(text: &str) -> Option<&str> {
    Some(text.trim()).filter(|text| !text.is_empty())
}

/// `line` without a leading `label:`, in any letter case, with light
/// markdown (`**Label:**`, `- label:`, `## Label:`) around the label allowed;
/// a block quote (`> label:`) is no label.
pub fn strip_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    assert!(!label.is_empty(), "a label to strip");
    let line = line
        .trim()
        .trim_start_matches(['*', '-', '_', '`', '#', ' ']);
    let (head, rest) = line.split_at_checked(label.len())?;
    if !head.eq_ignore_ascii_case(label) {
        return None;
    }
    let rest = rest.trim_start_matches(['*', '_']);
    rest.strip_prefix(':')
        .map(|value| value.trim_start_matches(['*', '_', ' ']))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StageState {
    /// Agent creation requested but not yet acknowledged by Aether.
    Spawning,
    Running,
    Completed,
    Failed {
        error: String,
    },
    Cancelled,
    /// Wall-clock limit from the stage spec expired; Archon cancelled it.
    TimedOut,
}

/// How a stage agent's Aether task settled, as read from `get_task` or a
/// `task.*` event. Input to the pipeline engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StageOutcome {
    Completed {
        summary: String,
        structured: Option<serde_json::Value>,
    },
    Failed {
        error: String,
        /// Infrastructure failure (engine died, Aether restarted) rather than
        /// the agent giving up; eligible for `limits.infra_retries`.
        infra: bool,
    },
    Cancelled {
        reason: Option<String>,
    },
    TimedOut,
}

impl StageOutcome {
    /// The prefix a stage uses in `fail_task` to escalate to the operator.
    pub const BLOCKED_PREFIX: &'static str = "BLOCKED:";

    /// The prefix a stage uses in `fail_task` to hand its item back to the
    /// orchestrator: the work needs another shape than this run has.
    pub const ESCALATE_PREFIX: &'static str = "ESCALATE:";

    /// What Aether's error says of an agent it stopped at its cost budget.
    pub const COST_BUDGET_STOP: &'static str = "cost budget of $";

    /// How Aether's budget message opens around the stop reason it names.
    pub const BUDGET_STOP_MESSAGE: &'static str = "Stopped by its budget (";

    /// What Aether's budget message says of the stopped agent's subtree, right
    /// after the stop reason it wraps.
    pub const BUDGET_STOP_SUBTREE: [&'static str; 2] = [
        "its live subtree was cancelled with it",
        "no new subtree cancellation was requested",
    ];

    /// Archon's whole label after a cost stop whose dollars may be notional.
    pub const NOTIONAL_COST_LABEL: &'static str = " (notional cost: Claude Code reports list-price \
        dollars whether or not its login bills them, and Aether showed no fresh subscription usage \
        window with headroom for it, so the budget was enforced; under a subscription, raise or \
        unset the item's budget)";

    /// How the daemon's error opens when Aether's disk stayed full through
    /// every spawn retry: an infrastructure stop, also on 1.6.3 records.
    pub const DISK_FULL_SPAWN: &'static str = "create_agent failed on a full disk";

    /// How the daemon's error opens when Aether or the way to it failed
    /// through every spawn retry: an infrastructure stop, also on 1.6.3 records.
    pub const SPAWN_KEPT_FAILING: &'static str = "create_agent kept failing: ";

    /// How the daemon's error opens when a failed stage's workspace is gone.
    pub const WORKSPACE_MISSING: &'static str = "workspace missing:";

    /// What follows the missing workspace's path when the daemon wraps the
    /// stage's own error: `workspace missing: <path>; the engine said: <error>`.
    pub const ENGINE_SAID: &'static str = "; the engine said: ";

    /// Whether `error` is exactly Aether's cost-budget stop: its bare reason or
    /// its whole budget message, with nothing after but Archon's notional label.
    pub fn is_cost_budget_stop(error: &str) -> bool {
        let stop = error
            .strip_suffix(Self::NOTIONAL_COST_LABEL)
            .unwrap_or(error);
        match stop.strip_prefix(Self::BUDGET_STOP_MESSAGE) {
            Some(wrapped) => cost_stop_tail(wrapped).is_some_and(is_budget_message_rest),
            None => cost_stop_tail(stop) == Some(""),
        }
    }

    pub fn is_operator_blocker(&self) -> bool {
        matches!(self, Self::Failed { error, .. } if error.trim_start().starts_with(Self::BLOCKED_PREFIX))
    }

    /// Whether `error` ends its run at once: an operator blocker or an
    /// escalation, neither of which a fix loop or a retry can resolve.
    pub fn ends_run(error: &str) -> bool {
        let error = error.trim_start();
        error.starts_with(Self::BLOCKED_PREFIX) || error.starts_with(Self::ESCALATE_PREFIX)
    }

    /// Whether a completed result states its own failure: a fail verdict
    /// ([`Verdict::states_fail`]), or a summary opening as a `fail_task`
    /// error that [ends its run](Self::ends_run) would.
    pub fn states_failure(summary: &str, structured: Option<&serde_json::Value>) -> bool {
        Self::ends_run(summary) || Verdict::states_fail(summary, structured)
    }

    /// The error of the run that a stage's blocker or escalation `error`
    /// ends: trimmed, and clipped to what a run's error carries.
    pub fn run_stop_error(error: &str) -> String {
        let stop = clip(error.trim(), MAX_RUN_ERROR_CHARS);
        assert!(stop.chars().count() <= MAX_RUN_ERROR_CHARS + CLIP_MARKER.chars().count());
        assert!(!stop.starts_with(char::is_whitespace), "trimmed");
        stop
    }
}

/// Errors carried into `RunState::Failed` are capped; the full feedback
/// stays in the stage runs.
pub const MAX_RUN_ERROR_CHARS: usize = 8_000;

/// What [`clip`] ends a text it cut with.
pub const CLIP_MARKER: &str = "\n\n[… truncated by Archon]";

/// At most `max` characters, cut on a char boundary with a visible marker.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}{CLIP_MARKER}", &text[..cut]),
    }
}

/// What follows Aether's stop reason `cost budget of ${limit}: {cost} spent`
/// at the start of `text`; `None` when `text` does not open with one Aether
/// can write: it stops an agent only once the spend has reached the limit.
fn cost_stop_tail(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(StageOutcome::COST_BUDGET_STOP)?;
    let (limit, rest) = rest.split_once(": ")?;
    let (cost, tail) = rest.split_once(" spent")?;
    let spend = aether_spend(cost)?;
    let budget = aether_limit(limit)?;
    reaches(spend, limit, budget).then_some(tail)
}

/// Whether `rest` is all of Aether's budget message after the reason:
/// `); {subtree}, {n} descendant(s) were live at the stop. Details are in the
/// exhaustion.failed event.`
fn is_budget_message_rest(rest: &str) -> bool {
    let Some(rest) = rest.strip_prefix("); ") else {
        return false;
    };
    let after_subtree = StageOutcome::BUDGET_STOP_SUBTREE
        .iter()
        .find_map(|subtree| rest.strip_prefix(subtree)?.strip_prefix(", "));
    let Some(counted) = after_subtree else {
        return false;
    };
    let digits = counted.bytes().take_while(u8::is_ascii_digit).count();
    let (count, said) = counted.split_at(digits);
    let ending =
        " descendant(s) were live at the stop. Details are in the exhaustion.failed event.";
    // Aether counts the descendants in a `u64`.
    is_whole_number(count) && count.parse::<u64>().is_ok() && said == ending
}

/// The labels Aether gives a cost gap (`CostGap::label`), each in the
/// parentheses after a spend that is a floor.
const AETHER_COST_GAPS: [&str; 5] = [
    "some unpriced",
    "some cost unknown",
    "usage not fully reported",
    "interrupted run, cost unknown",
    "multiple cost gaps",
];

/// The dollars of `cost` when it is a spend as Aether's `cost_line` says one
/// in a stop: `$1.2345`, or a floor `≥ $1.2345 (some unpriced)`. A stop has
/// reached a positive limit, so its spend is never `not reported`.
fn aether_spend(cost: &str) -> Option<&str> {
    let dollars = match cost.strip_prefix("≥ $") {
        None => cost.strip_prefix('$')?,
        Some(floor) => {
            let (dollars, gap) = floor.split_once(' ')?;
            let label = gap.strip_prefix('(')?.strip_suffix(')')?;
            AETHER_COST_GAPS.contains(&label).then_some(dollars)?
        }
    };
    is_aether_spend(dollars).then_some(dollars)
}

/// Digits as a formatter writes a whole number: no sign, no leading zero.
fn is_whole_number(text: &str) -> bool {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    digits && (text == "0" || !text.starts_with('0'))
}

/// Whether `text` is dollars exactly as Aether writes a spend (`{:.4}`): a
/// plain decimal of four places, with no sign or exponent, that Rust writes
/// back unchanged for the amount it names.
fn is_aether_spend(text: &str) -> bool {
    let Some((whole, fraction)) = text.split_once('.') else {
        return false;
    };
    let four = fraction.len() == 4 && fraction.bytes().all(|b| b.is_ascii_digit());
    let written = |spent: f64| format!("{spent:.4}") == text;
    is_whole_number(whole) && four && text.parse::<f64>().is_ok_and(written)
}

/// Aether's `format_usd` for a budget, which it keeps positive and finite:
/// six decimals with the zeros trimmed, or the shortest exact form of an
/// amount below that precision.
fn aether_format_usd(amount: f64) -> String {
    assert!(
        amount.is_finite() && amount > 0.0,
        "Aether refuses any other budget"
    );
    let fixed = format!("{amount:.6}");
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    assert!(!trimmed.is_empty(), "formatting {amount} left nothing");
    if trimmed == "0" {
        return amount.to_string();
    }
    trimmed.to_owned()
}

/// The budget `text` names when it is one exactly as Aether writes it: a plain
/// decimal that [`aether_format_usd`] writes back unchanged for that amount.
fn aether_limit(text: &str) -> Option<f64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
    let plain = is_whole_number(whole)
        && !fraction.is_empty()
        && fraction.bytes().all(|b| b.is_ascii_digit());
    if !plain {
        return None;
    }
    let budget = text.parse::<f64>().ok()?;
    let positive = budget.is_finite() && budget > 0.0;
    (positive && aether_format_usd(budget) == text).then_some(budget)
}

/// Whether a spend Aether writes as `spend` can have reached the `budget` it
/// writes as `limit`. Aether stops at `spend >= limit`, so at four decimals
/// the spend reads no less than the least budget written as `limit` does.
fn reaches(spend: &str, limit: &str, budget: f64) -> bool {
    assert!(is_aether_spend(spend), "{spend} is no spend Aether writes");
    let least = least_budget_written(limit, budget);
    let floor = format!("{least:.4}");
    // Both are plain decimals of four places: the longer one is the larger.
    (spend.len(), spend) >= (floor.len(), floor.as_str())
}

/// The least amount Aether writes as `limit`, as it writes `budget`. It never
/// writes a larger amount as a smaller number, so those amounts are neighbours.
fn least_budget_written(limit: &str, budget: f64) -> f64 {
    assert_eq!(aether_format_usd(budget), limit, "the budget reads so");
    // Positive floats order as their bits do, and zero is no budget.
    let (mut below, mut least) = (0_u64, budget.to_bits());
    while least - below > 1 {
        let middle = below + (least - below) / 2;
        if aether_format_usd(f64::from_bits(middle)) == limit {
            least = middle;
        } else {
            below = middle;
        }
    }
    let least = f64::from_bits(least);
    assert!(least > 0.0 && least <= budget, "{least} is a budget");
    least
}

impl StageState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Spawning | Self::Running)
    }
}

/// One agent's worth of work: a stage, its attempt number, and for fan-out
/// stages which parallel slot it filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageRun {
    pub id: StageRunId,
    pub run: PipelineRunId,
    pub stage_id: String,
    /// The stage's kind in the pipeline this attempt ran in; `None` on stage
    /// runs recorded before Archon kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StageKind>,
    /// Whether the stage's work is only the review its pipeline declares (a
    /// review-only run's lead); `None` on stage runs recorded before Archon kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_reviews: Option<bool>,
    /// 1-based; grows with each retry/fix loop iteration.
    pub attempt: u32,
    /// 0-based slot within a fan-out stage; 0 for single-agent stages.
    pub fan_index: u32,
    pub choice: ModelChoice,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_segments: Vec<ModelSegment>,
    #[serde(default)]
    pub infrastructure_failure: bool,
    #[serde(default)]
    pub host_context: crate::config::HostContext,
    /// How the attempt stands to its stage's one record repair turn; `None`
    /// on a stage run from before Archon gave one, which is replayed as then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_repair: Option<RecordRepair>,
    /// Aether identifiers, present once Aether acknowledged creation.
    pub aether_agent: Option<AetherAgentId>,
    pub aether_task: Option<AetherTaskId>,
    pub state: StageState,
    pub verdict: Option<Verdict>,
    /// Why `verdict` is `Unknown`; `None` for any other verdict, and on stage
    /// runs recorded before Archon kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_note: Option<VerdictNote>,
    /// How the attempt's effort was chosen: `risk_class`, `effort_source` and
    /// `effort_reasons`, each absent when the sizer never decided its plan key.
    #[serde(flatten)]
    pub sizing: AttemptSizing,
    /// The one restatement of its verdict the attempt was asked for through
    /// `submit_verdict`; `None` when it was never asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restatement: Option<Restatement>,
    pub summary: Option<String>,
    pub structured: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

/// How an attempt's effort was chosen, copied from its item's stored plan as
/// the attempt is reserved. Display and grouping only: nothing routes on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AttemptSizing {
    /// The class the attempt's plan key was sized with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_class: Option<RiskClass>,
    /// Where the effort it ran at came from: the sizer, or the pin it advised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_source: Option<PlanSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_reasons: Option<String>,
}

impl From<SlotSizing> for AttemptSizing {
    fn from(sized: SlotSizing) -> Self {
        Self {
            risk_class: Some(sized.risk_class),
            effort_source: Some(sized.effort_source),
            effort_reasons: Some(sized.reason),
        }
    }
}

/// Whether a stage's one record repair turn is still to give when an attempt
/// of it completes with a broken delivery record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecordRepair {
    /// A broken record of this attempt gets the repair turn.
    Open,
    /// The attempt is the repair turn or follows it: a broken record of it
    /// fails the run, and its agent is not asked for the record again.
    Spent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelSegment {
    pub delivery: String,
    pub choice: ModelChoice,
    pub started_at: Timestamp,
    pub cost_before_usd: f64,
}

/// The key under a result's `review` where Archon, and only Archon, writes
/// why it refused the review pass the result claims.
pub const REVIEW_REFUSED_KEY: &str = "unverified";

/// The headline a gate fails a stage with when the review its pipeline
/// declares is not on record, e.g. a skip the pipeline does not allow.
pub const REVIEW_NOT_ON_RECORD: &str = "the review this pipeline declares is not on record";

/// The headline a gate fails a completed stage with when its own result
/// states a failure: a fail verdict, or a summary opening as a run stop.
pub const STATED_FAILURE: &str = "its agent stated a failure";

impl StageRun {
    /// Whether its agent failed on its engine's login: an infrastructure
    /// failure, never evidence about the model.
    pub fn failed_on_engine_login(&self) -> bool {
        match &self.state {
            StageState::Failed { error } => {
                self.infrastructure_failure && crate::engine_login::is_login_failure(error)
            }
            _ => false,
        }
    }

    pub fn current_choice(&self) -> &ModelChoice {
        self.model_segments
            .last()
            .map_or(&self.choice, |s| &s.choice)
    }

    /// Whether Archon refused the review pass this stage's result claims.
    pub fn review_refused(&self) -> bool {
        let review = self.structured.as_ref().and_then(|s| s.get("review"));
        let why = review.and_then(|review| review.get(REVIEW_REFUSED_KEY));
        why.and_then(serde_json::Value::as_str)
            .is_some_and(|why| !why.trim().is_empty())
    }
}

/// The newest `pr_url` a completed stage reported (the ship stage's), from
/// stage runs in the order they started.
pub fn pr_url<'a>(stages: impl DoubleEndedIterator<Item = &'a StageRun>) -> Option<&'a str> {
    stages
        .rev()
        .filter(|s| s.state == StageState::Completed)
        .find_map(|s| s.structured.as_ref()?.get("pr_url")?.as_str())
}

/// For stage runs given as `(stage, attempt, slot)` in the order they started,
/// how many earlier runs each slot had: 0 for its first run, `n` for its `n`th
/// retry (a refused swarm plan's correction, an infrastructure retry).
pub fn slot_retries<'a>(slots: impl IntoIterator<Item = (&'a str, u32, u32)>) -> Vec<u32> {
    let mut seen: std::collections::BTreeMap<(&str, u32, u32), u32> = Default::default();
    let retries: Vec<u32> = slots
        .into_iter()
        .map(|slot| {
            let runs = seen.entry(slot).or_insert(0);
            *runs += 1;
            *runs - 1
        })
        .collect();
    let total: u32 = seen.values().sum();
    assert_eq!(
        total as usize,
        retries.len(),
        "every stage run is counted once"
    );
    retries
}

/// What follows `attempt N slot M` in a stage run's line: its retry, if any.
pub fn retry_label(retry: u32) -> String {
    match retry {
        0 => String::new(),
        n => format!(" retry {n}"),
    }
}

/// The kv key of the review tally archond keeps for stage run `stage`: the
/// failing verdicts the stage's reviewers returned.
pub fn review_tally_key(stage: &StageRunId) -> String {
    format!("review_rounds:{stage}")
}

/// The most of a field of `structured.review` that Archon reads: a verdict,
/// a reviewer's model or a skip reason is one line of a summary.
pub const MAX_REVIEW_FIELD_CHARS: usize = 400;

/// `text` as one trimmed line of at most [`MAX_REVIEW_FIELD_CHARS`]
/// characters: how the gate and the outcome store read a review's field.
pub fn review_field(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = joined.chars().take(MAX_REVIEW_FIELD_CHARS).collect();
    assert!(!line.contains('\n'), "one line");
    assert!(line.chars().count() <= MAX_REVIEW_FIELD_CHARS);
    line
}

/// A stored review tally, read for the failing verdicts it holds: the fails
/// with a blocking finding, and the rest as (agent, head). It decodes only as
/// archond stores one, so a tally of any other shape is corrupt.
#[derive(Debug, Deserialize)]
#[serde(try_from = "serde_json::Value")]
pub struct TalliedFails {
    reviewers: usize,
}

impl TryFrom<serde_json::Value> for TalliedFails {
    type Error = String;

    fn try_from(stored: serde_json::Value) -> Result<Self, Self::Error> {
        if !stored.is_object() {
            return Err(format!("{stored} is no tally: archond stores an object"));
        }
        let tally: StoredTally = serde_json::from_value(stored).map_err(|e| e.to_string())?;
        Ok(Self {
            reviewers: tally.fails.len() + tally.other_fails.len(),
        })
    }
}

impl TalliedFails {
    /// How many failing verdicts the tally holds: archond keeps one a reviewer.
    pub fn reviewers(&self) -> usize {
        self.reviewers
    }
}

/// Every field archond has kept in a tally, each in the type it wrote: `told`
/// by 1.4 alone, the rest as they were added.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    dead_code,
    reason = "a field nothing reads still holds the tally to its shape"
)]
struct StoredTally {
    fails: Vec<StoredFail>,
    #[serde(default)]
    told: bool,
    #[serde(default)]
    read: Vec<TalliedAgent>,
    #[serde(default)]
    earlier: Option<Vec<TalliedAgent>>,
    #[serde(default)]
    other_fails: Vec<(TalliedAgent, ReadCommit)>,
    #[serde(default)]
    every_fail_kept: bool,
    #[serde(default)]
    reread: Vec<TalliedAgent>,
}

/// One failing verdict as archond keeps it, in either shape it has written:
/// its reviewer, the head that reviewer read, and its blocking findings or,
/// before they were described, their titles.
#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
#[expect(dead_code, reason = "decoding each field is the check")]
enum StoredFail {
    Described {
        agent: TalliedAgent,
        head: ReadCommit,
        findings: Vec<String>,
    },
    Titled {
        agent: TalliedAgent,
        head: ReadCommit,
        titles: Vec<String>,
    },
}

/// A reviewer as archond names one in a tally: the id Aether gave it, `agt_`
/// and a ULID in the 26 characters Aether writes.
#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
#[expect(dead_code, reason = "decoding it is the check")]
struct TalliedAgent(String);

impl TryFrom<String> for TalliedAgent {
    type Error = String;

    fn try_from(agent: String) -> Result<Self, Self::Error> {
        let ulid = agent.strip_prefix("agt_").unwrap_or_default();
        let written = |id: ulid::Ulid| id.to_string() == ulid;
        if ulid::Ulid::from_string(ulid).is_ok_and(written) {
            return Ok(Self(agent));
        }
        Err(format!("{agent:?} is no agent id Aether writes"))
    }
}

/// The commit a reviewer read, as archond takes one from a verdict: 7 to 64
/// lowercase hexadecimal digits.
#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
#[expect(dead_code, reason = "decoding it is the check")]
struct ReadCommit(String);

impl TryFrom<String> for ReadCommit {
    type Error = String;

    fn try_from(head: String) -> Result<Self, Self::Error> {
        let hex = head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if (7..=64).contains(&head.len()) && hex {
            return Ok(Self(head));
        }
        Err(format!("{head:?} is no commit a reviewer read"))
    }
}

/// How the error of a run that recovery could not resume opens: its stage
/// rows may be gone, so the store stopped it and no stage record explains it.
pub const UNRESUMABLE_RUN: &str = "cannot resume the run: ";

/// The error archond fails a stage and its run with when it refused the
/// stage's spawn before any agent existed.
pub fn refused_at_spawn(stage_id: &str, error: &ArchonError) -> String {
    format!("stage {stage_id} refused at spawn: {error}")
}

/// The stage id a run stopped by its item's dollar budget fails under; no
/// stage of that id runs.
pub const BUDGET_STAGE: &str = "budget";

/// The stage id a failed delivery is reported under; no agent runs it.
pub const SHIP_STEP: &str = "ship";

/// The stage id a run that recovery could not resume fails under when no stage
/// was live; no agent runs it.
pub const RECOVERY_STAGE: &str = "recovery";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Running {
        stage_id: String,
        attempt: u32,
    },
    /// An operator gate (ARCHITECTURE §18.3): the stage passed and the run
    /// waits for the operator to approve, revise or reject its output.
    AwaitingOperator {
        stage_id: String,
        attempt: u32,
        subject: GateSubject,
    },
    Succeeded {
        summary: String,
    },
    Failed {
        stage_id: String,
        error: String,
    },
    Cancelled {
        reason: Option<String>,
    },
}

impl RunState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running { .. } | Self::AwaitingOperator { .. })
    }

    /// The stage a live run is at; `None` once it finished.
    pub fn live_stage(&self) -> Option<&str> {
        match self {
            Self::Running { stage_id, .. } | Self::AwaitingOperator { stage_id, .. } => {
                Some(stage_id)
            }
            _ => None,
        }
    }
}

/// What an operator gate asks the operator to judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateSubject {
    /// The `propose` stage's direction, before implementing.
    Proposal,
    /// The change report, before shipping.
    Report,
}

impl GateSubject {
    pub fn noun(self) -> &'static str {
        match self {
            Self::Proposal => "direction",
            Self::Report => "change report",
        }
    }
}

/// The operator's answer to a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    Approve {
        note: Option<String>,
    },
    /// Loop back with `feedback` in the prompt.
    Revise {
        feedback: String,
    },
    /// Cancel the item.
    Reject {
        reason: String,
    },
}

impl GateDecision {
    /// Read an operator's free-text answer: `yes`/`approve` approve (text
    /// after the word is a note), `reject` or a bare `no` rejects, and
    /// anything else (`no, use X` included) is revision feedback.
    pub fn from_answer(text: &str) -> Result<Self, ArchonError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ArchonError::Invalid("a gate answer needs text".into()));
        }
        let (word, rest) = match text.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim()),
            None => (text, ""),
        };
        let word = word
            .trim_end_matches(|c: char| c.is_ascii_punctuation())
            .to_ascii_lowercase();
        let note = (!rest.is_empty()).then(|| rest.to_owned());
        let decision = match word.as_str() {
            "yes" | "y" | "approve" | "approved" | "ok" | "lgtm" => Self::Approve { note },
            "no" | "n" if note.is_none() => Self::Reject {
                reason: "rejected by the operator".into(),
            },
            "reject" | "rejected" | "cancel" => Self::Reject {
                reason: note.unwrap_or_else(|| "rejected by the operator".into()),
            },
            _ => Self::Revise {
                feedback: text.to_owned(),
            },
        };
        Ok(decision)
    }

    pub fn word(&self) -> &'static str {
        match self {
            Self::Approve { .. } => "approved",
            Self::Revise { .. } => "revision requested",
            Self::Reject { .. } => "rejected",
        }
    }
}

/// One operator gate of a run, keyed by `(run, stage_id, attempt)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OperatorGate {
    pub run: PipelineRunId,
    pub item: QueueItemId,
    pub stage_id: String,
    pub attempt: u32,
    pub subject: GateSubject,
    /// The gated stage's summary.
    pub summary: String,
    /// The HTML page the stage wrote: an absolute path in the run's workspace.
    pub page: Option<String>,
    /// Lavish URL or file path the operator was shown.
    pub location: Option<String>,
    pub report: Option<ReportId>,
    pub question: Option<QuestionId>,
    pub decision: Option<GateDecision>,
    /// Why the gate closed undecided: its run ended first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn: Option<String>,
    pub opened_at: Timestamp,
    /// When it was decided or withdrawn.
    pub decided_at: Option<Timestamp>,
}

impl OperatorGate {
    pub fn is_open(&self) -> bool {
        self.decision.is_none() && self.withdrawn.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PipelineRun {
    pub id: PipelineRunId,
    pub item: QueueItemId,
    pub pipeline: String,
    /// A ship-kind stage in the frozen pipeline completed before this run succeeded.
    /// Older runs have no proof of shipping and keep normal workspace retention.
    #[serde(default)]
    pub shipped: bool,
    /// Aether run id shared by every stage agent, once the first exists.
    pub aether_run: Option<AetherRunId>,
    /// Worktree the stages share; for a team pipeline, `<team_root>/main`.
    pub workspace: std::path::PathBuf,
    /// Directory holding the run worktree and its team workers' worktrees,
    /// set only for pipelines with a team stage; the run node's scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_root: Option<std::path::PathBuf>,
    pub branch: String,
    pub state: RunState,
    pub cost_usd: f64,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

impl PipelineRun {
    pub fn finish(&mut self, state: RunState, at: Timestamp) -> Result<(), ArchonError> {
        if self.state.is_terminal() {
            return Err(ArchonError::InvalidTransition(format!(
                "run {} already finished",
                self.id
            )));
        }
        assert!(state.is_terminal(), "finish requires a terminal state");
        self.state = state;
        self.finished_at = Some(at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A refused swarm plan's correction reruns the same slot; each rerun is
    /// numbered so `attempt 1 slot 0` is not shown twice alike.
    #[test]
    fn each_rerun_of_a_slot_is_numbered_in_start_order() {
        let slots = [
            ("plan", 1, 0),
            ("plan", 1, 0),
            ("build", 1, 0),
            ("build", 1, 1),
            ("plan", 1, 0),
            ("plan", 2, 0),
        ];
        assert_eq!(slot_retries(slots), [0, 1, 0, 0, 2, 0]);
        assert_eq!(retry_label(0), "");
        assert_eq!(retry_label(2), " retry 2");
    }

    /// Only a finding's own `fault` names the environment: its wording, its
    /// title and any other key do not send a run back to setup.
    #[test]
    fn only_a_finding_whose_fault_is_environment_names_the_environment() {
        let naming = [
            json!({"findings": [{"fault": "environment"}]}),
            json!({"findings": [{"fault": "product"}, {"fault": " Environment "}]}),
        ];
        for structured in naming {
            assert!(names_environment(&structured), "{structured}");
        }
        let silent = [
            json!({"findings": [{"fault": "product"}]}),
            json!({"findings": [{"title": "the environment is broken", "severity": "high"}]}),
            json!({"findings": [{"fault": "environment or product"}, {"fault": true}]}),
            json!({"fault": "environment", "findings": []}),
            json!({"findings": "environment"}),
            json!({"verdict": "fail"}),
            json!(null),
        ];
        for structured in silent {
            assert!(!names_environment(&structured), "{structured}");
        }
    }

    #[test]
    fn a_json_object_opening_a_summary_on_its_own_lines_is_its_json() {
        let pass = json!({"verdict": "pass", "head": "abcdef1"});
        let text = pass.to_string();
        let pretty = serde_json::to_string_pretty(&pass).unwrap();
        for summary in [
            format!("{text}\nVERDICT: PASS"),
            format!("\n  \n{pretty}\n\nChecks passed."),
            format!("{text}  \r\nChecks passed."),
        ] {
            assert_eq!(summary_json(&summary), Some(pass.clone()), "{summary:?}");
        }
        for summary in [
            format!("Reviewed: {text}\nVERDICT: PASS"),
            format!("Reviewed:\n{text}\nVERDICT: PASS"),
            format!("  {text}\nVERDICT: PASS"),
            format!("{text} VERDICT: PASS"),
            format!("{text}\n{text}"),
            format!("{text}\n[1]"),
            format!("{text}\nAlso:\n{{\"head\": \"1234567\"}}"),
            format!("{text}\nIt returns {{}} for no input."),
            format!("{text}\n~~~\nexample\n~~~"),
            "{\"verdict\": \"pass\"\nVERDICT: PASS".to_owned(),
        ] {
            assert_eq!(summary_json(&summary), None, "{summary:?}");
        }
        // A fenced block below it is read as before, never shadowed by the object.
        let fail = json!({"verdict": "fail"});
        let fenced = format!("{text}\n```json\n{fail}\n```\nVERDICT: PASS");
        assert_eq!(summary_json(&fenced), Some(fail.clone()));
        assert_eq!(
            summary_json(&format!("{{draft\n```\n{fail}\n```")),
            Some(fail)
        );
    }

    #[test]
    fn text_after_a_leading_json_pass_takes_it_back_as_after_a_verdict_line() {
        let text = json!({"verdict": "pass", "head": "abcdef1"}).to_string();
        for kept in [
            format!("{text}\n\nChecks: 9 tests passed.\nVERDICT: PASS"),
            format!("{text}\nhead: abcdef1\nVERDICT: PASS"),
        ] {
            assert_eq!(Verdict::extract(&kept, None), Verdict::Pass, "{kept}");
        }
        assert!(!Verdict::json_taken_back(&format!(
            "{text}\n\nNo findings."
        )));
        // JSON that states no verdict is evidence: prose after it is not held.
        let report = json!({"tests": 12}).to_string();
        let explained = format!("{report}\nOnly docs changed.\nhead: abcdef1\nVERDICT: PASS");
        assert_eq!(Verdict::extract(&explained, None), Verdict::Pass);
        for held in [
            format!("{text}\n\nChanges needed in the parser."),
            format!("{text}\n\nThe build is still broken.\nVERDICT: PASS"),
            format!("{text}\nVERDICT: FAIL\nVERDICT: PASS"),
            format!("{text}\nVERDICT: PASS\nDo not merge yet."),
            format!("{text}\nAll good. VERDICT: PASS"),
        ] {
            assert!(Verdict::json_taken_back(&held), "{held}");
        }
        // A stage verdict is its line or `structured`, never that JSON: prose before the line stays explanation.
        let stage = format!("{text}\nAll tests passed after the fix.\nVERDICT: PASS");
        assert!(Verdict::json_taken_back(&stage));
        assert_eq!(Verdict::extract(&stage, None), Verdict::Pass);
        let structured = json!({"verdict": "pass", "head": "abcdef1"});
        let held = format!("{text}\n\nChanges needed in the parser.");
        assert_eq!(Verdict::extract(&held, Some(&structured)), Verdict::Pass);
    }

    #[test]
    fn verdict_prefers_structured_then_summary_line() {
        let structured = serde_json::json!({"verdict": "fail"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&structured)),
            Verdict::Fail
        );
        assert_eq!(
            Verdict::extract("looks good\n\nVERDICT: PASS\n", None),
            Verdict::Pass
        );
        assert_eq!(Verdict::extract("no verdict here", None), Verdict::Unknown);
    }

    #[test]
    fn a_whole_pass_with_follow_ups_field_is_a_pass_unless_it_lists_a_blocker() {
        let word = "approve_with_nonblocking_finding";
        let plain = serde_json::json!({"verdict": word, "findings": [{"blocking": false}]});
        assert_eq!(Verdict::from_structured(&plain), Some(Verdict::Pass));
        let blocked = serde_json::json!({"verdict": word, "findings": [{"blocking": true}]});
        assert_eq!(Verdict::from_structured(&blocked), Some(Verdict::Unknown));
        let severe = serde_json::json!({"verdict": word, "findings": [{"severity": "high"}]});
        assert_eq!(Verdict::from_structured(&severe), Some(Verdict::Unknown));
        for other in [
            "approve_with_nonblocking_finding, pending CI",
            "approve_with_blocking_finding",
        ] {
            let given = serde_json::json!({"verdict": other});
            assert_eq!(
                Verdict::from_structured(&given),
                Some(Verdict::Unknown),
                "{other}"
            );
        }
        assert_eq!(
            Verdict::extract(&format!("VERDICT: {word}"), None),
            Verdict::Unknown
        );
    }

    #[test]
    fn verdict_reads_markdown_any_case_synonyms_and_other_structured_keys() {
        let findings_only = serde_json::json!({"findings": []});
        let passed = [
            ("**Verdict:** PASS — all 214 tests pass", None),
            ("tests ran\n\n**VERDICT: PASS**", None),
            ("Verdict: passed\nhead: abc1234", None),
            ("## verdict: Approved", None),
            (
                "**Verdict:** PASS — all 214 tests pass",
                Some(&findings_only),
            ),
        ];
        for (summary, structured) in passed {
            assert_eq!(
                Verdict::extract(summary, structured),
                Verdict::Pass,
                "{summary}"
            );
        }
        let in_result = serde_json::json!({"result": "**Passed**"});
        assert_eq!(Verdict::extract("", Some(&in_result)), Verdict::Pass);
        let in_status = serde_json::json!({"result": "done", "status": "FAILED"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&in_status)),
            Verdict::Fail
        );
        let rejected = serde_json::json!({"verdict": "Rejected: two findings"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&rejected)),
            Verdict::Fail
        );
    }

    #[test]
    fn each_word_of_the_vocabulary_reads_as_it_says_in_any_spelling() {
        let read = |word: &str| {
            let structured = Verdict::extract("", Some(&json!({"verdict": word})));
            let line = Verdict::extract(&format!("VERDICT: {word}"), None);
            assert_eq!(structured, line, "{word}: both paths read alike");
            structured
        };
        for (words, verdict) in [
            (&PASS_WORDS[..], Verdict::Pass),
            (&FAIL_WORDS[..], Verdict::Fail),
        ] {
            for word in words {
                for spelt in [
                    word.to_string(),
                    word.to_uppercase(),
                    word.replace(' ', "_"),
                ] {
                    assert_eq!(read(&spelt), verdict, "{spelt}");
                    assert_eq!(Verdict::of_phrase(&spelt), Some(verdict), "{spelt}");
                }
                assert_eq!(read(&format!("{}: why", word.replace(' ', "-"))), verdict);
            }
        }
        for word in [
            "Changes_Requested",
            "request-changes",
            "changes  requested",
            "Needs_Work",
        ] {
            assert_eq!(read(word), Verdict::Fail, "{word}");
        }
        for word in [
            "changes",
            "requested",
            "needs",
            "maybe",
            "pass or fail",
            "block-level",
            "okay",
            "passes",
        ] {
            assert_eq!(read(word), Verdict::Unknown, "{word}");
            assert_eq!(Verdict::of_phrase(word), None, "{word}");
        }
    }

    /// A fail word beside a blocking finding is a fail, never an unreadable pass.
    #[test]
    fn a_fail_word_beside_a_blocking_finding_reads_as_a_fail() {
        let blocking = json!([{"title": "data loss", "blocking": true}]);
        let said = json!({"verdict": "changes_requested", "findings": blocking});
        assert_eq!(Verdict::extract("", Some(&said)), Verdict::Fail);
        assert_eq!(Verdict::as_worded(&said), Some(Verdict::Fail));
        let passed = json!({"verdict": "approved", "findings": blocking});
        assert_eq!(Verdict::from_structured(&passed), Some(Verdict::Unknown));
        assert_eq!(Verdict::as_worded(&passed), Some(Verdict::Pass));
    }

    #[test]
    fn verdict_stays_unknown_without_a_readable_word_and_quotes_what_was_said() {
        let unclear = serde_json::json!({"verdict": "maybe"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&unclear)),
            Verdict::Unknown
        );
        assert_eq!(
            Verdict::stated("", Some(&unclear)).as_deref(),
            Some("maybe")
        );
        for malformed in [
            json!({"verdict": ""}),
            json!({"verdict": null}),
            json!({"verdict": true}),
        ] {
            let read = Verdict::extract("VERDICT: PASS", Some(&malformed));
            assert_eq!(read, Verdict::Unknown, "{malformed}");
        }
        let blank = json!({"verdict": " ", "status": "pass"});
        assert_eq!(Verdict::extract("", Some(&blank)), Verdict::Unknown);
        assert_eq!(
            Verdict::stated("", Some(&json!({"verdict": null}))).as_deref(),
            Some("null")
        );
        let prose = serde_json::json!({"result": "passing most tests, two fail"});
        assert_eq!(Verdict::extract("", Some(&prose)), Verdict::Unknown);
        assert_eq!(
            Verdict::extract("Not passed: VERDICT: PASS", None),
            Verdict::Unknown
        );
        assert_eq!(
            Verdict::extract("VERDICT: not a pass", None),
            Verdict::Unknown
        );
        assert_eq!(
            Verdict::stated("work done\nVERDICT: maybe\n", None).as_deref(),
            Some("VERDICT: maybe")
        );
        assert_eq!(Verdict::stated("no verdict at all", None), None);
    }

    #[test]
    fn only_the_last_verdict_line_outside_quotes_and_code_counts() {
        let quoted = "Example:\n> VERDICT: PASS\n\nVERDICT: not passed";
        assert_eq!(Verdict::extract(quoted, None), Verdict::Unknown);
        let fenced = "```\nVERDICT: PASS\n```\nThree findings remain.";
        assert_eq!(Verdict::extract(fenced, None), Verdict::Unknown);
        let howto = "VERDICT: FAIL\n- `VERDICT: PASS` or `VERDICT: FAIL`";
        assert_eq!(Verdict::extract(howto, None), Verdict::Unknown);
        let earlier = "VERDICT: PASS\nthen a regression\nVERDICT: FAIL";
        assert_eq!(Verdict::extract(earlier, None), Verdict::Fail);
        let after_quote = "> VERDICT: FAIL\n\nVERDICT: PASS";
        assert_eq!(Verdict::extract(after_quote, None), Verdict::Pass);
        for lazy in [
            "Blocking issue.\n> Example of a passing result:\nVERDICT: PASS",
            "Blocking issue.\n   > Example of a passing result:\nVERDICT: PASS",
        ] {
            assert_eq!(Verdict::extract(lazy, None), Verdict::Unknown, "{lazy:?}");
        }
        let listed_after_quote = "> a note\n- **Verdict:** PASS";
        assert_eq!(Verdict::extract(listed_after_quote, None), Verdict::Pass);
        for quoted in [
            "> example\n    - test\nVERDICT: PASS",
            "> example\n    # not a heading here\nVERDICT: PASS",
            "> example\n#not-a-heading\nVERDICT: PASS",
            "> example\n2. not a first item\nVERDICT: PASS",
            "> - a quoted item\nVERDICT: PASS",
            "> > nested\nVERDICT: PASS",
        ] {
            assert_eq!(
                Verdict::extract(quoted, None),
                Verdict::Unknown,
                "{quoted:?}"
            );
        }
        for top in [
            "> example\n1. tests pass\nVERDICT: PASS",
            "> # heading\nVERDICT: PASS",
            "> example\n***\nVERDICT: PASS",
            "> - - -\nVERDICT: PASS",
            "> -\nVERDICT: PASS",
            "> > 1.\nVERDICT: PASS",
            "> example\n## Verdict: PASS",
            "> a\n>\nVERDICT: PASS",
            ">     code\nVERDICT: PASS",
            "> ```\n> code\n> ```\nVERDICT: PASS",
        ] {
            assert_eq!(Verdict::extract(top, None), Verdict::Pass, "{top:?}");
        }
        let mixed = "````\n~~~\nVERDICT: PASS\n```\n````\nFindings remain.";
        assert_eq!(Verdict::extract(mixed, None), Verdict::Unknown);
        let closed = "~~~\nVERDICT: FAIL\n~~~~\nVERDICT: PASS";
        assert_eq!(Verdict::extract(closed, None), Verdict::Pass);
        let content = "```text\n```not-a-close\nVERDICT: PASS\n```";
        assert_eq!(Verdict::extract(content, None), Verdict::Unknown);
        let indented = "Example:\n\n    VERDICT: PASS\n\nFindings remain.";
        assert_eq!(Verdict::extract(indented, None), Verdict::Unknown);
        let indented_marker = "```\n    ```\nVERDICT: PASS\n```";
        assert_eq!(Verdict::extract(indented_marker, None), Verdict::Unknown);
        for code in [
            " \tVERDICT: PASS",
            "  \tVERDICT: PASS",
            "   \tVERDICT: PASS",
        ] {
            assert_eq!(Verdict::extract(code, None), Verdict::Unknown, "{code:?}");
        }
        let mixed_marker = "```\n \t```\nVERDICT: PASS\n```";
        assert_eq!(Verdict::extract(mixed_marker, None), Verdict::Unknown);
        let nested = "Blocking issue.\n- Example of a pass:\n  VERDICT: PASS";
        assert_eq!(Verdict::extract(nested, None), Verdict::Unknown);
        let listed = "Checked.\n- **Verdict:** PASS";
        assert_eq!(Verdict::extract(listed, None), Verdict::Pass);
        let explained = "VERDICT: PASS — all 214 tests pass";
        assert_eq!(Verdict::extract(explained, None), Verdict::Pass);
    }

    #[test]
    fn a_verdict_statement_reads_with_text_after_it_and_markdown_around_it() {
        let pass = json!({"verdict": "pass", "head": "abcdef1", "findings": []});
        for summary in [
            "Verdict: pass. No blocking findings; the parser change is covered.",
            "**VERDICT: PASS**",
            "**verdict: pass.** All checks green.",
            "Verdict: pass. No critical bugs, no blocking findings.",
            "- Verdict: Pass! Nothing to fix.",
            "Verdict: pass. Fixes the reported bug in the code block parser.",
            "Verdict: pass. No blocking findings…\n\nThe change is small; one non-blocking note.",
            "Verdict: fail — a regression.\nFixed in the next commit.\nVerdict: pass.",
        ] {
            assert_eq!(Verdict::extract(summary, None), Verdict::Pass, "{summary}");
            assert_eq!(
                Verdict::extract(summary, Some(&pass)),
                Verdict::Pass,
                "{summary}"
            );
        }
        for summary in [
            "- verdict: fail — two blocking findings in the parser",
            "Verdict: pass.\nVerdict: FAIL. A regression remains.",
        ] {
            assert_eq!(Verdict::extract(summary, None), Verdict::Fail, "{summary}");
            assert_eq!(Verdict::extract(summary, Some(&pass)), Verdict::Unknown);
        }
        let unsaid = "No verdict, but the tests pass.";
        assert_eq!(Verdict::extract(unsaid, None), Verdict::Unknown);
        assert_eq!(Verdict::extract(unsaid, Some(&pass)), Verdict::Pass);
        for summary in [
            "Verdict: pass or fail, depending on CI",
            "Verdict: pass. Two tests failed.",
            "Verdict: pass. One blocker remains in run.rs.",
            "Verdict: pass. Don't merge yet.",
            "Verdict: pass. Changes requested.",
            "Verdict: pass. A critical bug remains.",
            "Verdict: pass. Needs a fix; unsafe to merge.",
            "Verdict: pass.\n\nDo not merge until CI is green.",
        ] {
            assert_eq!(
                Verdict::extract(summary, None),
                Verdict::Unknown,
                "{summary}"
            );
            assert_ne!(Verdict::extract(summary, Some(&pass)), Verdict::Pass);
        }
        let blocked = json!({"verdict": "pass", "findings": [{"blocking": true}]});
        let summary = "Verdict: pass. No blocking findings.";
        assert_eq!(Verdict::extract(summary, Some(&blocked)), Verdict::Unknown);
        assert!(Verdict::pass_disputed(summary, Some(&blocked)));
        assert!(!Verdict::pass_disputed(summary, Some(&pass)));
    }

    #[test]
    fn a_pass_followed_by_a_reported_problem_is_held() {
        let pass = json!({"verdict": "pass", "head": "abcdef1", "findings": []});
        for summary in [
            "Verdict: pass. Changes needed in run.rs",
            "Verdict: pass. Changes are needed in run.rs",
            "Verdict: pass. Further work needed",
            "Verdict: pass. Issues found: the parser drops a line",
            "Verdict: pass. Two new bugs were found in review.rs",
            "Verdict: pass. Found 3 problems in the gate",
            "Verdict: pass. Errors remain in the reader",
            "Verdict: pass. Still broken on Windows",
            "Verdict: pass. The gate is still failing",
            "Verdict: pass. CI is red",
            "Verdict: pass, but tests fail",
            "Verdict: pass.\nChanges needed in X",
            "Verdict: pass.\n\nSummary: issues found in the reader.",
            "Verdict: pass. No blockers. Issues remain in run.rs",
            "Verdict: pass. No issues found. Changes needed in X",
            "Verdict: pass. No blockers and issues remain in run.rs",
            "Verdict: pass. No blockers and changes are needed in run.rs",
            "Verdict: pass. No regressions and bugs were found in the reader",
            "Verdict: pass. No blocking findings and the build is still broken",
            "Verdict: pass. Changes will be needed in X",
            "Verdict: pass. Changes would still be needed in X",
            "Verdict: pass. Needed changes remain",
            "Verdict: pass. Issues still open",
            "Verdict: pass. Bug is still there",
            "Verdict: pass. Issues remain to be fixed",
            "Verdict: pass. Changes needed to be addressed",
            "Verdict: pass. Bugs found. None have been fixed",
            "Verdict: pass. Issues found, none fixed",
            "Verdict: pass. Bugs found. Nothing was fixed",
            "Verdict: pass. Lint errors found",
            "Verdict: pass. Two type errors found",
            "Verdict: pass. Remaining issues: the parser drops a line",
            "Verdict: pass. Open issues: the gate crashes",
            "Verdict: pass. The gate still crashes",
            "Verdict: pass. Changes are necessary",
            "Verdict: pass. No issues found, bugs remain",
            "Verdict: pass. No issues, changes needed in X",
            "Verdict: pass. No issues, 3 bugs found",
            "Verdict: pass. No issues, a bug remains",
            "Verdict: pass. No issues and tests are still broken",
            "Verdict: pass. Nothing is wrong but the build is broken",
            "Verdict: pass. None of the changes are needed and the tests are broken",
            "Verdict: pass. Nothing is broken but the gate is still broken",
            "Verdict: pass. None of the tests, and the build, are still broken",
            "Verdict: pass. None of the tests pass and the build is still broken",
            "Verdict: pass. No blockers, several bugs found",
            "Verdict: pass. No blockers, minor issues remain",
            "Verdict: pass. Zero blockers, three issues remain",
            "Verdict: pass.\nNo issues.\nBugs found in run.rs",
            "Verdict: pass.\n\n### Issues found\nThe parser drops a line",
        ] {
            assert_eq!(
                Verdict::extract(summary, None),
                Verdict::Unknown,
                "{summary}"
            );
            assert_ne!(Verdict::extract(summary, Some(&pass)), Verdict::Pass);
        }
        for field in ["pass. Changes needed in X", "pass: issues found in X"] {
            let given = json!({"verdict": field});
            assert_eq!(
                Verdict::extract("", Some(&given)),
                Verdict::Unknown,
                "{field}"
            );
        }
    }

    #[test]
    fn a_pass_denying_a_problem_stays_a_pass() {
        let pass = json!({"verdict": "pass", "head": "abcdef1", "findings": []});
        for summary in [
            "Verdict: pass. No issues found.",
            "Verdict: pass. Nothing is broken.",
            "Verdict: pass. Non-blocking issues noted.",
            "Verdict: pass. Fixes the bug found in review.",
            "Verdict: pass. Fixes the broken parser.",
            "Verdict: pass. Fixes bugs found by fuzzing.",
            "Verdict: pass. Nothing is still broken.",
            "Verdict: pass. None of the tests are still broken.",
            "Verdict: pass. Is none of the changes still broken",
            "Verdict: pass. Zero tests still broken.",
            "Verdict: pass. Neither of the bugs remains",
            "Verdict: pass. Neither of the tests is still broken",
            "Verdict: pass. Neither bug was found",
            "Verdict: pass. Nothing in the tests are still broken.",
            "Verdict: pass. None of the tests or the build are still broken.",
            "Verdict: pass. None of the build is broken.",
            "Verdict: pass. None of the parser is still broken.",
            "Verdict: pass. None of the code is still broken.",
            "Verdict: pass. Nothing in the gate is still broken.",
            "Verdict: pass. Without any checks still broken.",
            "Verdict: pass. Found no issues.",
            "Verdict: pass. No problems remain.",
            "Verdict: pass. Issues found: none.",
            "Verdict: pass. Issues found: 0",
            "Verdict: pass. Bugs found: 0",
            "Verdict: pass. Problems found: zero",
            "Verdict: pass. Issues remaining: none",
            "Verdict: pass. Changes needed: none",
            "Verdict: pass. Changes needed (none)",
            "Verdict: pass. Changes needed — none",
            "Verdict: pass. Issues found - none",
            "Verdict: pass. Issues were found and fixed.",
            "Verdict: pass. 0 issues found",
            "Verdict: pass. Found 0 issues",
            "Verdict: pass. 0 issues remain",
            "Verdict: pass. Problems found: n/a",
            "Verdict: pass. Remaining issues: none",
            "Verdict: pass. Open issues: 0",
            "Verdict: pass. No issues or bugs found",
            "Verdict: pass. No critical bugs or issues found",
            "Verdict: pass. No blockers or issues remain",
            "Verdict: pass. No issues, problems or bugs found",
            "Verdict: pass. Zero issues or bugs found",
            "Verdict: pass. No issues, bugs found",
            "Verdict: pass. No issues, bugs, or problems found",
            "Verdict: pass. No blockers, bugs, or errors remain",
            "Verdict: pass. Without bugs, issues found",
            "Verdict: pass. None of the bugs remain",
            "Verdict: pass. None of the changes are needed",
            "Verdict: pass. Without any issues remaining",
            "Verdict: pass. Without any changes needed",
            "Verdict: pass.\n\n### Issues found\nNone",
            "Verdict: pass.\nIssues found:\n- none",
            "Verdict: pass.\nChanges needed:\n(none)",
            "Verdict: pass.\nIssues found: none\nChanges needed: none",
        ] {
            assert_eq!(Verdict::extract(summary, None), Verdict::Pass, "{summary}");
            assert_eq!(
                Verdict::extract(summary, Some(&pass)),
                Verdict::Pass,
                "{summary}"
            );
        }
    }

    #[test]
    fn the_negated_forms_the_old_reader_refused_stay_refused() {
        for summary in [
            "Verdict: pass. No changes needed.",
            "Verdict: pass. No failing tests.",
            "Verdict: pass. No new issues found.",
        ] {
            assert_eq!(
                Verdict::extract(summary, None),
                Verdict::Unknown,
                "{summary}"
            );
        }
    }

    #[test]
    fn markdown_lines_tell_prose_from_quotes_code_and_fences() {
        use LineKind::*;
        let text = "a\n> b\n \tc\n~~~json\n    d\n```\n~~~~\n- e\n  f\n  ```\n  g\n  ```\n```\nh";
        let kinds: Vec<_> = markdown_lines(text).map(|(kind, _)| kind).collect();
        let want = [
            Prose,
            Quote,
            Indented,
            FenceOpen,
            Fenced,
            Fenced,
            FenceClose,
            Prose,
            Indented,
            NestedFenceOpen,
            Fenced,
            FenceClose,
            FenceOpen,
            Fenced,
        ];
        assert_eq!(kinds, want);
        let lazy = "> a\nb\n  c\nd\n\ne\n> f\n# g\n> h\n```\ni";
        let kinds: Vec<_> = markdown_lines(lazy).map(|(kind, _)| kind).collect();
        let want = [
            Quote, Quote, Indented, Quote, Prose, Prose, Quote, Prose, Quote, FenceOpen, Fenced,
        ];
        assert_eq!(kinds, want);
    }

    #[test]
    fn a_choice_of_verdicts_is_no_verdict() {
        for said in [
            "PASS or FAIL",
            "pass/fail",
            "**PASS** | FAIL",
            "fail OR pass",
        ] {
            let line = format!("VERDICT: {said}");
            assert_eq!(Verdict::extract(&line, None), Verdict::Unknown, "{said}");
            let field = json!({"verdict": said});
            assert_eq!(
                Verdict::extract("", Some(&field)),
                Verdict::Unknown,
                "{said}"
            );
        }
        for said in [
            "PASS, or FAIL",
            "PASS (or FAIL)",
            "PASS, but two tests failed",
        ] {
            let line = format!("VERDICT: {said}");
            assert_eq!(Verdict::extract(&line, None), Verdict::Unknown, "{said}");
        }
        let explained = "**Verdict:** PASS — 214 tests passed, 0 issues, CI green";
        assert_eq!(Verdict::extract(explained, None), Verdict::Pass);
        for said in [
            "PASS, pending fixes",
            "passed with two open nits",
            "PASS if CI agrees",
            "PASS (pending fixes)",
            "PASS (two advisory notes): nothing failed",
        ] {
            let line = format!("VERDICT: {said}");
            assert_eq!(Verdict::extract(&line, None), Verdict::Unknown, "{said}");
            let field = json!({"verdict": said});
            let read = Verdict::extract("VERDICT: PASS", Some(&field));
            assert_eq!(read, Verdict::Unknown, "{said}");
        }
        let held = [
            "PASS — pending fixes",
            "PASS: if CI passes",
            "PASS — do not merge until the blocking issue is fixed",
            "PASS — don't ship yet",
            "PASS. One blocker remains",
            "PASS — CI must pass before merge",
            "PASS — changes required before merge",
            "PASS — merge after CI is green",
            "PASS — fix the failing tests first",
            "PASS — two tests failed",
            "PASS — merge only with green CI",
            "PASS — verdict confirmed by CI",
        ];
        for said in held {
            let line = format!("VERDICT: {said}");
            assert_eq!(Verdict::extract(&line, None), Verdict::Unknown, "{said}");
            for key in ["verdict", "result", "status"] {
                let field = json!({key: said});
                let read = Verdict::extract("VERDICT: PASS", Some(&field));
                assert_eq!(read, Verdict::Unknown, "{key}: {said}");
            }
        }
        let pass = json!({"verdict": "pass"});
        for (summary, want) in [
            ("Done.", Verdict::Pass),
            ("VERDICT: PASS", Verdict::Pass),
            (
                "VERDICT: PASS — CI must pass before merge",
                Verdict::Unknown,
            ),
            ("VERDICT: FAIL", Verdict::Unknown),
        ] {
            assert_eq!(Verdict::extract(summary, Some(&pass)), want, "{summary}");
        }
        let fail = json!({"verdict": "fail"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&fail)),
            Verdict::Fail
        );
        let said = Verdict::stated("VERDICT: FAIL", Some(&pass));
        assert_eq!(said.as_deref(), Some("VERDICT: FAIL"));
        let failed = json!({"verdict": "fail", "findings": [{"title": "defect"}]});
        for summary in [failed.to_string(), format!("```json\n{failed}\n```")] {
            assert_eq!(Verdict::extract(&summary, Some(&pass)), Verdict::Unknown);
            let said = Verdict::stated(&summary, Some(&pass));
            assert_eq!(said.as_deref(), Some("fail"), "{summary}");
        }
        for severity in ["blocking", "High", "critical"] {
            let listed = json!({"verdict": "pass", "findings": [{"severity": severity}]});
            assert_eq!(
                Verdict::extract("", Some(&listed)),
                Verdict::Unknown,
                "{severity}"
            );
        }
        let advisory = json!({"verdict": "pass", "findings": [{"severity": "advisory"}]});
        assert_eq!(Verdict::extract("", Some(&advisory)), Verdict::Pass);
        let marked = json!({"verdict": "pass", "findings": [{"blocking": true}]});
        assert_eq!(Verdict::extract("", Some(&marked)), Verdict::Unknown);
        let follow_up = json!({"verdict": "pass", "findings": [{"blocking": false}]});
        assert_eq!(Verdict::extract("", Some(&follow_up)), Verdict::Pass);
        let failing = json!({"verdict": "fail", "findings": [{"severity": "blocking"}]});
        assert_eq!(Verdict::extract("", Some(&failing)), Verdict::Fail);
        let evidence = json!({"head": "abcdef1", "findings": [{"severity": "blocking"}]});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&evidence)),
            Verdict::Unknown
        );
        let blocked = json!({"verdict": "pass", "findings": [{"severity": "high"}]}).to_string();
        assert_eq!(Verdict::extract(&blocked, Some(&pass)), Verdict::Unknown);
        for key in ["result", "status"] {
            let failed = json!({key: "fail"}).to_string();
            assert_eq!(
                Verdict::extract(&failed, Some(&pass)),
                Verdict::Unknown,
                "{key}"
            );
            let said = Verdict::stated(&failed, Some(&pass));
            assert_eq!(said.as_deref(), Some("fail"), "{key}");
        }
        let structured = json!({"verdict": "pass", "head": "abcdef1"});
        let wrapped = json!({"summary": "VERDICT: FAIL", "structured": structured}).to_string();
        for summary in [wrapped.clone(), format!("```json\n{wrapped}\n```")] {
            assert_eq!(Verdict::extract(&summary, Some(&pass)), Verdict::Unknown);
        }
        let agreed = json!({"summary": "Done.\nVERDICT: PASS", "structured": structured});
        assert_eq!(
            Verdict::extract(&agreed.to_string(), Some(&pass)),
            Verdict::Pass
        );
        let report = json!({"tests": 214}).to_string();
        assert_eq!(Verdict::extract(&report, Some(&pass)), Verdict::Pass);
        let later = json!({"verdict": "PASS\nDo not merge until CI is green"});
        assert_eq!(Verdict::extract("", Some(&later)), Verdict::Unknown);
        let continued = "Reviewed.\n\nVERDICT: PASS\nDo not merge until CI is green";
        assert_eq!(Verdict::extract(continued, None), Verdict::Unknown);
        let separate = "VERDICT: PASS\n\nDo not merge until CI is green";
        assert_eq!(Verdict::extract(separate, None), Verdict::Unknown);
        let anchored = "VERDICT: PASS\nhead: abc123\nbase: `def456`";
        assert_eq!(Verdict::extract(anchored, None), Verdict::Pass);
        let smuggled = "VERDICT: PASS\nhead: abc123 — do not merge";
        assert_eq!(Verdict::extract(smuggled, None), Verdict::Unknown);
        for (said, want) in [
            ("PASS — no findings", Verdict::Pass),
            ("PASS — all checks green", Verdict::Pass),
            ("PASS — 214 passed, 0 failed", Verdict::Unknown),
            ("PASS — no blocking findings", Verdict::Pass),
            ("PASS — one blocking finding", Verdict::Unknown),
            ("PASS — no tests", Verdict::Unknown),
            ("PASS — no CI", Verdict::Unknown),
            (
                "FAIL — do not merge until the blocker is fixed",
                Verdict::Fail,
            ),
        ] {
            let line = format!("VERDICT: {said}");
            assert_eq!(Verdict::extract(&line, None), want, "{said}");
        }
        let multiline = json!({"verdict": "PASS\nall 214 tests pass"});
        assert_eq!(Verdict::extract("", Some(&multiline)), Verdict::Pass);
        for result in [
            "FAIL — tests failed",
            "FAIL or PASS",
            "FAIL — tests failed\nVERDICT: PASS",
        ] {
            for key in ["result", "status"] {
                let field = json!({key: result});
                let read = Verdict::extract("VERDICT: PASS", Some(&field));
                assert_ne!(read, Verdict::Pass, "{key}: {result}");
            }
        }
        for key in ["result", "status"] {
            let example = json!({key: "Example:\nVERDICT: PASS"});
            let read = Verdict::extract("VERDICT: FAIL", Some(&example));
            assert_eq!(read, Verdict::Fail, "{key}");
            let labelled = json!({key: "**VERDICT:** FAIL — two findings"});
            let read = Verdict::extract("VERDICT: PASS", Some(&labelled));
            assert_eq!(read, Verdict::Fail, "{key}");
            let unreadable = json!({key: "VERDICT: not passed"});
            let read = Verdict::extract("VERDICT: PASS", Some(&unreadable));
            assert_eq!(read, Verdict::Unknown, "{key}");
            let said = Verdict::stated("VERDICT: PASS", Some(&unreadable));
            assert_eq!(said.as_deref(), Some("VERDICT: not passed"), "{key}");
        }
        let choice = json!({"status": "PASS or FAIL"});
        let said = Verdict::stated("VERDICT: PASS", Some(&choice));
        assert_eq!(said.as_deref(), Some("PASS or FAIL"));
        let skipped = json!({"result": "done"});
        let said = Verdict::stated("VERDICT: unsure", Some(&skipped));
        assert_eq!(said.as_deref(), Some("VERDICT: unsure"));
        assert_eq!(Verdict::stated("", Some(&skipped)).as_deref(), Some("done"));
    }

    #[test]
    fn the_blocking_mark_decides_and_severity_only_without_one() {
        assert!(Verdict::is_follow_up(
            &json!({"blocking": false, "severity": "low"})
        ));
        let high_follow_up = json!({"blocking": false, "severity": "high"});
        assert!(Verdict::is_follow_up(&high_follow_up));
        assert!(
            !Verdict::blocks(&high_follow_up),
            "the mark outranks severity"
        );
        assert!(Verdict::blocks(&json!({"severity": "high"})));
        assert!(!Verdict::is_follow_up(&json!({"severity": "low"})));
        assert!(!Verdict::is_follow_up(&json!({"blocking": "no"})));
        assert!(Verdict::blocks(
            &json!({"blocking": true, "severity": "low"})
        ));
    }

    #[test]
    fn blockers_and_escalations_end_a_run_and_other_failures_do_not() {
        assert!(StageOutcome::ends_run("  BLOCKED: no push access"));
        assert!(StageOutcome::ends_run(
            "ESCALATE: needs a review; use adaptive"
        ));
        assert!(!StageOutcome::ends_run(
            "tests failed: ESCALATE: is not a prefix here"
        ));
        let escalated = StageOutcome::Failed {
            error: "ESCALATE: too large".into(),
            infra: false,
        };
        assert!(
            !escalated.is_operator_blocker(),
            "the operator is not asked"
        );
    }

    #[test]
    fn gate_answers_read_as_approve_reject_or_feedback() {
        assert_eq!(
            GateDecision::from_answer("yes").unwrap(),
            GateDecision::Approve { note: None }
        );
        assert_eq!(
            GateDecision::from_answer("Approve, but keep the old flag").unwrap(),
            GateDecision::Approve {
                note: Some("but keep the old flag".into())
            }
        );
        assert!(matches!(
            GateDecision::from_answer("no").unwrap(),
            GateDecision::Reject { .. }
        ));
        assert_eq!(
            GateDecision::from_answer("reject: too risky").unwrap(),
            GateDecision::Reject {
                reason: "too risky".into()
            }
        );
        assert!(matches!(
            GateDecision::from_answer("no, split it in two").unwrap(),
            GateDecision::Revise { .. }
        ));
        assert_eq!(
            GateDecision::from_answer("use a trait instead").unwrap(),
            GateDecision::Revise {
                feedback: "use a trait instead".into()
            }
        );
        assert!(GateDecision::from_answer("  ").is_err());
    }

    #[test]
    fn an_awaiting_run_is_live() {
        let waiting = RunState::AwaitingOperator {
            stage_id: "propose".into(),
            attempt: 1,
            subject: GateSubject::Proposal,
        };
        assert!(!waiting.is_terminal());
        assert_eq!(waiting.live_stage(), Some("propose"));
        let done = RunState::Succeeded {
            summary: "ok".into(),
        };
        assert_eq!(done.live_stage(), None);
    }

    #[test]
    fn a_cost_stop_is_read_only_as_aether_and_archon_write_it() {
        let label = StageOutcome::NOTIONAL_COST_LABEL;
        assert_eq!(
            label,
            " (notional cost: Claude Code reports list-price dollars whether or not its login bills them, and Aether showed no fresh subscription usage window with headroom for it, so the budget was enforced; under a subscription, raise or unset the item's budget)",
            "the label 1.6.3 stored"
        );
        let reason = "cost budget of $2: $2.0100 spent";
        let message = format!(
            "Stopped by its budget ({reason}); its live subtree was cancelled with it, 3 \
             descendant(s) were live at the stop. Details are in the exhaustion.failed event."
        );
        for stop in [reason.to_owned(), message.clone()] {
            assert!(StageOutcome::is_cost_budget_stop(&stop), "{stop}");
            let labelled = format!("{stop}{label}");
            assert!(StageOutcome::is_cost_budget_stop(&labelled), "{labelled}");
            for inexact in [
                format!(" {stop}"),
                format!("{stop} "),
                format!("{stop} (notional cost: as I see it)"),
                format!("{stop}{label} and more"),
                format!("{stop}{label}{label}"),
            ] {
                assert!(!StageOutcome::is_cost_budget_stop(&inexact), "{inexact}");
            }
        }
        for limit in ["1e3", "+5", "inf", "NaN", ".5", "5.", "", "5 or so"] {
            let odd = format!("cost budget of ${limit}: $9.0000 spent");
            assert!(!StageOutcome::is_cost_budget_stop(&odd), "{odd}");
        }
    }

    /// Aether writes a limit with `format_usd` (six decimals, zeros trimmed, or
    /// the shortest exact form below that) and a spend with `{:.4}`.
    #[test]
    fn a_cost_stop_holds_only_amounts_aether_can_write() {
        let stop = |limit: &str, cost: &str| format!("cost budget of ${limit}: {cost} spent");
        for limit in [
            "5",
            "120",
            "0.3",
            "2.5",
            "0.000001",
            "0.0000004",
            "0.0000001",
        ] {
            let said = stop(limit, "$120.0000");
            assert!(StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
        // Aether refuses a zero budget, and writes each of the others another way.
        let unwritten = [
            "0",
            "0.0",
            "5.00",
            "2.50",
            "05",
            "00",
            "5.0",
            "1.0000001",
            "0.0000010",
            "0.0000009",
            "0.0000006",
            "0.00000010",
        ];
        for limit in unwritten {
            let said = stop(limit, "$120.0000");
            assert!(!StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
        for cost in [
            "$5.0000",
            "$12.3456",
            "≥ $5.0000 (some unpriced)",
            "≥ $9.0000 (some cost unknown)",
            "≥ $9.0000 (usage not fully reported)",
            "≥ $9.0000 (interrupted run, cost unknown)",
            "≥ $9.0000 (multiple cost gaps)",
        ] {
            let said = stop("5", cost);
            assert!(StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
        for cost in [
            "$9",
            "$5.01",
            "$5.010",
            "$5.01000",
            "$05.0100",
            "$.0100",
            // No amount is written so: a number that large keeps no such decimals.
            "$70368744177664.0001",
            "$9007199254740993.0000",
            "≥ $9 (some unpriced)",
            "≥ $9.00 (some unpriced)",
            "≥ $9.0000 (for the smoke tests)",
            "≥ $9.0000(some unpriced)",
            "≥$9.0000 (some unpriced)",
            "≥ $9.0000",
            "$9.0000 (some unpriced)",
        ] {
            let said = stop("5", cost);
            assert!(!StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
    }

    /// Aether stops an agent at `spend >= limit`, and its budget is positive,
    /// so a stop never says less was spent, nor that no cost was reported.
    #[test]
    fn a_cost_stop_says_a_spend_that_reached_its_limit() {
        let stop = |limit: &str, cost: &str| format!("cost budget of ${limit}: {cost} spent");
        let reached = [
            ("5", "$5.0000"),
            ("5", "$10.0000"),
            ("9.5", "$10.0000"),
            // Four decimals round a spend down by up to half a unit.
            ("5.00005", "$5.0000"),
            ("4.99995", "$4.9999"),
            ("0.03125", "$0.0312"),
            ("0.0000001", "$0.0000"),
            ("0.000001", "$0.0000"),
            ("0.00005", "$0.0000"),
            ("4294967296.00045", "$4294967296.0005"),
            ("4294967296.00044", "$4294967296.0004"),
        ];
        for (limit, cost) in reached {
            let said = stop(limit, cost);
            assert!(StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
        let short = [
            ("5", "$4.9999"),
            ("5", "$0.0100"),
            ("10", "$9.0000"),
            ("5.00006", "$5.0000"),
            ("0.000051", "$0.0000"),
            // No amount written as that limit is written as so small a spend.
            ("4294967296.00045", "$4294967296.0004"),
            ("5", "≥ $1.0000 (some unpriced)"),
            ("0.5", "not reported"),
            ("0.5", "not reported (usage not fully reported)"),
            ("0.5", "not reported (some unpriced)"),
        ];
        for (limit, cost) in short {
            let said = stop(limit, cost);
            assert!(!StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
    }

    /// Every stop Aether's own formatters write for a spend at or over a
    /// budget is read as one, and none for a spend clearly short of it.
    #[test]
    fn every_cost_stop_aether_formats_is_read() {
        let limits = [
            0.000_000_1,
            0.000_000_4,
            0.000_001,
            0.031_25,
            0.1 + 0.2,
            0.5,
            1.0,
            1.005,
            2.5,
            4.999_95,
            5.000_05,
            9.999_999_49,
            10.0,
            99.999_96,
            120.0,
            1234.5678,
            1_000_000.000_001,
            4_294_967_296.000_45,
            70_368_744_177_664.5,
            9e15,
        ];
        let over = [0.0, 1e-7, 0.000_04, 0.000_05, 0.0001, 0.37, 1000.0];
        for limit in limits {
            for extra in over {
                let cost: f64 = limit + extra;
                assert!(cost >= limit, "Aether stops at the limit");
                let said = format!(
                    "cost budget of ${}: ${cost:.4} spent",
                    aether_format_usd(limit)
                );
                assert!(StageOutcome::is_cost_budget_stop(&said), "{said}");
                let floor = format!(
                    "cost budget of ${}: ≥ ${cost:.4} (multiple cost gaps) spent",
                    aether_format_usd(limit)
                );
                assert!(StageOutcome::is_cost_budget_stop(&floor), "{floor}");
            }
            // A float this large holds no thousandth: none is short by one.
            if limit < 0.001 || limit - 0.001 == limit {
                continue;
            }
            let short = format!(
                "cost budget of ${}: ${:.4} spent",
                aether_format_usd(limit),
                limit - 0.001
            );
            assert!(!StageOutcome::is_cost_budget_stop(&short), "{short}");
        }
    }

    #[test]
    fn a_budget_message_counts_its_descendants_as_aether_does() {
        let message = |count: &str| {
            format!(
                "Stopped by its budget (cost budget of $2: $2.0100 spent); no new subtree \
                 cancellation was requested, {count} descendant(s) were live at the stop. \
                 Details are in the exhaustion.failed event."
            )
        };
        for count in ["0", "3", "18446744073709551615"] {
            let said = message(count);
            assert!(StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
        for count in [
            "",
            "03",
            "007",
            "-1",
            "+3",
            "3.0",
            "18446744073709551616",
            "many",
        ] {
            let said = message(count);
            assert!(!StageOutcome::is_cost_budget_stop(&said), "{said}");
        }
    }

    #[test]
    fn a_review_field_is_one_line_of_at_most_its_cap() {
        assert_eq!(review_field("  PASS\n"), "PASS");
        assert_eq!(review_field("not\n\tpassed  yet"), "not passed yet");
        let long = format!("PASS{}x", "-".repeat(MAX_REVIEW_FIELD_CHARS - 4));
        let read = review_field(&long);
        assert_eq!(read.chars().count(), MAX_REVIEW_FIELD_CHARS);
        assert_eq!(Verdict::of_phrase(&read), Some(Verdict::Pass));
        assert_eq!(Verdict::of_phrase(&long), None, "the whole text names none");
    }

    #[test]
    fn a_review_tally_decodes_only_in_the_shape_archond_stores() {
        let read = |tally: &str| serde_json::from_str::<TalliedFails>(tally);
        let fail =
            r#"{"agent":"agt_01JB000000000000000000000R","head":"abc1234","findings":["a bug"]}"#;
        let stored = [
            (r#"{"fails":[]}"#.to_owned(), 0),
            (format!(r#"{{"fails":[{fail}]}}"#), 1),
            (format!(r#"{{"fails":[{fail}],"told":true}}"#), 1),
            (
                r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234","titles":[]}]}"#.to_owned(),
                1,
            ),
            (
                format!(
                    r#"{{"fails":[{fail}],"read":["agt_01JB000000000000000000000R","agt_01JB000000000000000000000S"],"earlier":["agt_01JB000000000000000000000Q"],
                    "other_fails":[["agt_01JB000000000000000000000S","{}"]],"every_fail_kept":true,"reread":[]}}"#,
                    "f".repeat(64)
                ),
                2,
            ),
        ];
        for (tally, reviewers) in stored {
            let read = read(&tally).unwrap_or_else(|e| panic!("{tally}: {e}"));
            assert_eq!(read.reviewers(), reviewers, "{tally}");
        }
        for unstorable in [
            r#"{}"#,
            r#"[[]]"#,
            r#"{"fails":[],"told":"yes"}"#,
            r#"{"fails":[],"told":null}"#,
            r#"{"fails":[],"round":1}"#,
            r#"{"fails":[{"agent":"agt_r","head":"abc1234","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_not an agent","head":"abc1234","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_01jb000000000000000000000r","head":"abc1234","findings":[]}]}"#,
            r#"{"fails":[{"agent":"agt_81JB000000000000000000000R","head":"abc1234","findings":[]}]}"#,
            r#"{"fails":[{"agent":"tsk_01JB000000000000000000000R","head":"abc1234","findings":[]}]}"#,
            r#"{"fails":[],"other_fails":[["agt_not an agent","abc1234"]]}"#,
            r#"{"fails":[],"read":["agt_r"]}"#,
            r#"{"fails":[],"earlier":["agt_r"]}"#,
            r#"{"fails":[],"reread":["agt_r"]}"#,
            r#"{"fails":[null]}"#,
            r#"{"fails":[{"agent":"not an agent"}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R"}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234"}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"not an agent","head":"abc1234","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":" ","head":"abc1234","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc123","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"ABC1234","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"the head","findings":["a bug"]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234","findings":"a bug"}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234","findings":[7]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234","findings":[],"titles":[]}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234","findings":[],"pass":true}]}"#,
            r#"{"fails":[{"agent":"agt_01JB000000000000000000000R","head":"abc1234","findings":null,"titles":[]}]}"#,
            r#"{"fails":[["agt_01JB000000000000000000000R","abc1234",[]]]}"#,
            r#"{"fails":[],"other_fails":[["not an agent","abc1234"]]}"#,
            r#"{"fails":[],"other_fails":[["agt_01JB000000000000000000000R","abc1234","more"]]}"#,
            r#"{"fails":[],"other_fails":[["agt_01JB000000000000000000000R","HEAD"]]}"#,
            r#"{"fails":[],"other_fails":[["agt_01JB000000000000000000000R"]]}"#,
            r#"{"fails":[],"read":"agt_01JB000000000000000000000R"}"#,
            r#"{"fails":[],"every_fail_kept":"yes"}"#,
        ] {
            assert!(read(unstorable).is_err(), "{unstorable}");
        }
        let listed = format!("[[{fail}]]");
        assert!(read(&listed).is_err(), "{listed}");
        assert!(
            read(&format!(
                r#"{{"fails":[{{"agent":"agt_01JB000000000000000000000R","head":"{}","findings":[]}}]}}"#,
                "a".repeat(65)
            ))
            .is_err()
        );
    }

    fn stage(state: StageState, structured: Option<serde_json::Value>) -> StageRun {
        StageRun {
            only_reviews: None,
            id: StageRunId::new(),
            run: PipelineRunId::new(),
            stage_id: "ship".into(),
            kind: Some(StageKind::Ship),
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            record_repair: None,
            host_context: Default::default(),
            choice: ModelChoice {
                model: "claude-code/opus".parse().unwrap(),
                effort: crate::model::Effort::Low,
                tier: None,
            },
            aether_agent: None,
            aether_task: None,
            state,
            verdict: None,
            verdict_note: None,
            sizing: Default::default(),
            restatement: None,
            summary: None,
            structured,
            cost_usd: 0.0,
            started_at: crate::now(),
            finished_at: None,
        }
    }

    #[test]
    fn the_pr_url_is_the_newest_a_completed_stage_reported() {
        let url = |n: u32| Some(serde_json::json!({"pr_url": format!("https://x/pull/{n}")}));
        let stages = [
            stage(StageState::Completed, url(1)),
            stage(StageState::Completed, url(2)),
            stage(StageState::Failed { error: "e".into() }, url(3)),
            stage(
                StageState::Completed,
                Some(serde_json::json!({"verdict": "pass"})),
            ),
        ];
        assert_eq!(pr_url(stages.iter()), Some("https://x/pull/2"));
        assert_eq!(
            pr_url(stages[2..].iter()),
            None,
            "only completed stages count"
        );
        assert_eq!(pr_url([].iter()), None);
    }

    #[test]
    fn a_review_is_refused_only_with_archons_reason_on_record() {
        let review = |review: serde_json::Value| Some(serde_json::json!({"review": review}));
        let refused = review(serde_json::json!({"verdict": "pass", REVIEW_REFUSED_KEY: "why"}));
        assert!(stage(StageState::Completed, refused).review_refused());
        let checked = review(
            serde_json::json!({"verdict": "pass", "reviewer_agents": ["agt_01JB000000000000000000000R"]}),
        );
        assert!(!stage(StageState::Completed, checked).review_refused());
        let blank = review(serde_json::json!({REVIEW_REFUSED_KEY: "  "}));
        assert!(!stage(StageState::Completed, blank).review_refused());
        assert!(!stage(StageState::Completed, None).review_refused());
    }

    #[test]
    fn a_run_finishes_once() {
        let mut run = PipelineRun {
            id: PipelineRunId::new(),
            item: QueueItemId::new(),
            pipeline: "direct".into(),
            shipped: false,
            aether_run: None,
            workspace: "/archon/worktrees/x".into(),
            team_root: None,
            branch: "archon/x".into(),
            state: RunState::Running {
                stage_id: "implement".into(),
                attempt: 1,
            },
            cost_usd: 0.0,
            started_at: crate::now(),
            finished_at: None,
        };
        let mut legacy = serde_json::to_value(&run).unwrap();
        legacy.as_object_mut().unwrap().remove("shipped");
        let legacy: PipelineRun = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.shipped, "old records cannot prove shipping");
        run.finish(
            RunState::Succeeded {
                summary: "ok".into(),
            },
            crate::now(),
        )
        .unwrap();
        assert!(
            run.finish(RunState::Cancelled { reason: None }, crate::now())
                .is_err()
        );
    }
}
