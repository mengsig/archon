//! Pipeline runs and their stage runs: the durable record of what Archon
//! asked Aether to do for one queue item and what came back.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::{AetherAgentId, AetherRunId, AetherTaskId};
use crate::catalog::StageKind;
use crate::error::ArchonError;
use crate::id::{PipelineRunId, QuestionId, QueueItemId, ReportId, StageRunId};
use crate::model::ModelChoice;

/// What a review/test stage concluded, as [`Verdict::extract`] reads it from
/// the stage agent's result; anything unreadable is `Unknown` and fails a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Whether anything in a result takes a pass back: the summary's last
    /// `VERDICT:` line or [`summary_json`] verdict reading other than pass,
    /// or a blocking finding in `structured` or that JSON.
    pub fn pass_disputed(summary: &str, structured: Option<&serde_json::Value>) -> bool {
        let json = summary_json(summary);
        // A summary written as `{summary, structured}` holds a summary of its own, read one level deep.
        let inner = wrapped_summary(summary);
        let inner_json = inner.as_deref().and_then(summary_json);
        let blocking = [structured, json.as_ref(), inner_json.as_ref()]
            .into_iter()
            .flatten()
            .any(Self::lists_blocking);
        let inner_disagrees = inner
            .as_deref()
            .is_some_and(|inner| Self::summary_disagrees(inner, inner_json.as_ref()));
        blocking || inner_disagrees || Self::summary_disagrees(summary, json.as_ref())
    }

    /// Whether the summary's last `VERDICT:` line or its JSON's verdict,
    /// `result` or `status` reads as anything but pass.
    fn summary_disagrees(summary: &str, json: Option<&serde_json::Value>) -> bool {
        let said = [
            Self::from_summary(summary),
            json.and_then(Self::from_structured),
        ];
        said.into_iter().flatten().any(|said| said != Self::Pass)
    }

    /// The verdict `structured` states: its `verdict` key whenever it has
    /// one, `Unknown` unless it reads as a verdict, else the first `result`
    /// or `status` that opens with a verdict word or a `VERDICT:` label. A
    /// pass that [lists a blocking finding](Self::lists_blocking) is `Unknown`.
    pub fn from_structured(structured: &serde_json::Value) -> Option<Self> {
        let stated = match structured.get(VERDICT_KEYS[0]) {
            Some(given) => {
                let text = given.as_str().unwrap_or_default();
                Some(Self::from_field(text).unwrap_or(Self::Unknown))
            }
            None => Self::other_field(structured).map(|(_, verdict)| verdict),
        };
        match stated {
            Some(Self::Pass) if Self::lists_blocking(structured) => Some(Self::Unknown),
            stated => stated,
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
    /// in is an example or a quote.
    fn from_field(text: &str) -> Option<Self> {
        match strip_label(text, "verdict") {
            Some(value) => Some(Self::from_word(value)),
            None => Self::leading(text),
        }
    }

    /// What the last `VERDICT: <word>` line of `text` says, in any letter
    /// case and with light markdown (`**Verdict:** PASS`); `Unknown` when
    /// that line names no verdict, even if an earlier one did, or a pass is
    /// explained beyond plain success words or followed by other lines.
    pub fn from_summary(text: &str) -> Option<Self> {
        let (line, value) = last_verdict_line(text)?;
        let verdict = Self::from_word(value);
        let end = line.as_ptr() as usize - text.as_ptr() as usize + line.len();
        assert!(
            text.get(..end).is_some_and(|head| head.ends_with(line)),
            "a summary line"
        );
        // A pass the summary goes on to explain might be taken back there.
        let held = verdict == Self::Pass && trails_on(&text[end..]);
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
    /// separator (`—`, `:`, `;`, `.`, ` - `, a line break): any is a choice
    /// (`PASS or FAIL`) or a condition (`PASS, pending fixes`, `PASS (if CI agrees)`),
    /// and a pass explained beyond plain success words ([`holds_back`]) is `Unknown` too.
    fn leading(text: &str) -> Option<Self> {
        let mut words = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty());
        let verdict = Self::of_word(words.next()?)?;
        let first = text
            .find(|c: char| c.is_alphanumeric())
            .expect("a first word");
        let after = text[first..].trim_start_matches(char::is_alphanumeric);
        let head = after
            .split(['—', '–', ':', ';', '.', '\n'])
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

    fn of_word(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "pass" | "passed" | "passes" | "ok" | "approve" | "approved" => Some(Self::Pass),
            "fail" | "failed" | "fails" | "reject" | "rejected" => Some(Self::Fail),
            _ => None,
        }
    }
}

/// The only words a pass's explanation may hold besides numbers (`PASS — all
/// 214 tests pass`): any other could make it conditional, so it reads `Unknown`.
const PLAIN_SUCCESS: [&str; 17] = [
    "all", "tests", "test", "checks", "check", "pass", "passes", "passed", "passing", "green",
    "clean", "findings", "issues", "lgtm", "ok", "ci", "and",
];

/// Whether a pass's explanation could take it back: any word in it that is
/// neither a number nor a [`PLAIN_SUCCESS`] word, `no` before `findings` or
/// `issues` aside. A plain pass has none.
fn holds_back(explanation: &str) -> bool {
    let words: Vec<&str> = explanation
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let plain = |word: &str| PLAIN_SUCCESS.iter().any(|ok| ok.eq_ignore_ascii_case(word));
    words.iter().enumerate().any(|(at, word)| {
        let next = words.get(at + 1).copied().unwrap_or_default();
        let denies_findings = word.eq_ignore_ascii_case("no")
            && ["findings", "issues"]
                .iter()
                .any(|n| n.eq_ignore_ascii_case(next));
        let number = word.chars().all(|c| c.is_ascii_digit());
        !plain(word) && !number && !denies_findings
    })
}

/// Whether a summary goes on after its verdict line with anything but blank
/// lines and the `head: <hex>`/`base: <hex>` lines a review names its commits in.
fn trails_on(rest: &str) -> bool {
    let is_commit = |value: &str| {
        let value = value.trim().trim_matches(['`', '*']);
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit())
    };
    rest.lines().any(|line| {
        let commit = strip_label(line, "head").or_else(|| strip_label(line, "base"));
        !line.trim().is_empty() && !commit.is_some_and(is_commit)
    })
}

/// The verdict JSON of `summary`: the whole summary or its one fenced block,
/// unwrapped from the `complete_task` arguments `{summary, structured}`.
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

/// The whole summary, or its one fenced block, as JSON.
fn raw_summary_json(summary: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(summary)
        .ok()
        .or_else(|| fenced_json(summary))
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    pub fn is_operator_blocker(&self) -> bool {
        matches!(self, Self::Failed { error, .. } if error.trim_start().starts_with(Self::BLOCKED_PREFIX))
    }

    /// Whether `error` ends its run at once: an operator blocker or an
    /// escalation, neither of which a fix loop or a retry can resolve.
    pub fn ends_run(error: &str) -> bool {
        let error = error.trim_start();
        error.starts_with(Self::BLOCKED_PREFIX) || error.starts_with(Self::ESCALATE_PREFIX)
    }
}

impl StageState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Spawning | Self::Running)
    }
}

/// One agent's worth of work: a stage, its attempt number, and for fan-out
/// stages which parallel slot it filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageRun {
    pub id: StageRunId,
    pub run: PipelineRunId,
    pub stage_id: String,
    /// The stage's kind in the pipeline this attempt ran in; `None` on stage
    /// runs recorded before Archon kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StageKind>,
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
    /// Aether identifiers, present once Aether acknowledged creation.
    pub aether_agent: Option<AetherAgentId>,
    pub aether_task: Option<AetherTaskId>,
    pub state: StageState,
    pub verdict: Option<Verdict>,
    pub summary: Option<String>,
    pub structured: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSegment {
    pub delivery: String,
    pub choice: ModelChoice,
    pub started_at: Timestamp,
    pub cost_before_usd: f64,
}

/// The key under a result's `review` where Archon, and only Archon, writes
/// why it refused the review pass the result claims.
pub const REVIEW_REFUSED_KEY: &str = "unverified";

impl StageRun {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    fn verdict_stays_unknown_without_a_readable_word_and_quotes_what_was_said() {
        let unclear = serde_json::json!({"verdict": "lgtm"});
        assert_eq!(
            Verdict::extract("VERDICT: PASS", Some(&unclear)),
            Verdict::Unknown
        );
        assert_eq!(Verdict::stated("", Some(&unclear)).as_deref(), Some("lgtm"));
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
            ("PASS — no blocking findings", Verdict::Unknown),
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

    fn stage(state: StageState, structured: Option<serde_json::Value>) -> StageRun {
        StageRun {
            id: StageRunId::new(),
            run: PipelineRunId::new(),
            stage_id: "ship".into(),
            kind: Some(StageKind::Ship),
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
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
        let checked = review(serde_json::json!({"verdict": "pass", "reviewer_agents": ["agt_r"]}));
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
