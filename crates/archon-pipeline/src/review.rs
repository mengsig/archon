//! The review a pipeline declares one of its stages holds (ARCHITECTURE §8.2): what its
//! agent must put on record, what Archon checked of it, and the line later stages get.

use serde_json::Value;

/// A reviewer's model or a skip reason is one line of a summary.
const MAX_FIELD_CHARS: usize = 400;

/// `review` keys only Archon writes: the reviewer it found, or why it found none.
const REVIEWER_AGENT: &str = "reviewer_agent";
const UNVERIFIED: &str = "unverified";

/// What a review-holding stage put on record about its change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// A reviewer Archon found in Aether passed the change as it stands.
    Passed {
        reviewer_model: String,
        reviewer_agent: String,
    },
    /// No review ran, for the stated reason.
    Skipped { reason: String },
}

impl Evidence {
    /// Read `structured.review`. `Err` says what is missing or wrong, in words
    /// for the agent that wrote it and the operator who reads why it failed.
    pub(crate) fn read(structured: Option<&Value>) -> Result<Self, String> {
        let Some(review) = structured.and_then(|s| s.get("review")) else {
            return Err("the result carries no `review` object".into());
        };
        let field = |key: &str| one_line(review.get(key).and_then(Value::as_str).unwrap_or(""));
        match field("verdict").as_str() {
            "pass" => passed(
                field("reviewer_model"),
                field(REVIEWER_AGENT),
                field(UNVERIFIED),
            ),
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
            Self::Passed {
                reviewer_model,
                reviewer_agent,
            } => format!("REVIEW: pass, by {reviewer_model} (agent {reviewer_agent})"),
            Self::Skipped { reason } => format!("REVIEW: skipped: {reason}"),
        }
    }
}

/// A pass counts only with the reviewer Archon found for the model it names.
fn passed(model: String, agent: String, unverified: String) -> Result<Evidence, String> {
    if model.is_empty() {
        return Err("`review.reviewer_model` does not name the reviewer's model".into());
    }
    if !unverified.is_empty() {
        return Err(unverified);
    }
    if agent.is_empty() {
        return Err("Archon has not checked its reviewer against Aether".into());
    }
    Ok(Evidence::Passed {
        reviewer_model: model,
        reviewer_agent: agent,
    })
}

/// The review a stored result holds, as the line later stages are handed;
/// `None` when it holds none that counts.
pub fn on_record(structured: Option<&Value>) -> Option<String> {
    Evidence::read(structured).ok().map(|held| held.line())
}

/// The model a result names as its passing reviewer's, for Archon to check.
pub fn claimed_reviewer(structured: &Value) -> Option<String> {
    let review = structured.get("review")?;
    let field = |key: &str| one_line(review.get(key).and_then(Value::as_str).unwrap_or(""));
    (field("verdict") == "pass" && !field("reviewer_model").is_empty())
        .then(|| field("reviewer_model"))
}

/// Put Archon's finding on a result's review, replacing whatever its agent
/// wrote there: the reviewer's agent, or why none was found.
pub fn record_check(structured: &mut Value, found: Option<Result<String, String>>) {
    let Some(review) = structured.get_mut("review").and_then(Value::as_object_mut) else {
        assert!(found.is_none(), "a checked pass has a review object");
        return;
    };
    review.remove(REVIEWER_AGENT);
    review.remove(UNVERIFIED);
    match found {
        Some(Ok(agent)) => review.insert(REVIEWER_AGENT.into(), agent.into()),
        Some(Err(why)) => review.insert(UNVERIFIED.into(), why.into()),
        None => None,
    };
    debug_assert!(!(review.contains_key(REVIEWER_AGENT) && review.contains_key(UNVERIFIED)));
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

    fn read(structured: Value) -> Result<Evidence, String> {
        Evidence::read(Some(&structured))
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
        let passed = read(pass(json!({"reviewer_agent": "agt_r"})));
        assert_eq!(passed.unwrap().line(), "REVIEW: pass, by a/1 (agent agt_r)");
        let skipped = read(json!({"review": {"verdict": "skipped", "reason": "a typo\nfix"}}));
        assert_eq!(skipped.unwrap().line(), "REVIEW: skipped: a typo fix");
    }

    #[test]
    fn anything_short_of_a_pass_or_a_reasoned_skip_is_refused() {
        assert!(Evidence::read(None).unwrap_err().contains("no `review`"));
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
        let refuted = pass(json!({"reviewer_agent": "agt_r", "unverified": "no such agent"}));
        assert_eq!(read(refuted).unwrap_err(), "no such agent");
    }

    #[test]
    fn archons_finding_replaces_what_the_agent_wrote_about_it() {
        let mut forged = pass(json!({"reviewer_agent": "agt_forged"}));
        assert_eq!(claimed_reviewer(&forged).as_deref(), Some("a/1"));
        record_check(&mut forged, Some(Err("no agent ran on a/1".into())));
        assert_eq!(read(forged.clone()).unwrap_err(), "no agent ran on a/1");
        record_check(&mut forged, Some(Ok("agt_real".into())));
        assert!(read(forged).unwrap().line().ends_with("(agent agt_real)"));

        let mut skipped = json!({"review": {"verdict": "skipped", "reason": "a typo",
            "reviewer_agent": "agt_forged"}});
        assert_eq!(claimed_reviewer(&skipped), None);
        record_check(&mut skipped, None);
        assert_eq!(skipped["review"].get("reviewer_agent"), None);
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
