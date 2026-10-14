//! The review a pipeline declares one of its stages holds (ARCHITECTURE §8.2): what its
//! agent must put on record, what Archon checked of it, and the line later stages get.

use serde_json::Value;

use crate::spec::ReviewDuty;

/// A reviewer's model or a skip reason is one line of a summary.
const MAX_FIELD_CHARS: usize = 400;

/// Most reviewers one record may name: more is a lead listing its tree.
pub const MAX_NAMED_REVIEWERS: usize = 12;

/// `review` keys only Archon writes: the reviewers it found, or why it found too few.
const REVIEWER_AGENTS: &str = "reviewer_agents";
const REVIEWER_MODELS: &str = "reviewer_models";
const UNVERIFIED: &str = "unverified";
/// What Archon wrote for the one reviewer before a review could need several,
/// beside the model its lead named.
const LEGACY_REVIEWER_AGENT: &str = "reviewer_agent";
const LEGACY_REVIEWER_MODEL: &str = "reviewer_model";
const ARCHON_KEYS: [&str; 5] = [
    REVIEWER_AGENTS,
    REVIEWER_MODELS,
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

/// A reviewer's own verdict, from the structured result of its task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub pass: bool,
    /// The commit it read the change at.
    pub head: String,
    /// Where the diff it read starts; `None` for the whole change.
    pub base: Option<String>,
}

impl Verdict {
    /// Read a reviewer's `{"verdict", "head", "base"}`; `Err` says, in words
    /// for the operator, what keeps it from being a verdict on a commit.
    pub fn read(structured: Option<&Value>) -> Result<Self, String> {
        let Some(data) = structured.and_then(Value::as_object) else {
            return Err("its result carries no structured verdict".into());
        };
        let pass = match data.get("verdict").and_then(Value::as_str).map(str::trim) {
            Some("pass") => true,
            Some("fail") => false,
            Some(other) => {
                return Err(format!(
                    "its `verdict` is {:?}, not \"pass\" or \"fail\"",
                    one_line(other)
                ));
            }
            None => return Err("its result carries no `verdict`".into()),
        };
        let head = commit_of(data.get("head")).ok_or("its `head` names no commit it read")?;
        let base = match data.get("base") {
            None | Some(Value::Null) => None,
            given => Some(commit_of(given).ok_or("its `base` names no commit")?),
        };
        assert!(head.len() >= MIN_COMMIT_CHARS, "a checked commit id");
        Ok(Self { pass, head, base })
    }
}

/// Shortest abbreviated commit id a verdict may give.
const MIN_COMMIT_CHARS: usize = 7;

/// `value` as a commit id: hex, abbreviated to no fewer than seven digits.
fn commit_of(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
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
        match field("verdict").as_str() {
            "pass" => passed(review, field(UNVERIFIED)),
            "skipped" => match field("reason") {
                reason if reason.is_empty() => {
                    Err("`review.reason` does not say why no review was needed".into())
                }
                reason => Ok(Self::Skipped { reason }),
            },
            "fail" => Err("the reviewer's verdict is `fail`".into()),
            "" => Err("`review.verdict` is missing".into()),
            other => Err(format!(
                "`review.verdict` is {other:?}, not \"pass\" or \"skipped\""
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
    if one_line(verdict) != "pass" {
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
            (json!({"review": {"verdict": "PASS"}}), "\"PASS\""),
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
        for (structured, why) in [
            (None, "no structured verdict"),
            (Some(json!({"head": "abcdef1"})), "no `verdict`"),
            (Some(json!({"verdict": "ok", "head": "abcdef1"})), "\"ok\""),
            (Some(json!({"verdict": "pass"})), "`head`"),
            (Some(json!({"verdict": "pass", "head": "abc"})), "`head`"),
            (Some(json!({"verdict": "pass", "head": "HEAD~1"})), "`head`"),
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
    fn a_long_reason_is_cut_to_one_summary_line() {
        let reason = "word ".repeat(200);
        let skipped = read(json!({"review": {"verdict": "skipped", "reason": reason}})).unwrap();
        let line = skipped.line();
        assert!(!line.contains('\n'), "{line}");
        assert!(line.chars().count() <= MAX_FIELD_CHARS + "REVIEW: skipped: ".len());
    }
}
