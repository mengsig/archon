//! What archond ships for a pipeline that declares `[pipeline.ship]`
//! (ARCHITECTURE §8.7): the record a stage puts on its result, the delivery
//! the driver asks of the daemon once every stage passed, and how it ended.
//! Pure data and parsing; git and `gh` live in the daemon.

use archon_core::repo::Attribution;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The stage id a failed delivery is reported under; no agent runs it.
pub const SHIP_STEP: &str = "ship";
/// GitHub refuses a longer pull request title.
pub const MAX_TITLE_CHARS: usize = 256;
/// GitHub refuses a body of 65536 characters; archond's own lines fit above this.
pub const MAX_BODY_CHARS: usize = 60_000;

/// What a stage reports of the commits it leaves to be shipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipRecord {
    /// `git rev-parse HEAD` when the stage finished.
    pub commit: String,
    /// `git rev-parse HEAD^{tree}`: the tree that was verified and reviewed.
    pub tree: String,
    pub title: String,
    pub body: String,
    /// The verification commands the stage ran, as it listed them.
    pub checks: Vec<String>,
}

impl ShipRecord {
    /// Read the record from a stage's `structured` result. `Err` says what is
    /// missing or wrong, in words for the agent that wrote it.
    pub fn read(structured: Option<&Value>) -> Result<Self, String> {
        let Some(data) = structured.filter(|data| data.is_object()) else {
            return Err("the result carries no `structured` object".into());
        };
        let commit = object_id(data, "commit")?;
        let tree = object_id(data, "tree")?;
        let Some(pr) = data.get("pr").filter(|pr| pr.is_object()) else {
            return Err("`pr` is missing: it holds the pull request's `title` and `body`".into());
        };
        let text = |key: &str| pr.get(key).and_then(Value::as_str).unwrap_or("").trim();
        let title = text("title")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            return Err(format!(
                "`pr.title` must be one line of 1 to {MAX_TITLE_CHARS} characters"
            ));
        }
        if text("body").is_empty() {
            return Err(
                "`pr.body` is empty: it describes the change and how it was verified".into(),
            );
        }
        let checks = data.get("checks").and_then(Value::as_array);
        let checks = checks.into_iter().flatten().filter_map(Value::as_str);
        Ok(Self {
            commit,
            tree,
            title,
            body: text("body").to_owned(),
            checks: checks.map(str::to_owned).collect(),
        })
    }
}

/// `data[key]` as a full git object id: 40 (SHA-1) or 64 (SHA-256) hex digits.
fn object_id(data: &Value, key: &str) -> Result<String, String> {
    let id = data.get(key).and_then(Value::as_str).unwrap_or("").trim();
    let full = matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit());
    if !full {
        return Err(format!(
            "`{key}` must be the full hash `git rev-parse` prints, got {id:?}"
        ));
    }
    Ok(id.to_ascii_lowercase())
}

/// The delivery of one run, asked of the daemon once its last stage passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipPlan {
    /// The stage whose latest pass carries `record`.
    pub record_stage: String,
    pub record: ShipRecord,
    /// The pull request body: the record's, then what Archon adds
    /// (`pr_text`); delivery ends it with [`pr_body`] under the repository's
    /// attribution policy as it stands then.
    pub body: String,
    /// The run's summary once it shipped, before the line saying where to.
    pub summary: String,
    /// The risk level the pull request is labelled with, and why.
    pub risk: crate::risk::Assessment,
    /// The line Archon wrote for the review the record stage holds.
    pub review: Option<String>,
}

/// How a delivery ended; persisted by the daemon and fed back through
/// [`crate::RunDriver::on_shipped`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ShipOutcome {
    /// `commit` is on the run branch and, in `pr` mode, behind `pr_url`;
    /// `updated` when that pull request existed and `commit` was pushed into it.
    Shipped {
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr_url: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        updated: bool,
        /// How labelling the pull request with its risk level went, in one
        /// line for the run's summary; never a reason the delivery failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        risk: Option<String>,
    },
    /// Nothing more is delivered; `error` says exactly why.
    Failed { error: String },
}

impl ShipOutcome {
    /// The lines that end a shipped run's summary: how its risk label went,
    /// then where it shipped.
    pub fn lines(&self) -> Option<String> {
        let line = self.line()?;
        match self {
            Self::Shipped {
                risk: Some(risk), ..
            } => {
                assert!(!risk.contains('\n'), "one line");
                Some(format!("{risk}\n{line}"))
            }
            _ => Some(line),
        }
    }

    /// The line saying where a shipped run went.
    pub fn line(&self) -> Option<String> {
        match self {
            Self::Shipped {
                commit,
                pr_url: Some(url),
                updated: true,
                ..
            } => {
                let pr = archon_core::pull_request::PullRequestRef::from_url(url);
                let number = pr.map_or_else(|| url.clone(), |pr| pr.number.to_string());
                Some(format!("SHIPPED: updated #{number} to {commit} ({url})"))
            }
            Self::Shipped {
                commit,
                pr_url: Some(url),
                updated: false,
                ..
            } => Some(format!("SHIPPED: {url} (commit {commit})")),
            Self::Shipped {
                commit,
                pr_url: None,
                ..
            } => Some(format!("SHIPPED: commit {commit} on the run branch")),
            Self::Failed { .. } => None,
        }
    }
}

/// What Archon adds to a pull request body below the record's own text.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BodyAdditions<'a> {
    /// The line Archon wrote for the review the record stage holds.
    pub review: Option<&'a str>,
    /// The stages that passed after the record stage, with their summaries.
    pub later: &'a [(&'a str, &'a str)],
    /// The change report's summary.
    pub report: Option<&'a str>,
}

/// The pull request text a run delivers: `record`'s, then `added` under
/// headings of its own, each part cut to leave room below GitHub's limit.
pub(crate) fn pr_text(record: &ShipRecord, added: BodyAdditions<'_>) -> String {
    use crate::gate::clip;
    assert!(
        added.review.is_none_or(|line| !line.contains('\n')),
        "one line"
    );
    let mut body = clip(&record.body, MAX_BODY_CHARS / 2);
    if let Some(review) = added.review {
        body.push_str(&format!("\n\n{review}"));
    }
    if !added.later.is_empty() {
        let stages: String = added.later.iter().map(stage_section).collect();
        let stages = clip(stages.trim_end(), MAX_BODY_CHARS / 5);
        body.push_str(&format!("\n\n## Review and verification\n{stages}"));
    }
    if let Some(report) = added.report.map(str::trim).filter(|r| !r.is_empty()) {
        let report = clip(report, MAX_BODY_CHARS / 5);
        body.push_str(&format!("\n\n## Change report\n\n{report}"));
    }
    debug_assert!(body.chars().count() < MAX_BODY_CHARS);
    body
}

/// The pull request body archond sends: `text` from [`ShipPlan::body`], then
/// `item`'s line where the repository's `attribution` policy keeps it.
pub fn pr_body(text: &str, item: &str, attribution: Attribution) -> String {
    assert!(!item.is_empty(), "every run belongs to an item");
    assert!(!text.trim().is_empty(), "a ship record has a body");
    let body = match attribution {
        Attribution::ItemId => format!("{text}\n\nArchon item: {item}\n"),
        Attribution::None => format!("{text}\n"),
    };
    debug_assert!(body.chars().count() <= MAX_BODY_CHARS);
    body
}

/// The lines of `text` that attribute it to a model or a tool: a
/// `Co-Authored-By:` trailer or a `Generated with` footer, each trimmed.
/// A line is matched from its start, past leading space and emoji, so prose
/// that mentions such a line in passing or in backticks is not one. The
/// trailer is matched as git parses it (`Co-Authored-By :x` too); the footer
/// phrase must end a word, so "Generated without" is prose.
pub fn attribution_lines(text: &str) -> Vec<&str> {
    fn after<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
        let rest = line.get(prefix.len()..)?;
        line[..prefix.len()]
            .eq_ignore_ascii_case(prefix)
            .then_some(rest)
    }
    let is_trailer = |line: &str| {
        after(line, "co-authored-by")
            .is_some_and(|rest| rest.trim_start_matches([' ', '\t']).starts_with(':'))
    };
    let is_footer = |line: &str| {
        after(line, "generated with")
            .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()))
    };
    let found: Vec<&str> = text
        .lines()
        .map(|line| line.trim_start_matches(|c: char| c.is_whitespace() || !c.is_ascii()))
        .filter(|line| is_trailer(line) || is_footer(line))
        .map(str::trim_end)
        .collect();
    debug_assert!(found.iter().all(|line| !line.contains('\n')));
    found
}

fn stage_section((stage, summary): &(&str, &str)) -> String {
    format!("\n### `{stage}`\n\n{}\n", summary.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const TREE: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn record() -> Value {
        json!({"commit": COMMIT, "tree": TREE, "checks": ["cargo test", 7],
            "pr": {"title": "  fix(parser): handle\n empty input ", "body": "Why and how.\n"}})
    }

    #[test]
    fn a_record_names_the_commit_the_tree_and_the_pull_request() {
        let read = ShipRecord::read(Some(&record())).unwrap();
        assert_eq!((read.commit.as_str(), read.tree.as_str()), (COMMIT, TREE));
        assert_eq!(read.title, "fix(parser): handle empty input");
        assert_eq!(read.body, "Why and how.");
        assert_eq!(read.checks, ["cargo test"], "only listed commands are kept");
        let upper = json!({"commit": COMMIT.to_uppercase(), "tree": TREE,
            "pr": {"title": "t", "body": "b"}});
        assert_eq!(ShipRecord::read(Some(&upper)).unwrap().commit, COMMIT);
    }

    #[test]
    fn a_record_that_cannot_be_shipped_says_what_is_wrong() {
        assert!(ShipRecord::read(None).unwrap_err().contains("structured"));
        let without = |key: &str| {
            let mut data = record();
            data.as_object_mut().unwrap().remove(key);
            ShipRecord::read(Some(&data)).unwrap_err()
        };
        assert!(without("commit").contains("`commit`"));
        assert!(without("tree").contains("`tree`"));
        assert!(without("pr").contains("`pr` is missing"));
        for (pr, why) in [
            (json!({"title": " ", "body": "b"}), "`pr.title`"),
            (json!({"title": "t".repeat(257), "body": "b"}), "`pr.title`"),
            (json!({"title": "t", "body": " \n"}), "`pr.body`"),
        ] {
            let mut data = record();
            data["pr"] = pr;
            let refused = ShipRecord::read(Some(&data)).unwrap_err();
            assert!(refused.contains(why), "{refused}");
        }
        let mut short = record();
        short["tree"] = json!("89abcde");
        assert!(
            ShipRecord::read(Some(&short))
                .unwrap_err()
                .contains("full hash")
        );
    }

    #[test]
    fn the_body_ends_with_the_item_and_carries_the_change_report() {
        let record = ShipRecord::read(Some(&record())).unwrap();
        let text = pr_text(&record, BodyAdditions::default());
        assert_eq!(text, "Why and how.", "the plan's text names no item");
        let plain = pr_body(&text, "qi_1", Attribution::ItemId);
        assert_eq!(plain, "Why and how.\n\nArchon item: qi_1\n");
        let added = BodyAdditions {
            review: Some("REVIEW: pass, by a/1 (agent agt_r)"),
            later: &[
                ("review", "No findings.\nVERDICT: PASS\n"),
                ("test", "All green."),
            ],
            report: Some(" The report. \n"),
        };
        let full = pr_body(&pr_text(&record, added), "qi_1", Attribution::ItemId);
        assert_eq!(
            full,
            "Why and how.\n\nREVIEW: pass, by a/1 (agent agt_r)\n\n\
             ## Review and verification\n\n### `review`\n\nNo findings.\nVERDICT: PASS\n\n\
             ### `test`\n\nAll green.\n\n## Change report\n\nThe report.\n\nArchon item: qi_1\n"
        );
        let mut long = record.clone();
        long.body = "x".repeat(MAX_BODY_CHARS * 2);
        let flood = "y".repeat(MAX_BODY_CHARS);
        let added = BodyAdditions {
            review: None,
            later: &[("review", &flood), ("test", &flood)],
            report: Some(&flood),
        };
        let cut = pr_body(&pr_text(&long, added), "qi_1", Attribution::ItemId);
        assert!(cut.chars().count() <= MAX_BODY_CHARS);
        assert!(cut.contains("## Change report") && cut.ends_with("Archon item: qi_1\n"));
    }

    #[test]
    fn without_attribution_the_body_names_no_item() {
        let record = ShipRecord::read(Some(&record())).unwrap();
        let plain = pr_body(
            &pr_text(&record, BodyAdditions::default()),
            "qi_1",
            Attribution::None,
        );
        assert_eq!(plain, "Why and how.\n");
        let added = BodyAdditions {
            review: Some("REVIEW: pass, by a/1 (agent agt_r)"),
            later: &[("test", "All green.")],
            report: Some("The report."),
        };
        let full = pr_body(&pr_text(&record, added), "qi_1", Attribution::None);
        assert!(
            !full.contains("Archon item:") && !full.contains("qi_1"),
            "{full}"
        );
        assert!(
            full.ends_with("## Change report\n\nThe report.\n"),
            "{full}"
        );
    }

    #[test]
    fn attribution_lines_are_the_trailers_and_footers_not_mentions() {
        let body = "Why.\n\n\u{1f916} Generated with [Claude Code](https://claude.com/claude-code)\n\n\
                    Co-Authored-By: Claude <noreply@anthropic.com>  \n  co-authored-by: x <y>\n\
                    Refuses `Generated with` lines; nothing was Generated with care.\n\
                    > Generated with quotes is prose.";
        assert_eq!(
            attribution_lines(body),
            [
                "Generated with [Claude Code](https://claude.com/claude-code)",
                "Co-Authored-By: Claude <noreply@anthropic.com>",
                "co-authored-by: x <y>",
            ]
        );
        assert!(attribution_lines("Why and how.\nCo-authored by people.").is_empty());
        assert_eq!(
            attribution_lines(
                "Co-Authored-By:Claude <c@a>\nCo-authored-by\t: x <y>\nCo-authored-byline"
            ),
            ["Co-Authored-By:Claude <c@a>", "Co-authored-by\t: x <y>"],
            "every trailer git parses, glued to its value or spaced from its colon"
        );
        let prose = "Generated without changing the public API.\nGenerated within budget.";
        assert!(attribution_lines(prose).is_empty(), "words, not the footer");
        assert_eq!(
            attribution_lines("Generated with: a tool"),
            ["Generated with: a tool"]
        );
        assert!(attribution_lines("").is_empty());
    }

    #[test]
    fn an_outcome_round_trips_and_says_where_it_shipped() {
        let pr = ShipOutcome::Shipped {
            commit: COMMIT.into(),
            pr_url: Some("https://github.com/o/r/pull/7".into()),
            updated: false,
            risk: None,
        };
        let text = serde_json::to_string(&pr).unwrap();
        assert!(!text.contains("updated"), "{text}");
        assert_eq!(serde_json::from_str::<ShipOutcome>(&text).unwrap(), pr);
        assert!(
            pr.line()
                .unwrap()
                .starts_with("SHIPPED: https://github.com/o/r/pull/7")
        );
        let updated = ShipOutcome::Shipped {
            commit: COMMIT.into(),
            pr_url: Some("https://github.com/o/r/pull/7".into()),
            updated: true,
            risk: None,
        };
        let text = serde_json::to_string(&updated).unwrap();
        assert_eq!(serde_json::from_str::<ShipOutcome>(&text).unwrap(), updated);
        let line = updated.line().unwrap();
        assert!(
            line.starts_with(&format!("SHIPPED: updated #7 to {COMMIT}")),
            "{line}"
        );
        let local = ShipOutcome::Shipped {
            commit: COMMIT.into(),
            pr_url: None,
            updated: false,
            risk: None,
        };
        assert!(local.line().unwrap().contains("on the run branch"));
        let failed = ShipOutcome::Failed { error: "e".into() };
        assert_eq!(failed.line(), None);
    }
}
