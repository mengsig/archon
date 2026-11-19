//! Why a verdict reads `Unknown`, kept beside it so evidence tells a pass its
//! own findings dispute from a fail, and how a review-only lead's verdict is read.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{VERDICT_KEYS, Verdict};

/// Why a stage's verdict is `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerdictNote {
    /// A worded pass beside a finding that blocks it (marked `"blocking": true`,
    /// or unmarked at a blocking severity), and nothing else against it.
    DisputedPass,
    /// No verdict where a result may give one: no `structured` verdict and no
    /// `VERDICT:` line.
    MissingVerdictLine,
    /// A verdict given in words that read as no verdict: a choice, a condition,
    /// a pass taken back, or a structured verdict its summary contradicts.
    HedgedVerdict,
}

impl Verdict {
    /// The verdict [`Self::extract`] reads, and why when it is `Unknown`.
    pub fn read(summary: &str, structured: Option<&Value>) -> (Self, Option<VerdictNote>) {
        let verdict = Self::extract(summary, structured);
        let note = (verdict == Self::Unknown).then(|| Self::why_unknown(summary, structured));
        assert_eq!(
            note.is_some(),
            verdict == Self::Unknown,
            "a note explains only an unknown verdict"
        );
        (verdict, note)
    }

    /// A review-only lead's verdict: as [`Self::read`] reads it, except that a
    /// result stating none of its own is given its review record's verdict.
    pub fn read_review_lead(
        summary: &str,
        structured: Option<&Value>,
    ) -> (Self, Option<VerdictNote>) {
        let states_none = Self::stated(summary, structured).is_none();
        let recorded = structured
            .filter(|_| states_none)
            .and_then(with_recorded_verdict);
        debug_assert!(recorded.is_none() || states_none, "only a silent result");
        Self::read(summary, recorded.as_ref().or(structured))
    }

    /// Why a result whose verdict reads `Unknown` gives no readable one.
    fn why_unknown(summary: &str, structured: Option<&Value>) -> VerdictNote {
        debug_assert_eq!(Self::extract(summary, structured), Self::Unknown);
        if Self::stated(summary, structured).is_none() {
            return VerdictNote::MissingVerdictLine;
        }
        let worded = structured
            .and_then(Self::as_worded)
            .or_else(|| Self::from_summary(summary));
        let only_findings =
            !Self::words_dispute(summary) && Self::findings_dispute(summary, structured);
        match worded == Some(Self::Pass) && only_findings {
            true => VerdictNote::DisputedPass,
            false => VerdictNote::HedgedVerdict,
        }
    }
}

/// `structured` with its review record's `verdict` as its own, when it is
/// an object carrying `review.verdict`.
fn with_recorded_verdict(structured: &Value) -> Option<Value> {
    let key = VERDICT_KEYS[0];
    let verdict = structured.get("review")?.get(key)?;
    let mut result = structured.as_object()?.clone();
    let replaced = result.insert(key.to_owned(), verdict.clone());
    assert!(
        replaced.is_none(),
        "only a result with no verdict of its own"
    );
    Some(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn note(summary: &str, structured: Option<Value>) -> Option<VerdictNote> {
        Verdict::read(summary, structured.as_ref()).1
    }

    #[test]
    fn a_readable_verdict_has_no_note() {
        assert_eq!(note("VERDICT: PASS", None), None);
        assert_eq!(note("VERDICT: FAIL", None), None);
        let followed =
            json!({"verdict": "pass", "findings": [{"severity": "high", "blocking": false}]});
        assert_eq!(Verdict::read("", Some(&followed)), (Verdict::Pass, None));
    }

    #[test]
    fn a_pass_beside_a_blocking_finding_is_a_disputed_pass() {
        let high = json!({"verdict": "pass", "findings": [{"severity": "high"}]});
        assert_eq!(
            note("VERDICT: PASS", Some(high)),
            Some(VerdictNote::DisputedPass)
        );
        let marked = json!({"findings": [{"severity": "low", "blocking": true}]});
        assert_eq!(
            note("VERDICT: PASS", Some(marked)),
            Some(VerdictNote::DisputedPass)
        );
    }

    #[test]
    fn a_pass_its_words_also_take_back_is_hedged_not_disputed() {
        let high = json!({"verdict": "pass", "findings": [{"severity": "high"}]});
        assert_eq!(
            note("VERDICT: FAIL", Some(high)),
            Some(VerdictNote::HedgedVerdict)
        );
        assert_eq!(
            note("VERDICT: PASS or FAIL", None),
            Some(VerdictNote::HedgedVerdict)
        );
        let odd = json!({"verdict": "approved-ish"});
        assert_eq!(note("", Some(odd)), Some(VerdictNote::HedgedVerdict));
    }

    #[test]
    fn no_verdict_anywhere_is_a_missing_verdict_line() {
        assert_eq!(
            note("All done.", None),
            Some(VerdictNote::MissingVerdictLine)
        );
        let bare = json!({"findings": []});
        assert_eq!(
            note("All done.", Some(bare)),
            Some(VerdictNote::MissingVerdictLine)
        );
    }

    #[test]
    fn a_review_lead_is_read_by_its_record_only_when_it_states_no_verdict() {
        let record = json!({"review": {"verdict": "pass", "reviewers": ["agt_1"]}});
        assert_eq!(
            Verdict::read("Reviewed.", Some(&record)),
            (Verdict::Unknown, Some(VerdictNote::MissingVerdictLine))
        );
        assert_eq!(
            Verdict::read_review_lead("Reviewed.", Some(&record)),
            (Verdict::Pass, None)
        );
        assert_eq!(
            Verdict::read_review_lead("Reviewed.\nVERDICT: FAIL", Some(&record)).0,
            Verdict::Fail,
            "its own words decide"
        );
        let disputed =
            json!({"review": {"verdict": "pass"}, "findings": [{"severity": "critical"}]});
        assert_eq!(
            Verdict::read_review_lead("Reviewed.", Some(&disputed)),
            (Verdict::Unknown, Some(VerdictNote::DisputedPass))
        );
    }
}
