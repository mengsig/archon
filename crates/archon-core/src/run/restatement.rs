//! One restatement of a verdict Archon cannot read. A finished agent cannot be
//! asked again, so a stage submits its verdict through `submit_verdict` while
//! it still runs: a first unreadable submission is answered once with what to
//! restate, and the restated one is final. It never passes what the gate
//! would not: a final restatement that is no pass stands over the result the
//! stage then completes with, and a pass is still read from that result.

use serde::{Deserialize, Serialize};

use super::{StageRun, Verdict, VerdictNote};

/// A verdict as Archon read it, with why when it is `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VerdictReading {
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<VerdictNote>,
}

impl VerdictReading {
    /// A reading as [`Verdict::read`] gives it.
    pub fn new((verdict, note): (Verdict, Option<VerdictNote>)) -> Self {
        assert_eq!(
            note.is_some(),
            verdict == Verdict::Unknown,
            "a note explains only an unknown verdict"
        );
        Self { verdict, note }
    }

    /// The reading that passes less: a fail before an unknown before a pass,
    /// and `terminal` when they pass alike.
    fn weaker(restated: Self, terminal: Self) -> Self {
        let rank = |reading: Self| match reading.verdict {
            Verdict::Fail => 0,
            Verdict::Unknown => 1,
            Verdict::Pass => 2,
        };
        let weaker = match rank(restated) < rank(terminal) {
            true => restated,
            false => terminal,
        };
        assert!(
            weaker.verdict != Verdict::Pass || terminal.verdict == Verdict::Pass,
            "a restatement never passes a result that does not"
        );
        weaker
    }

    /// The words a stage agent is told its verdict reads as.
    pub fn words(self) -> &'static str {
        match (self.verdict, self.note) {
            (Verdict::Pass, _) => "a pass",
            (Verdict::Fail, _) => "a fail",
            (Verdict::Unknown, Some(VerdictNote::DisputedPass)) => {
                "a pass beside a blocking finding, which counts as a fail"
            }
            (Verdict::Unknown, Some(VerdictNote::MissingVerdictLine)) => {
                "no verdict, which counts as a fail"
            }
            (Verdict::Unknown, _) => "neither a pass nor a fail, which counts as a fail",
        }
    }
}

/// What a stage attempt's one restatement stands at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Restatement {
    /// Why the first unreadable submission read as no verdict.
    pub asked: VerdictNote,
    /// What the restated submission read as, which is final; `None` while it is owed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restated: Option<VerdictReading>,
}

/// What `submit_verdict` did with a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckDecision {
    /// It reads as a pass or a fail: the stage completes with it.
    Accepted,
    /// It reads as neither: the stage is asked, once, to restate it.
    Restate,
    /// The restated verdict, whatever it reads as: nothing later changes it.
    Final,
}

/// The answer to one `submit_verdict` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VerdictCheck {
    pub decision: CheckDecision,
    /// The submission's reading; for a `final` given before, that one's.
    pub reading: VerdictReading,
}

impl Restatement {
    /// The state after a submission that read as `reading`, and its answer.
    /// The first unreadable one is asked again; the next is final, and so is
    /// every later answer, which repeats it unread.
    pub fn submit(state: Option<Self>, reading: VerdictReading) -> (Option<Self>, VerdictCheck) {
        let answer = |decision, reading| VerdictCheck { decision, reading };
        let (next, check) = match state {
            None => match reading.note {
                None => (None, answer(CheckDecision::Accepted, reading)),
                Some(asked) => {
                    let asked = Self {
                        asked,
                        restated: None,
                    };
                    (Some(asked), answer(CheckDecision::Restate, reading))
                }
            },
            Some(asked) => {
                let stands = asked.restated.unwrap_or(reading);
                let restated = Self {
                    restated: Some(stands),
                    ..asked
                };
                (Some(restated), answer(CheckDecision::Final, stands))
            }
        };
        assert!(
            state.is_none() || check.decision == CheckDecision::Final,
            "a stage attempt is asked to restate once"
        );
        assert_eq!(
            check.decision == CheckDecision::Restate,
            state.is_none() && reading.verdict == Verdict::Unknown,
            "only a first unreadable submission is asked again"
        );
        (next, check)
    }

    /// The verdict of a stage that completed with a result reading as
    /// `terminal`: a final restatement that is no pass stands, and a pass is
    /// only ever the result's own.
    pub fn bind(self, terminal: VerdictReading) -> VerdictReading {
        match self.restated {
            Some(restated) => VerdictReading::weaker(restated, terminal),
            None => terminal,
        }
    }
}

impl VerdictCheck {
    /// What the calling stage agent is told.
    pub fn message(&self) -> String {
        let reads = self.reading.words();
        let text = match self.decision {
            CheckDecision::Accepted => format!(
                "Archon reads this as {reads}. Call `complete_task` now with exactly this \
                 `summary` and `structured`."
            ),
            CheckDecision::Restate => format!(
                "Archon reads this as {reads}. {} Then call `submit_verdict` once more with \
                 the corrected `summary` and `structured`: that submission is final.",
                how_to_restate(self.reading.note)
            ),
            CheckDecision::Final => format!(
                "Final: Archon reads your restated verdict as {reads}. No later submission or \
                 result changes that to a pass. Call `complete_task` now with the `summary` \
                 and `structured` you restated."
            ),
        };
        assert!(
            text.contains(reads),
            "the answer says how the verdict reads"
        );
        text
    }
}

/// What to change in a submission that read as no verdict for `note`.
fn how_to_restate(note: Option<VerdictNote>) -> &'static str {
    match note {
        Some(VerdictNote::DisputedPass) => {
            "Mark every finding `\"blocking\": true` or `false` by your stage's rule, and \
             restate the verdict: `pass` only if no finding blocks, else `fail`."
        }
        Some(VerdictNote::MissingVerdictLine) => {
            "Put `\"verdict\": \"pass\"` or `\"fail\"` in `structured`, and end the `summary` \
             with a last line that is exactly `VERDICT: PASS` or `VERDICT: FAIL`."
        }
        Some(VerdictNote::HedgedVerdict) | None => {
            "State one verdict: `structured.verdict` exactly `pass` or `fail`, the same word \
             on the `summary`'s last `VERDICT:` line, and no other verdict wording."
        }
    }
}

impl StageRun {
    /// The final restated verdict that is no pass, which stands whatever the
    /// stage completed with; `None` without one.
    pub fn standing_restatement(&self) -> Option<VerdictReading> {
        let restated = self.restatement?.restated?;
        (restated.verdict != Verdict::Pass).then_some(restated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: VerdictReading = VerdictReading {
        verdict: Verdict::Pass,
        note: None,
    };
    const FAIL: VerdictReading = VerdictReading {
        verdict: Verdict::Fail,
        note: None,
    };

    fn unknown(note: VerdictNote) -> VerdictReading {
        VerdictReading::new((Verdict::Unknown, Some(note)))
    }

    #[test]
    fn a_readable_first_submission_is_accepted_and_keeps_no_state() {
        for reading in [PASS, FAIL] {
            let (state, check) = Restatement::submit(None, reading);
            assert_eq!(state, None);
            assert_eq!(check.decision, CheckDecision::Accepted);
            assert_eq!(check.reading, reading);
            assert!(check.message().contains("Call `complete_task` now"));
        }
    }

    #[test]
    fn each_unreadable_first_submission_is_asked_to_restate_once() {
        let notes = [
            (VerdictNote::DisputedPass, "\"blocking\": true"),
            (VerdictNote::MissingVerdictLine, "VERDICT: PASS"),
            (VerdictNote::HedgedVerdict, "no other verdict wording"),
        ];
        for (note, how) in notes {
            let (state, check) = Restatement::submit(None, unknown(note));
            let state = state.expect("the ask is kept");
            assert_eq!((state.asked, state.restated), (note, None));
            assert_eq!(check.decision, CheckDecision::Restate);
            let text = check.message();
            assert!(text.contains(how), "{text}");
            assert!(text.contains("that submission is final"), "{text}");
        }
    }

    #[test]
    fn the_restated_submission_is_final_whatever_it_reads_as() {
        let asked = Restatement::submit(None, unknown(VerdictNote::DisputedPass)).0;
        let still = unknown(VerdictNote::DisputedPass);
        for restated in [PASS, FAIL, still] {
            let (state, check) = Restatement::submit(asked, restated);
            assert_eq!(check.decision, CheckDecision::Final);
            assert_eq!(check.reading, restated);
            assert_eq!(state.unwrap().restated, Some(restated));
            assert!(check.message().starts_with("Final: "));
        }
    }

    #[test]
    fn a_submission_after_the_final_one_changes_nothing() {
        let asked = Restatement::submit(None, unknown(VerdictNote::HedgedVerdict)).0;
        let (done, _) = Restatement::submit(asked, FAIL);
        let (after, check) = Restatement::submit(done, PASS);
        assert_eq!(after, done, "the final restatement stands");
        assert_eq!(check.decision, CheckDecision::Final);
        assert_eq!(check.reading, FAIL, "the later pass is not read");
    }

    #[test]
    fn a_final_restatement_that_is_no_pass_stands_over_the_result() {
        let disputed = unknown(VerdictNote::DisputedPass);
        let bound = |restated, terminal| {
            let state = Restatement {
                asked: VerdictNote::DisputedPass,
                restated: Some(restated),
            };
            state.bind(terminal)
        };
        assert_eq!(bound(FAIL, PASS), FAIL, "a restated fail still fails");
        assert_eq!(bound(disputed, PASS), disputed, "a still-disputed one too");
        assert_eq!(bound(FAIL, disputed), FAIL);
        assert_eq!(bound(PASS, PASS), PASS);
        let hedged = unknown(VerdictNote::HedgedVerdict);
        assert_eq!(bound(disputed, hedged), hedged, "alike: the result's own");
    }

    #[test]
    fn a_restated_pass_never_passes_a_result_that_does_not() {
        let state = Restatement {
            asked: VerdictNote::DisputedPass,
            restated: Some(PASS),
        };
        let disputed = unknown(VerdictNote::DisputedPass);
        assert_eq!(state.bind(disputed), disputed);
        assert_eq!(state.bind(FAIL), FAIL);
    }

    #[test]
    fn an_ask_never_answered_leaves_the_result_as_read() {
        let state = Restatement {
            asked: VerdictNote::MissingVerdictLine,
            restated: None,
        };
        for terminal in [PASS, FAIL, unknown(VerdictNote::MissingVerdictLine)] {
            assert_eq!(state.bind(terminal), terminal);
        }
    }
}
