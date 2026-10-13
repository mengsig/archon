//! The review a pipeline declares one of its stages holds (ARCHITECTURE §8.2):
//! what its agent must put on record, and the line Archon hands later stages.

use serde_json::Value;

/// A reviewer's model or a skip reason is one line of a summary.
const MAX_FIELD_CHARS: usize = 400;

/// What a review-holding stage put on record about its change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// An independent reviewer passed the change as it stands.
    Passed { reviewer_model: String },
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
            "pass" => match field("reviewer_model") {
                model if model.is_empty() => {
                    Err("`review.reviewer_model` does not name the reviewer's model".into())
                }
                reviewer_model => Ok(Self::Passed { reviewer_model }),
            },
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
            Self::Passed { reviewer_model } => format!("REVIEW: pass, by {reviewer_model}"),
            Self::Skipped { reason } => format!("REVIEW: skipped: {reason}"),
        }
    }
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

    #[test]
    fn a_pass_names_its_reviewer_and_a_skip_its_reason() {
        let passed = read(json!({"review": {"verdict": "pass", "reviewer_model": "a/1"}}));
        assert_eq!(passed.unwrap().line(), "REVIEW: pass, by a/1");
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
    fn a_long_reason_is_cut_to_one_summary_line() {
        let reason = "word ".repeat(200);
        let skipped = read(json!({"review": {"verdict": "skipped", "reason": reason}})).unwrap();
        let line = skipped.line();
        assert!(!line.contains('\n'), "{line}");
        assert!(line.chars().count() <= MAX_FIELD_CHARS + "REVIEW: skipped: ".len());
    }
}
