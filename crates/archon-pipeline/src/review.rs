//! The review a pipeline declares one of its stages holds (ARCHITECTURE §8.2): what its
//! agent must put on record, what Archon checked of it, and the line later stages get.

use serde_json::Value;

use crate::spec::ReviewDuty;

/// A reviewer's model or a skip reason is one line of a summary.
const MAX_FIELD_CHARS: usize = 400;

/// `review` keys only Archon writes: the reviewers it found, or why it found too few.
const REVIEWER_AGENTS: &str = "reviewer_agents";
const UNVERIFIED: &str = "unverified";
/// What Archon wrote for the one reviewer before a review could need several.
const LEGACY_REVIEWER_AGENT: &str = "reviewer_agent";

/// One reviewer of a passed review: the model the stage named and the agent
/// Archon found for it among those the stage delegated to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reviewer {
    pub model: String,
    pub agent: String,
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

/// A pass counts only with the reviewers Archon found for the models it names.
fn passed(review: &Value, unverified: String) -> Result<Evidence, String> {
    let models = models_of(review);
    if models.is_empty() {
        return Err("`review.reviewer_model` does not name the reviewer's model".into());
    }
    if !unverified.is_empty() {
        return Err(unverified);
    }
    let agents = lines_of(review, REVIEWER_AGENTS, LEGACY_REVIEWER_AGENT);
    if agents.len() != models.len() {
        return Err("Archon has not checked its reviewer against Aether".into());
    }
    let reviewers = models.into_iter().zip(agents);
    Ok(Evidence::Passed {
        reviewers: reviewers
            .map(|(model, agent)| Reviewer { model, agent })
            .collect(),
    })
}

/// The reviewers' models: `reviewer_models`, or the one `reviewer_model`.
fn models_of(review: &Value) -> Vec<String> {
    lines_of(review, "reviewer_models", "reviewer_model")
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

/// The models a result names as its passing reviewers', for Archon to check;
/// empty unless it claims a pass.
pub fn claimed_reviewers(structured: &Value) -> Vec<String> {
    let Some(review) = structured.get("review") else {
        return Vec::new();
    };
    let verdict = review.get("verdict").and_then(Value::as_str).unwrap_or("");
    if one_line(verdict) != "pass" {
        return Vec::new();
    }
    models_of(review)
}

/// Put Archon's finding on a result's review, replacing whatever its agent
/// wrote there: one reviewer agent per claimed model, or why some are missing.
pub fn record_check(structured: &mut Value, found: Option<Result<Vec<String>, String>>) {
    let Some(review) = structured.get_mut("review").and_then(Value::as_object_mut) else {
        assert!(found.is_none(), "a checked pass has a review object");
        return;
    };
    for key in [REVIEWER_AGENTS, LEGACY_REVIEWER_AGENT, UNVERIFIED] {
        review.remove(key);
    }
    match found {
        Some(Ok(agents)) => review.insert(REVIEWER_AGENTS.into(), agents.into()),
        Some(Err(why)) => review.insert(UNVERIFIED.into(), why.into()),
        None => None,
    };
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
        let mut review = json!({"verdict": "pass", "reviewer_model": "a/1"});
        review
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"review": review})
    }

    #[test]
    fn a_pass_names_its_reviewer_and_a_skip_its_reason() {
        let passed = read(pass(json!({"reviewer_agents": ["agt_r"]})));
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
            (json!({"review": {"verdict": "pass"}}), "reviewer_model"),
            (
                json!({"review": {"verdict": "pass", "reviewer_model": " "}}),
                "reviewer_model",
            ),
            (json!({"review": {"verdict": "skipped"}}), "`review.reason`"),
            (
                json!({"review": {"verdict": "fail", "reviewer_model": "a/1"}}),
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
    fn a_pass_counts_only_with_the_reviewer_archon_found() {
        let unchecked = read(pass(json!({}))).unwrap_err();
        assert!(
            unchecked.contains("not checked its reviewer"),
            "{unchecked}"
        );
        let refuted = pass(json!({"reviewer_agents": ["agt_r"], "unverified": "no such agent"}));
        assert_eq!(read(refuted).unwrap_err(), "no such agent");
        let stored_before = pass(json!({"reviewer_agent": "agt_old"}));
        assert!(
            read(stored_before)
                .unwrap()
                .line()
                .ends_with("(agent agt_old)")
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
        let one = pass(json!({"reviewer_agents": ["agt_r"]}));
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

    #[test]
    fn archons_finding_replaces_what_the_agent_wrote_about_it() {
        let mut forged = pass(json!({"reviewer_agents": ["agt_forged"], "reviewer_agent": "x"}));
        assert_eq!(claimed_reviewers(&forged), ["a/1"]);
        record_check(&mut forged, Some(Err("no agent ran on a/1".into())));
        assert_eq!(read(forged.clone()).unwrap_err(), "no agent ran on a/1");
        record_check(&mut forged, Some(Ok(vec!["agt_real".into()])));
        assert_eq!(forged["review"].get("reviewer_agent"), None);
        assert!(read(forged).unwrap().line().ends_with("(agent agt_real)"));

        let mut skipped = json!({"review": {"verdict": "skipped", "reason": "a typo",
            "reviewer_agents": ["agt_forged"]}});
        assert!(claimed_reviewers(&skipped).is_empty());
        record_check(&mut skipped, None);
        assert_eq!(skipped["review"].get("reviewer_agents"), None);
        let several =
            json!({"review": {"verdict": "pass", "reviewer_models": ["a/1", " ", "b/2"]}});
        assert_eq!(claimed_reviewers(&several), ["a/1", "b/2"]);
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
