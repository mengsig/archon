//! The review a pipeline declares one of its stages holds (ARCHITECTURE §8.2): what its
//! agent must put on record, what Archon checked of it, and the line later stages get.

use archon_core::run::{Verdict as Said, strip_label};
use serde_json::Value;

use crate::spec::ReviewDuty;

/// A reviewer's model or a skip reason is one line of a summary.
const MAX_FIELD_CHARS: usize = 400;

/// Most reviewers one record may name: more is a lead listing its tree.
pub const MAX_NAMED_REVIEWERS: usize = 12;

/// `review` keys only Archon writes: the reviewers it found, or why it found too few.
const REVIEWER_AGENTS: &str = "reviewer_agents";
const REVIEWER_MODELS: &str = "reviewer_models";
/// Earlier reviewers the lead did not name that Archon found anchoring named delta reads.
const IMPLICIT_REVIEWER_AGENTS: &str = "implicit_reviewer_agents";
const UNVERIFIED: &str = archon_core::run::REVIEW_REFUSED_KEY;
/// What Archon wrote for the one reviewer before a review could need several,
/// beside the model its lead named.
const LEGACY_REVIEWER_AGENT: &str = "reviewer_agent";
const LEGACY_REVIEWER_MODEL: &str = "reviewer_model";
/// What each reviewer Archon read returned: verdict, head, base and finding count.
const REVIEWER_VERDICTS: &str = "reviewer_verdicts";
/// Reviewer ids the lead named that Archon corrected to the agents it delegated to.
const REVIEWER_CORRECTIONS: &str = "reviewer_id_corrections";
const ARCHON_KEYS: [&str; 8] = [
    REVIEWER_AGENTS,
    REVIEWER_MODELS,
    REVIEWER_VERDICTS,
    REVIEWER_CORRECTIONS,
    IMPLICIT_REVIEWER_AGENTS,
    UNVERIFIED,
    LEGACY_REVIEWER_AGENT,
    LEGACY_REVIEWER_MODEL,
];

/// One reviewer of a passed review: an agent Archon found among those the
/// stage delegated to, and the model Aether ran it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub model: String,
    pub agent: String,
}

/// A reviewer's own verdict, from the JSON result of its task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub pass: bool,
    /// The commit it read the change at.
    pub head: String,
    /// Where the diff it read starts; `None` for the whole change.
    pub base: Option<String>,
    /// How many findings it listed; `None` when its result gave no list.
    pub findings: Option<usize>,
    /// The title of each finding that has one, one line each.
    pub titles: Vec<String>,
}

/// What a reviewer must do when its result gives no verdict Archon can read.
const VERDICT_HOWTO: &str = "complete the task with `structured` set to \
{\"verdict\": \"pass\"|\"fail\", \"head\": \"<commit>\", \"base\": \"<commit>\", \"findings\": []}, \
or end the summary with a line `VERDICT: PASS` or `VERDICT: FAIL` beside lines \
`head: <commit>` and `base: <commit>`";

impl Verdict {
    /// Prefer the task's `structured` field when it states a verdict. Aether can
    /// auto-complete a Codex review with its final text in `summary` and no
    /// structured verdict: the verdict as JSON (bare, wrapped as the
    /// `complete_task` arguments `{summary, structured}`, or in one fenced
    /// block), or a `VERDICT: PASS|FAIL` line beside `head:` and `base:` lines.
    /// Prose with none of these is never read as a verdict.
    pub fn read_result(summary: &str, structured: Option<&Value>) -> Result<Self, String> {
        if structured.is_some_and(|data| Said::from_structured(data).is_some()) {
            return Self::read(structured);
        }
        let json = serde_json::from_str::<Value>(summary)
            .ok()
            .or_else(|| fenced_json(summary));
        if let Some(data) = json {
            let read = Self::read_json(&data, structured)?;
            return match Said::from_summary(summary) {
                None => Ok(read),
                Some(Said::Pass) if read.pass => Ok(read),
                Some(Said::Fail) if !read.pass => Ok(read),
                Some(Said::Unknown) => Err(no_verdict(summary, None)),
                Some(_) => Err(format!(
                    "its summary's JSON verdict and its final `VERDICT:` line disagree: {VERDICT_HOWTO}"
                )),
            };
        }
        match Said::from_summary(summary) {
            Some(Said::Pass) => Self::from_lines(summary, structured, true),
            Some(Said::Fail) => Self::from_lines(summary, structured, false),
            Some(Said::Unknown) | None => Err(no_verdict(summary, structured)),
        }
    }

    /// A verdict given as JSON in the summary; the commits it read and its
    /// findings may come from a `structured` that gave no verdict, the JSON's own first.
    fn read_json(data: &Value, structured: Option<&Value>) -> Result<Self, String> {
        let wrapped = data.get("verdict").is_none()
            && data.get("summary").is_some_and(Value::is_string)
            && data.get("structured").is_some_and(Value::is_object);
        let mut given = match wrapped {
            true => data.get("structured").cloned(),
            false => Some(data.clone()),
        };
        let extra = structured.and_then(Value::as_object);
        if let (Some(Value::Object(fields)), Some(extra)) = (given.as_mut(), extra) {
            assert!(
                !extra.contains_key("verdict"),
                "a structured verdict is read first"
            );
            for key in ["head", "base", "findings"] {
                if let Some(value) = extra.get(key) {
                    fields.entry(key).or_insert_with(|| value.clone());
                }
            }
        }
        Self::read(given.as_ref())
    }

    /// A verdict given as a `VERDICT:` line: the commit it read comes from
    /// `head:`/`base:` lines, else from a `structured` that gave no verdict.
    fn from_lines(summary: &str, structured: Option<&Value>, pass: bool) -> Result<Self, String> {
        let field = |key| structured.and_then(|data| data.get(key));
        let line = |label| labelled(summary, label).map(Value::from);
        let Some(head) = commit_of(line("head").as_ref().or(field("head"))) else {
            return Err(format!(
                "its summary has a `VERDICT:` line but no `head: <commit>` line naming the commit it read: {VERDICT_HOWTO}"
            ));
        };
        let base = match line("base").as_ref().or(field("base")) {
            None | Some(Value::Null) => None,
            given => Some(commit_of(given).ok_or("its `base:` line names no commit")?),
        };
        let listed = field("findings").and_then(Value::as_array);
        Ok(Self {
            pass,
            head,
            base,
            findings: listed.map(Vec::len),
            titles: listed.map(|list| finding_titles(list)).unwrap_or_default(),
        })
    }

    /// Read a reviewer's `{"verdict", "head", "base"}`; `Err` says, in words
    /// for the operator, what keeps it from being a verdict on a commit.
    pub fn read(structured: Option<&Value>) -> Result<Self, String> {
        let Some((value, data)) = structured.and_then(|v| Some((v, v.as_object()?))) else {
            return Err(format!(
                "its result carries no structured verdict: {VERDICT_HOWTO}"
            ));
        };
        let pass = match Said::from_structured(value) {
            Some(Said::Pass) => true,
            Some(Said::Fail) => false,
            Some(Said::Unknown) => {
                let said = Said::stated("", structured).unwrap_or_default();
                return Err(format!(
                    "its `verdict` is {:?}, not \"pass\" or \"fail\"",
                    one_line(&said)
                ));
            }
            None => {
                return Err(format!("its result carries no `verdict`: {VERDICT_HOWTO}"));
            }
        };
        let head = commit_of(data.get("head")).ok_or("its `head` names no commit it read")?;
        let base = match data.get("base") {
            None | Some(Value::Null) => None,
            given => Some(commit_of(given).ok_or("its `base` names no commit")?),
        };
        assert!(head.len() >= MIN_COMMIT_CHARS, "a checked commit id");
        let listed = data.get("findings").and_then(Value::as_array);
        let titles = listed.map(|list| finding_titles(list)).unwrap_or_default();
        Ok(Self {
            pass,
            head,
            base,
            findings: listed.map(Vec::len),
            titles,
        })
    }
}

/// The `title` of each finding in `list` that has a non-empty one.
fn finding_titles(list: &[Value]) -> Vec<String> {
    list.iter()
        .filter_map(|finding| finding.get("title")?.as_str())
        .map(one_line)
        .filter(|title| !title.is_empty())
        .collect()
}

/// The one fenced block of `text` that holds a JSON value; `None` for no block or several.
fn fenced_json(text: &str) -> Option<Value> {
    let mut blocks = text.split("```").skip(1).step_by(2);
    let block = blocks.next()?;
    if blocks.next().is_some() {
        return None;
    }
    let body = block.strip_prefix("json").unwrap_or(block);
    serde_json::from_str(body.trim()).ok()
}

/// Why a result holds no verdict, quoting the one it gave if any.
fn no_verdict(summary: &str, structured: Option<&Value>) -> String {
    match Said::stated(summary, structured) {
        Some(said) => format!(
            "its verdict {:?} reads as neither pass nor fail: {VERDICT_HOWTO}",
            one_line(&said)
        ),
        None => format!("its result carries no readable verdict: {VERDICT_HOWTO}"),
    }
}

/// The value of the last line of `text` that reads `label: value`.
fn labelled(text: &str, label: &str) -> Option<String> {
    let value = text.lines().rev().find_map(|l| strip_label(l, label))?;
    Some(value.trim().trim_matches('`').to_owned())
}

/// Shortest abbreviated commit id a verdict may give.
const MIN_COMMIT_CHARS: usize = 7;

/// `value` as a commit id: hex, abbreviated to no fewer than seven digits.
/// One trailing `...` or `…` is dropped: models mark abbreviations so, and git still resolves the id.
fn commit_of(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    let text = text
        .strip_suffix("...")
        .or_else(|| text.strip_suffix('…'))
        .unwrap_or(text);
    let sized = (MIN_COMMIT_CHARS..=64).contains(&text.len());
    let hex = sized && text.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| text.to_ascii_lowercase())
}

/// What a review-holding stage put on record about its change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// Reviewers Archon found in Aether passed the change as it stands.
    Passed { reviewers: Vec<Reviewer> },
    /// No review ran, for the stated reason.
    Skipped { reason: String },
    /// The run's review level is `none`: nothing was asked for.
    NotAsked,
}

impl Evidence {
    /// Read `structured.review` and hold it to `duty`. `Err` says what is
    /// missing or wrong, in words for the agent that wrote it and the operator.
    pub(crate) fn read(structured: Option<&Value>, duty: ReviewDuty) -> Result<Self, String> {
        assert!(
            !duty.may_skip || duty.reviewers == 1,
            "a skip stands in for one reviewer"
        );
        if duty.reviewers == 0 {
            return Ok(Self::NotAsked);
        }
        let evidence = Self::parse(structured)?;
        let needed = duty.reviewers as usize;
        match &evidence {
            Self::Skipped { .. } if !duty.may_skip => Err(format!(
                "`review.verdict` is \"skipped\", but this run's review level needs {needed} \
                 independent reviewer(s) to pass the change"
            )),
            Self::Passed { reviewers } if reviewers.len() < needed => Err(format!(
                "this run's review level needs {needed} independent reviewers, and the record \
                 names {}",
                reviewers.len()
            )),
            Self::Passed { .. } | Self::Skipped { .. } | Self::NotAsked => Ok(evidence),
        }
    }

    /// Read `structured.review` whatever the run's review level asked for.
    fn parse(structured: Option<&Value>) -> Result<Self, String> {
        let Some(review) = structured.and_then(|s| s.get("review")) else {
            return Err("the result carries no `review` object".into());
        };
        let field = |key: &str| one_line(review.get(key).and_then(Value::as_str).unwrap_or(""));
        match field("verdict").to_ascii_lowercase().as_str() {
            "pass" => passed(review, field(UNVERIFIED)),
            "skipped" => match field("reason") {
                reason if reason.is_empty() => {
                    Err("`review.reason` does not say why no review was needed".into())
                }
                reason => Ok(Self::Skipped { reason }),
            },
            "fail" => Err("the reviewer's verdict is `fail`".into()),
            "" => Err("`review.verdict` is missing".into()),
            _ => Err(format!(
                "`review.verdict` is {:?}, not \"pass\" or \"skipped\"",
                field("verdict")
            )),
        }
    }

    /// The last line of the stage's summary, written by Archon from the
    /// structured evidence so a later stage need not trust the prose.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Passed { reviewers } => {
                let named: Vec<String> = reviewers
                    .iter()
                    .map(|r| format!("{} (agent {})", r.model, r.agent))
                    .collect();
                format!("REVIEW: pass, by {}", named.join(", "))
            }
            Self::Skipped { reason } => format!("REVIEW: skipped: {reason}"),
            Self::NotAsked => "REVIEW: none, as this item's review level asks".to_owned(),
        }
    }
}

/// A pass counts only with the reviewers Archon found and checked. A record
/// stored before 1.1 names its lead's `reviewer_model` beside Archon's agent.
fn passed(review: &Value, unverified: String) -> Result<Evidence, String> {
    if !unverified.is_empty() {
        return Err(unverified);
    }
    let models = lines_of(review, REVIEWER_MODELS, LEGACY_REVIEWER_MODEL);
    let agents = lines_of(review, REVIEWER_AGENTS, LEGACY_REVIEWER_AGENT);
    if models.is_empty() || agents.len() != models.len() {
        return Err("Archon has not checked its reviewers against Aether".into());
    }
    let reviewers = models.into_iter().zip(agents);
    Ok(Evidence::Passed {
        reviewers: reviewers
            .map(|(model, agent)| Reviewer { model, agent })
            .collect(),
    })
}

/// The non-empty lines of the array `review[many]`, else of the string `review[one]`.
fn lines_of(review: &Value, many: &str, one: &str) -> Vec<String> {
    let listed = review.get(many).and_then(Value::as_array);
    let texts: Vec<&str> = match listed {
        Some(entries) => entries.iter().filter_map(Value::as_str).collect(),
        None => review
            .get(one)
            .and_then(Value::as_str)
            .into_iter()
            .collect(),
    };
    let lines = texts.into_iter().map(one_line);
    lines.filter(|line| !line.is_empty()).collect()
}

/// The review a stored result holds, as the line later stages are handed;
/// `None` when it holds none that counts.
pub fn on_record(structured: Option<&Value>) -> Option<String> {
    Evidence::parse(structured).ok().map(|held| held.line())
}

/// The agents a result names in `review.reviewers` for the pass it claims,
/// for Archon to check; `None` unless it claims a pass.
pub fn claimed_reviewers(structured: &Value) -> Option<Vec<String>> {
    let review = structured.get("review")?;
    let verdict = review.get("verdict").and_then(Value::as_str).unwrap_or("");
    if !one_line(verdict).eq_ignore_ascii_case("pass") {
        return None;
    }
    let named = review.get("reviewers").and_then(Value::as_array);
    let agents = named.into_iter().flatten().filter_map(Value::as_str);
    let agents = agents.map(one_line).filter(|agent| !agent.is_empty());
    Some(agents.collect())
}

/// Put Archon's finding on a result's review, replacing whatever its agent
/// wrote there: the reviewers it found, or why they do not make the pass.
pub fn record_check(structured: &mut Value, found: Option<Result<Vec<Reviewer>, String>>) {
    let Some(review) = structured.get_mut("review").and_then(Value::as_object_mut) else {
        assert!(found.is_none(), "a checked pass has a review object");
        return;
    };
    for key in ARCHON_KEYS {
        review.remove(key);
    }
    match found {
        Some(Ok(reviewers)) => {
            assert!(!reviewers.is_empty(), "a checked pass has a reviewer");
            let (models, agents): (Vec<String>, Vec<String>) =
                reviewers.into_iter().map(|r| (r.model, r.agent)).unzip();
            review.insert(REVIEWER_MODELS.into(), models.into());
            review.insert(REVIEWER_AGENTS.into(), agents.into());
        }
        Some(Err(why)) => {
            review.insert(UNVERIFIED.into(), why.into());
        }
        None => {}
    }
    debug_assert!(!(review.contains_key(REVIEWER_AGENTS) && review.contains_key(UNVERIFIED)));
}

/// Put on a checked pass what each of the `reads` returned, by reviewer agent id.
pub fn record_reads(structured: &mut Value, reads: &[(String, Verdict)]) {
    let review = structured.get_mut("review").and_then(Value::as_object_mut);
    let review = review.expect("read verdicts belong to a checked pass");
    assert!(!reads.is_empty(), "a checked pass has a verdict read");
    let entries = reads.iter().map(|(agent, v)| {
        serde_json::json!({
            "agent": agent,
            "verdict": if v.pass { "pass" } else { "fail" },
            "head": v.head,
            "base": v.base,
            "findings": v.findings,
        })
    });
    review.insert(REVIEWER_VERDICTS.into(), entries.collect());
}

/// Put on a checked pass the reviewer ids Archon corrected, one line each.
/// None adds no key.
pub fn record_corrections(structured: &mut Value, corrections: &[String]) {
    if corrections.is_empty() {
        return;
    }
    let review = structured.get_mut("review").and_then(Value::as_object_mut);
    let review = review.expect("a corrected id belongs to a checked pass");
    review.insert(REVIEWER_CORRECTIONS.into(), corrections.to_vec().into());
}

/// One line per reviewer verdict and id correction a result records, for `archon show`.
pub fn verdict_lines(structured: Option<&Value>) -> Vec<String> {
    let review = structured.and_then(|s| s.get("review"));
    let mut lines = Vec::new();
    let corrections = review.and_then(|r| r.get(REVIEWER_CORRECTIONS));
    for note in corrections.and_then(Value::as_array).into_iter().flatten() {
        lines.push(format!(
            "reviewer id corrected: {}",
            note.as_str().unwrap_or("?")
        ));
    }
    let reads = review.and_then(|r| r.get(REVIEWER_VERDICTS));
    for read in reads.and_then(Value::as_array).into_iter().flatten() {
        let text = |key: &str| read.get(key).and_then(Value::as_str).unwrap_or("?");
        let base = read
            .get("base")
            .and_then(Value::as_str)
            .unwrap_or("whole change");
        let findings = match read.get("findings").and_then(Value::as_u64) {
            Some(n) => format!("{n} finding(s)"),
            None => "findings not counted".to_owned(),
        };
        lines.push(format!(
            "reviewer {}: {} at {} from {base}, {findings}",
            text("agent"),
            text("verdict"),
            text("head")
        ));
    }
    lines
}

/// Put on a checked pass the `agents` Archon added to anchor its named delta
/// reads, so the record shows what its coverage rests on. None adds no key.
pub fn record_implicit(structured: &mut Value, agents: &[String]) {
    if agents.is_empty() {
        return;
    }
    let review = structured.get_mut("review").and_then(Value::as_object_mut);
    let review = review.expect("an implicit reviewer anchors a checked pass");
    assert!(
        review.contains_key(REVIEWER_AGENTS) && !review.contains_key(UNVERIFIED),
        "implicit reviewers are recorded on a pass Archon found"
    );
    review.insert(IMPLICIT_REVIEWER_AGENTS.into(), agents.to_vec().into());
}

/// `text` as one trimmed line of at most [`MAX_FIELD_CHARS`] characters.
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = joined.chars().take(MAX_FIELD_CHARS).collect();
    debug_assert!(!line.contains('\n') && line.chars().count() <= MAX_FIELD_CHARS);
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ONE: ReviewDuty = ReviewDuty {
        reviewers: 1,
        may_skip: false,
    };
    const ONE_OR_SKIP: ReviewDuty = ReviewDuty {
        reviewers: 1,
        may_skip: true,
    };
    const THREE: ReviewDuty = ReviewDuty {
        reviewers: 3,
        may_skip: false,
    };

    fn read(structured: Value) -> Result<Evidence, String> {
        Evidence::read(Some(&structured), ONE_OR_SKIP)
    }

    const HEAD: &str = "abcdef1234567";

    #[test]
    fn a_result_in_a_fenced_json_block_or_a_verdict_line_is_a_verdict() {
        let fenced = format!(
            "Done.\n```json\n{{\"verdict\": \"FAIL\", \"head\": \"{HEAD}\", \"findings\": [{{}}, {{}}]}}\n```\n"
        );
        let read = Verdict::read_result(&fenced, None).unwrap();
        assert!(!read.pass && read.head == HEAD && read.findings == Some(2));
        let line = format!("Looks right.\nhead: {HEAD}\nbase: 1234567abc\nverdict: Pass\n");
        let read = Verdict::read_result(&line, None).unwrap();
        assert!(read.pass && read.head == HEAD && read.base.as_deref() == Some("1234567abc"));
        assert_eq!(read.findings, None);
    }

    #[test]
    fn a_fenced_verdict_contradicting_the_final_line_is_refused() {
        let text = format!(
            "```json\n{{\"verdict\": \"pass\", \"head\": \"{HEAD}\"}}\n```\nhead: {HEAD}\nVERDICT: FAIL"
        );
        let error = Verdict::read_result(&text, None).unwrap_err();
        assert!(error.contains("disagree"), "{error}");
    }

    #[test]
    fn prose_without_a_verdict_is_refused_with_what_to_do() {
        let prose = "Blocking findings: the parser drops the tail.";
        let error = Verdict::read_result(prose, None).unwrap_err();
        assert!(
            error.contains("VERDICT: PASS") && error.contains("structured"),
            "{error}"
        );
        let two = "```json\n{}\n```\n```json\n{}\n```";
        assert!(
            Verdict::read_result(two, None).is_err(),
            "two blocks are no verdict"
        );
        let headless = Verdict::read_result("VERDICT: PASS", None).unwrap_err();
        assert!(headless.contains("head: <commit>"), "{headless}");
        let unclear = Verdict::read_result("VERDICT: looks fine", None).unwrap_err();
        assert!(unclear.contains("\"VERDICT: looks fine\""), "{unclear}");
    }

    #[test]
    fn a_verdict_line_is_read_wherever_it_stands_beside_structured_without_one() {
        let mid = format!("**Verdict:** PASS\nhead: {HEAD}\nThen more prose.");
        assert!(Verdict::read_result(&mid, None).unwrap().pass);
        let findings = json!({"head": HEAD, "base": "1234567", "findings": [{"title": "nit"}]});
        let read = Verdict::read_result("Two nits.\nVERDICT: Passed", Some(&findings)).unwrap();
        assert!(read.pass && read.head == HEAD);
        assert_eq!(
            (read.base.as_deref(), read.findings),
            (Some("1234567"), Some(1))
        );
        let in_result = json!({"result": "FAILED", "head": HEAD});
        assert!(!Verdict::read_result("", Some(&in_result)).unwrap().pass);
        let json_verdict = json!({"verdict": "pass"}).to_string();
        let commits = json!({"head": HEAD, "findings": []});
        let read = Verdict::read_result(&json_verdict, Some(&commits)).unwrap();
        assert!(read.pass && read.head == HEAD && read.findings == Some(0));
    }

    #[test]
    fn a_malformed_or_quoted_verdict_never_reads_as_a_pass() {
        for given in [json!(""), json!(null), json!(true), json!("  ")] {
            let structured = json!({"verdict": given, "head": HEAD, "status": "pass"});
            let refused = Verdict::read_result("VERDICT: PASS", Some(&structured));
            assert!(refused.is_err(), "{structured}: {refused:?}");
        }
        let quoted = format!("Example:\n> VERDICT: PASS\n\nVERDICT: not passed\nhead: {HEAD}");
        let refused = Verdict::read_result(&quoted, Some(&json!({"head": HEAD}))).unwrap_err();
        assert!(refused.contains("\"VERDICT: not passed\""), "{refused}");
        for said in ["PASS or FAIL", "PASS, or FAIL", "PASS (or FAIL)"] {
            let choice = format!("VERDICT: {said}\nhead: {HEAD}");
            assert!(Verdict::read_result(&choice, None).is_err(), "{said}");
        }
        let failed = json!({"result": "FAIL — tests failed", "head": HEAD});
        let read = Verdict::read_result("VERDICT: PASS", Some(&failed)).unwrap();
        assert!(!read.pass, "the structured result decides");
        let content = format!("```text\n```not-a-close\nVERDICT: PASS\n```\nhead: {HEAD}");
        assert!(Verdict::read_result(&content, None).is_err());
        let indented = format!("```\n    ```\nVERDICT: PASS\n```\nhead: {HEAD}");
        assert!(Verdict::read_result(&indented, None).is_err());
        let mixed = format!("```\n \t```\nVERDICT: PASS\n```\nhead: {HEAD}");
        assert!(Verdict::read_result(&mixed, None).is_err());
        let tabbed = format!(" \tVERDICT: PASS\nhead: {HEAD}");
        assert!(Verdict::read_result(&tabbed, None).is_err());
        let opening = json!({"status": "FAIL — tests failed\nVERDICT: PASS", "head": HEAD});
        assert!(
            !Verdict::read_result("VERDICT: PASS", Some(&opening))
                .unwrap()
                .pass
        );
        let choice = json!({"status": "PASS or FAIL", "head": HEAD});
        let refused = Verdict::read_result("VERDICT: PASS", Some(&choice)).unwrap_err();
        assert!(refused.contains("\"PASS or FAIL\""), "{refused}");
        let explained = format!("VERDICT: PASS — verdict confirmed by CI\nhead: {HEAD}");
        assert!(Verdict::read_result(&explained, None).unwrap().pass);
        let pass = json!({"verdict": "pass", "head": HEAD});
        let fenced = format!("```json\n{pass}\n```\nVERDICT: not passed");
        let refused = Verdict::read_result(&fenced, None).unwrap_err();
        assert!(refused.contains("\"VERDICT: not passed\""), "{refused}");
    }

    #[test]
    fn a_verdict_keeps_the_titles_of_its_findings() {
        let findings =
            json!([{"title": " Leaks  the\nhandle "}, {"severity": "low"}, {"title": ""}]);
        let data = json!({"verdict": "fail", "head": HEAD, "findings": findings});
        let read = Verdict::read(Some(&data)).unwrap();
        assert_eq!(read.findings, Some(3));
        assert_eq!(read.titles, ["Leaks the handle"]);
    }

    #[test]
    fn what_each_reviewer_returned_is_recorded_and_shown() {
        let mut result = json!({"review": {"verdict": "pass"}});
        let read = Verdict {
            pass: false,
            head: HEAD.into(),
            base: None,
            findings: Some(3),
            titles: Vec::new(),
        };
        record_reads(&mut result, &[("agt_r".into(), read)]);
        record_corrections(&mut result, &["agt_x -> agt_r".to_owned()]);
        let lines = verdict_lines(Some(&result));
        assert_eq!(lines[0], "reviewer id corrected: agt_x -> agt_r");
        let shown = format!("reviewer agt_r: fail at {HEAD} from whole change, 3 finding(s)");
        assert_eq!(lines[1], shown);
        record_check(&mut result, None);
        assert!(
            verdict_lines(Some(&result)).is_empty(),
            "Archon's finding replaces it"
        );
    }

    #[test]
    fn a_verdict_is_read_in_any_letter_case() {
        for (word, expected) in [
            ("PASS", true),
            ("Pass", true),
            (" fail ", false),
            ("FAIL", false),
            ("Passed", true),
            ("**APPROVED**", true),
            ("Rejected", false),
        ] {
            let verdict = json!({"verdict": word, "head": "abcdef1"});
            let read = Verdict::read(Some(&verdict)).unwrap();
            assert_eq!(read.pass, expected, "{word:?}");
        }
        let refused = Verdict::read(Some(&json!({"verdict": "unsure", "head": "abcdef1"})));
        assert!(refused.unwrap_err().contains("not \"pass\" or \"fail\""));
    }

    #[test]
    fn a_review_record_verdict_is_read_in_any_letter_case() {
        let upper = read(pass(json!({"verdict": "PASS",
            "reviewer_models": ["a/1"], "reviewer_agents": ["agt_r"]})));
        assert!(upper.is_ok(), "{upper:?}");
        let skipped = read(json!({"review": {"verdict": "Skipped", "reason": "a typo"}}));
        assert!(skipped.is_ok(), "{skipped:?}");
        let claimed = json!({"review": {"verdict": "Pass", "reviewers": ["agt_r"]}});
        assert_eq!(claimed_reviewers(&claimed), Some(vec!["agt_r".to_string()]));
        let other = read(json!({"review": {"verdict": "Passed"}})).unwrap_err();
        assert!(other.contains("\"Passed\""), "{other}");
    }

    fn pass(extra: Value) -> Value {
        let mut review = json!({"verdict": "pass", "reviewers": ["agt_r"]});
        review
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"review": review})
    }

    fn checked(model: &str, agent: &str) -> Reviewer {
        Reviewer {
            model: model.into(),
            agent: agent.into(),
        }
    }

    #[test]
    fn a_pass_names_its_reviewer_and_a_skip_its_reason() {
        let passed = read(pass(
            json!({"reviewer_models": ["a/1"], "reviewer_agents": ["agt_r"]}),
        ));
        assert_eq!(passed.unwrap().line(), "REVIEW: pass, by a/1 (agent agt_r)");
        let skipped = read(json!({"review": {"verdict": "skipped", "reason": "a typo\nfix"}}));
        assert_eq!(skipped.unwrap().line(), "REVIEW: skipped: a typo fix");
    }

    #[test]
    fn anything_short_of_a_pass_or_a_reasoned_skip_is_refused() {
        let none = Evidence::read(None, ONE).unwrap_err();
        assert!(none.contains("no `review`"), "{none}");
        for (structured, why) in [
            (json!({"verdict": "pass"}), "no `review` object"),
            (json!({"review": {}}), "`review.verdict` is missing"),
            (json!({"review": {"verdict": "pass"}}), "not checked"),
            (json!({"review": {"verdict": "skipped"}}), "`review.reason`"),
            (
                json!({"review": {"verdict": "fail", "reviewers": ["agt_r"]}}),
                "verdict is `fail`",
            ),
            (json!({"review": {"verdict": "Passed"}}), "\"Passed\""),
            (json!({"review": "pass"}), "`review.verdict` is missing"),
        ] {
            let refused = read(structured.clone()).unwrap_err();
            assert!(refused.contains(why), "{structured}: {refused}");
        }
    }

    #[test]
    fn a_pass_counts_only_with_the_reviewers_archon_found() {
        let unchecked = read(pass(json!({}))).unwrap_err();
        assert!(
            unchecked.contains("not checked its reviewers"),
            "{unchecked}"
        );
        let refuted = pass(
            json!({"reviewer_models": ["a/1"], "reviewer_agents": ["agt_r"],
            "unverified": "no such agent"}),
        );
        assert_eq!(read(refuted).unwrap_err(), "no such agent");
        let stored_before = json!({"review": {"verdict": "pass", "reviewer_model": "a/1",
            "reviewer_agent": "agt_old"}});
        assert_eq!(
            read(stored_before).unwrap().line(),
            "REVIEW: pass, by a/1 (agent agt_old)"
        );
    }

    #[test]
    fn the_run_s_review_level_sets_how_many_reviewers_must_pass() {
        let three = json!({"review": {"verdict": "pass",
            "reviewer_models": ["a/1", "b/2", "a/1"],
            "reviewer_agents": ["agt_1", "agt_2", "agt_3"]}});
        let held = Evidence::read(Some(&three), THREE).unwrap();
        assert_eq!(
            held.line(),
            "REVIEW: pass, by a/1 (agent agt_1), b/2 (agent agt_2), a/1 (agent agt_3)"
        );
        assert!(
            Evidence::read(Some(&three), ONE).is_ok(),
            "more than needed passes"
        );
        let one = pass(json!({"reviewer_models": ["a/1"], "reviewer_agents": ["agt_r"]}));
        let short = Evidence::read(Some(&one), THREE).unwrap_err();
        assert!(short.contains("needs 3 independent reviewers"), "{short}");
        assert!(short.ends_with("names 1"), "{short}");
        let half_checked = json!({"review": {"verdict": "pass",
            "reviewer_models": ["a/1", "b/2"], "reviewer_agents": ["agt_1"]}});
        let unchecked = Evidence::read(Some(&half_checked), ONE).unwrap_err();
        assert!(unchecked.contains("not checked"), "{unchecked}");
    }

    #[test]
    fn a_skip_stands_only_where_a_ship_agent_still_judges_it() {
        let skipped = json!({"review": {"verdict": "skipped", "reason": "a typo"}});
        assert!(Evidence::read(Some(&skipped), ONE_OR_SKIP).is_ok());
        for duty in [ONE, THREE] {
            let refused = Evidence::read(Some(&skipped), duty).unwrap_err();
            assert!(refused.contains("\"skipped\""), "{refused}");
            assert!(refused.contains(&duty.reviewers.to_string()), "{refused}");
        }
        assert_eq!(
            on_record(Some(&skipped)).unwrap(),
            "REVIEW: skipped: a typo"
        );
    }

    /// The lead names agents; the models and agents on record are Archon's.
    #[test]
    fn archons_finding_replaces_what_the_agent_wrote_about_it() {
        let mut forged = pass(json!({"reviewer_agents": ["agt_forged"],
            "reviewer_models": ["a/forged"], "reviewer_agent": "x", "reviewer_model": "y"}));
        assert_eq!(claimed_reviewers(&forged).unwrap(), ["agt_r"]);
        record_check(&mut forged, Some(Err("agt_r failed it".into())));
        assert_eq!(read(forged.clone()).unwrap_err(), "agt_r failed it");
        record_check(&mut forged, Some(Ok(vec![checked("a/1", "agt_r")])));
        for legacy in ["reviewer_agent", "reviewer_model"] {
            assert_eq!(forged["review"].get(legacy), None, "{legacy}");
        }
        assert_eq!(
            read(forged).unwrap().line(),
            "REVIEW: pass, by a/1 (agent agt_r)"
        );

        let mut skipped = json!({"review": {"verdict": "skipped", "reason": "a typo",
            "reviewer_agents": ["agt_forged"]}});
        assert_eq!(claimed_reviewers(&skipped), None);
        record_check(&mut skipped, None);
        assert_eq!(skipped["review"].get("reviewer_agents"), None);
        let several = json!({"review": {"verdict": "pass", "reviewers": ["agt_1", " ", "agt_2"]}});
        assert_eq!(claimed_reviewers(&several).unwrap(), ["agt_1", "agt_2"]);
        let unnamed = json!({"review": {"verdict": "pass"}});
        assert_eq!(claimed_reviewers(&unnamed).unwrap(), Vec::<String>::new());
    }

    /// Only Archon says which unnamed reviewers a pass rests on.
    #[test]
    fn archon_records_the_earlier_reviewers_it_added_and_nothing_forged() {
        let mut forged = pass(json!({"implicit_reviewer_agents": ["agt_forged"]}));
        record_check(&mut forged, Some(Ok(vec![checked("a/1", "agt_r")])));
        record_implicit(&mut forged, &[]);
        assert_eq!(forged["review"].get("implicit_reviewer_agents"), None);
        record_implicit(&mut forged, &["agt_whole".to_owned()]);
        assert_eq!(
            forged["review"]["implicit_reviewer_agents"],
            json!(["agt_whole"])
        );
        assert_eq!(
            read(forged).unwrap().line(),
            "REVIEW: pass, by a/1 (agent agt_r)"
        );
    }

    #[test]
    fn a_reviewer_s_verdict_names_the_commit_it_read() {
        let full = json!({"verdict": "pass", "head": "ABCDEF1", "findings": []});
        let read = Verdict::read(Some(&full)).unwrap();
        assert!(read.pass && read.base.is_none());
        assert_eq!(read.head, "abcdef1");
        let delta = json!({"verdict": "fail", "head": "abcdef1", "base": "1234567"});
        let read = Verdict::read(Some(&delta)).unwrap();
        assert!(!read.pass);
        assert_eq!(read.base.as_deref(), Some("1234567"));
        let marked = json!({"verdict": "pass", "head": "5d240fa...", "base": "e803fef…"});
        let read = Verdict::read(Some(&marked)).unwrap();
        assert_eq!(
            (read.head.as_str(), read.base.as_deref()),
            ("5d240fa", Some("e803fef"))
        );
        for (structured, why) in [
            (None, "no structured verdict"),
            (Some(json!({"head": "abcdef1"})), "no `verdict`"),
            (
                Some(json!({"verdict": "lgtm", "head": "abcdef1"})),
                "\"lgtm\"",
            ),
            (Some(json!({"verdict": "pass"})), "`head`"),
            (Some(json!({"verdict": "pass", "head": "abc"})), "`head`"),
            (Some(json!({"verdict": "pass", "head": "HEAD~1"})), "`head`"),
            (Some(json!({"verdict": "pass", "head": "abc..."})), "`head`"),
            (
                Some(json!({"verdict": "pass", "head": "abcdef1......"})),
                "`head`",
            ),
            (Some(json!({"verdict": "pass", "head": "..."})), "`head`"),
            (
                Some(json!({"verdict": "pass", "head": "abcdef1", "base": "main"})),
                "`base`",
            ),
        ] {
            let refused = Verdict::read(structured.as_ref()).unwrap_err();
            assert!(refused.contains(why), "{structured:?}: {refused}");
        }
    }

    #[test]
    fn luna_auto_completed_reviews_carry_json_in_the_summary() {
        let capture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/luna-review-results.json"))
                .unwrap();
        let reviews = capture["reviewers"].as_array().unwrap();
        assert_eq!(reviews.len(), 4);
        for (review, pass) in reviews.iter().zip([true, false, true, true]) {
            let result = &review["result"];
            assert!(result.get("structured").is_none());
            let summary = result["summary"].as_str().unwrap();
            let data: Value = serde_json::from_str(summary).unwrap();
            let verdict = Verdict::read_result(summary, result.get("structured")).unwrap();
            assert_eq!(verdict.pass, pass, "{}", review["agent"]);
            assert_eq!(verdict.head, data["head"]);
            assert_eq!(verdict.base.as_deref(), data["base"].as_str());
        }
    }

    #[test]
    fn luna_reviews_that_write_out_their_complete_task_call_are_read() {
        let capture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/luna-wrapped-review-results.json"
        ))
        .unwrap();
        let reviews = capture["reviewers"].as_array().unwrap();
        assert_eq!(reviews.len(), 2);
        for review in reviews {
            let summary = review["result"]["summary"].as_str().unwrap();
            let data: Value = serde_json::from_str(summary).unwrap();
            assert!(data.get("verdict").is_none(), "{}", review["agent"]);
            let verdict = Verdict::read_result(summary, None).unwrap();
            assert!(verdict.pass, "{}", review["agent"]);
            assert_eq!(verdict.head, data["structured"]["head"]);
            assert_eq!(verdict.base.as_deref(), data["structured"]["base"].as_str());
        }
    }

    #[test]
    fn only_a_whole_complete_task_call_is_unwrapped() {
        let pass = json!({"verdict": "pass", "head": "abcdef1"});
        let fail = json!({"verdict": "fail", "head": "1234567"});
        let own = json!({"verdict": "fail", "head": "1234567", "summary": "s", "structured": pass});
        let verdict = Verdict::read_result(&own.to_string(), None).unwrap();
        assert!(!verdict.pass, "a top-level verdict is the reviewer's own");
        for refused in [
            json!({"summary": "s", "structured": "pass"}),
            json!({"summary": "s", "structured": {"verdict": "pass"}}),
            json!({"structured": pass}),
            json!({"summary": 1, "structured": pass}),
            json!({"summary": "s", "structured": {"summary": "s", "structured": fail}}),
        ] {
            assert!(
                Verdict::read_result(&refused.to_string(), None).is_err(),
                "{refused}"
            );
        }
        let wrapped = json!({"summary": "s", "structured": fail}).to_string();
        assert!(!Verdict::read_result(&wrapped, None).unwrap().pass);
    }

    #[test]
    fn summary_json_never_overrides_a_structured_verdict_or_error() {
        let pass = json!({"verdict": "pass", "head": "abcdef1"}).to_string();
        let fail = json!({"verdict": "fail", "head": "1234567"});
        let verdict = Verdict::read_result(&pass, Some(&fail)).unwrap();
        assert!(!verdict.pass);
        assert_eq!(verdict.head, "1234567");
        let headless = json!({"verdict": "pass"});
        assert!(Verdict::read_result(&pass, Some(&headless)).is_err());
        for verdictless in [
            Value::Null,
            json!("pass"),
            json!({}),
            json!({"findings": []}),
        ] {
            let read = Verdict::read_result(&pass, Some(&verdictless)).unwrap();
            assert!(read.pass, "{verdictless}: falls back to the summary");
        }
    }

    #[test]
    fn a_summary_must_be_a_complete_json_verdict_on_a_commit() {
        let pass = json!({"verdict": "pass", "head": "abcdef1"}).to_string();
        for summary in [
            "".to_owned(),
            "Reviewed; no findings. VERDICT: PASS".to_owned(),
            format!("Reviewed: {pass}"),
            format!("{pass}\nVERDICT: PASS"),
            format!("{pass}{pass}"),
            "{\"verdict\":\"pass\"".to_owned(),
            "null".to_owned(),
            format!("[{pass}]"),
            "{}".to_owned(),
            json!({"verdict": "unsure", "head": "abcdef1"}).to_string(),
            json!({"verdict": "pass"}).to_string(),
            json!({"verdict": "pass", "head": "HEAD"}).to_string(),
            json!({"verdict": "pass", "head": "abcdef1", "base": "main"}).to_string(),
        ] {
            assert!(Verdict::read_result(&summary, None).is_err(), "{summary}");
        }
        assert!(
            Verdict::read_result(&format!(" \n{pass}\n"), None)
                .unwrap()
                .pass
        );
    }

    #[test]
    fn a_long_reason_is_cut_to_one_summary_line() {
        let reason = "word ".repeat(200);
        let skipped = read(json!({"review": {"verdict": "skipped", "reason": reason}})).unwrap();
        let line = skipped.line();
        assert!(!line.contains('\n'), "{line}");
        assert!(line.chars().count() <= MAX_FIELD_CHARS + "REVIEW: skipped: ".len());
    }
}
