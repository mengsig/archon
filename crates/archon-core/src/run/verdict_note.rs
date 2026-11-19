//! Why a verdict reads `Unknown`, kept beside it so evidence tells a pass its
//! own findings dispute from a fail, and how a review-only lead's verdict is read.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{StageRun, VERDICT_KEYS, Verdict};

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
        let states_none = !Self::states_verdict(summary, structured);
        let recorded = structured
            .filter(|_| states_none)
            .and_then(with_recorded_verdict);
        debug_assert!(recorded.is_none() || states_none, "only a silent result");
        Self::read(summary, recorded.as_ref().or(structured))
    }

    /// Whether a result gives a verdict, readable or not, where [`Self::extract`]
    /// reads one: a `structured` field or a `VERDICT:` line. Metadata such as
    /// `"status": "done"` is none.
    fn states_verdict(summary: &str, structured: Option<&Value>) -> bool {
        structured.and_then(Self::as_worded).is_some() || Self::from_summary(summary).is_some()
    }

    /// Why a result whose verdict reads `Unknown` gives no readable one.
    fn why_unknown(summary: &str, structured: Option<&Value>) -> VerdictNote {
        debug_assert_eq!(Self::extract(summary, structured), Self::Unknown);
        if !Self::states_verdict(summary, structured) {
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

impl StageRun {
    /// Its verdict and note as recorded; a run's `Unknown` recorded before notes
    /// is read again from its result, a review-only lead's by its review record.
    pub fn verdict_reading(&self) -> Option<(Verdict, Option<VerdictNote>)> {
        let verdict = self.verdict?;
        if verdict != Verdict::Unknown || self.verdict_note.is_some() {
            return Some((verdict, self.verdict_note));
        }
        let summary = self.summary.as_deref().unwrap_or_default();
        let structured = self.structured.as_ref();
        let lead = self.only_reviews == Some(true);
        let read = match lead {
            true => Verdict::read_review_lead(summary, structured),
            false => Verdict::read(summary, structured),
        };
        // Only a lead's verdict may change (its record was not read then); another's stays unknown.
        let reading = match read {
            (Verdict::Unknown, _) => read,
            _ if lead => read,
            _ => (Verdict::Unknown, None),
        };
        assert!(reading.1.is_none() || reading.0 == Verdict::Unknown);
        Some(reading)
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

    #[test]
    fn status_metadata_states_no_verdict_but_an_unreadable_verdict_does() {
        let done = json!({"status": "done", "review": {"verdict": "pass"}});
        assert_eq!(
            Verdict::read_review_lead("Reviewed.", Some(&done)),
            (Verdict::Pass, None)
        );
        assert_eq!(
            note("Reviewed.", Some(done)),
            Some(VerdictNote::MissingVerdictLine)
        );
        let unreadable = json!({"verdict": null, "review": {"verdict": "pass"}});
        assert_eq!(
            Verdict::read_review_lead("Reviewed.", Some(&unreadable)),
            (Verdict::Unknown, Some(VerdictNote::HedgedVerdict))
        );
    }

    #[test]
    fn a_blocking_finding_repeated_in_summary_json_is_a_disputed_pass() {
        let pass = json!({"verdict": "pass"});
        let finding = r#"{"verdict": "pass", "findings": [{"blocking": true}]}"#;
        let wrapped = json!({"summary": format!("{finding}\nVERDICT: PASS"), "structured": pass});
        let summaries = [
            finding.to_owned(),
            format!("{finding}\nVERDICT: PASS"),
            wrapped.to_string(),
        ];
        for summary in summaries {
            assert_eq!(
                Verdict::read(&summary, Some(&pass)),
                (Verdict::Unknown, Some(VerdictNote::DisputedPass)),
                "{summary}"
            );
        }
    }

    /// A completed stage as archond stored it before verdict notes.
    fn stored_before_notes(lead: bool, summary: &str, structured: Value) -> StageRun {
        use crate::id::{PipelineRunId, StageRunId};
        use crate::model::{Effort, ModelChoice};
        StageRun {
            only_reviews: lead.then_some(true),
            id: StageRunId::new(),
            run: PipelineRunId::new(),
            stage_id: "review".into(),
            kind: None,
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            record_repair: None,
            host_context: Default::default(),
            choice: ModelChoice {
                model: "codex/astra".parse().unwrap(),
                effort: Effort::Medium,
                tier: None,
            },
            aether_agent: None,
            aether_task: None,
            state: super::super::StageState::Completed,
            verdict: Some(Verdict::extract(summary, Some(&structured))),
            verdict_note: None,
            restatement: None,
            summary: Some(summary.to_owned()),
            structured: Some(structured),
            cost_usd: 0.0,
            started_at: crate::now(),
            finished_at: None,
        }
    }

    #[test]
    fn an_unknown_stored_before_notes_is_read_again_from_its_result() {
        let record = json!({"status": "done", "review": {"verdict": "pass"}});
        let lead = stored_before_notes(true, "Reviewed.", record.clone());
        assert_eq!(lead.verdict, Some(Verdict::Unknown), "as stored then");
        assert_eq!(lead.verdict_reading(), Some((Verdict::Pass, None)));
        let other = stored_before_notes(false, "Reviewed.", record);
        assert_eq!(
            other.verdict_reading(),
            Some((Verdict::Unknown, Some(VerdictNote::MissingVerdictLine)))
        );
        let high = json!({"verdict": "pass", "findings": [{"severity": "high"}]});
        let disputed = stored_before_notes(false, "VERDICT: PASS", high);
        assert_eq!(
            disputed.verdict_reading(),
            Some((Verdict::Unknown, Some(VerdictNote::DisputedPass)))
        );
        let mut noted = disputed.clone();
        noted.verdict_note = Some(VerdictNote::HedgedVerdict);
        assert_eq!(
            noted.verdict_reading().unwrap().1,
            Some(VerdictNote::HedgedVerdict),
            "a note stands"
        );
    }
}
