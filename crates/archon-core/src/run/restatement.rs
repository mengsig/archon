//! One restatement of a verdict Archon cannot read. A finished agent cannot be
//! asked again, so a stage submits its verdict through `submit_verdict` while
//! it still runs: an unreadable submission is answered with what to restate,
//! and the one sent back as the restatement is final. It never passes what
//! the gate would not: a final restatement that is no pass stands over the
//! result the stage then finishes with, and a pass is still read from that
//! result.

use serde::{Deserialize, Serialize};

use super::{StageRun, StageState, Verdict, VerdictNote, names_environment};

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
    /// Whether the restated submission's findings name the environment as at fault.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub environment: bool,
}

/// One `submit_verdict` call as Archon read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission {
    pub reading: VerdictReading,
    /// Sent as the restatement an earlier answer asked for.
    pub restating: bool,
    /// Whether its findings name the environment as at fault.
    pub environment: bool,
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
    /// The state after the submission `sent`, and its answer. An unreadable
    /// one is asked back, and again while it is not sent as the restatement:
    /// the first answer may never have arrived. The restatement, or a verdict
    /// that reads after the ask, is final, and every later answer repeats it.
    pub fn submit(state: Option<Self>, sent: Submission) -> (Option<Self>, VerdictCheck) {
        let answer = |decision, reading| VerdictCheck { decision, reading };
        let unreadable = sent.reading.note.is_some();
        let (next, check) = match state {
            None => match sent.reading.note {
                None => (None, answer(CheckDecision::Accepted, sent.reading)),
                Some(asked) => {
                    let asked = Self {
                        asked,
                        restated: None,
                        environment: false,
                    };
                    (Some(asked), answer(CheckDecision::Restate, sent.reading))
                }
            },
            Some(Self {
                restated: Some(stands),
                ..
            }) => (state, answer(CheckDecision::Final, stands)),
            Some(_) if unreadable && !sent.restating => {
                (state, answer(CheckDecision::Restate, sent.reading))
            }
            Some(asked) => {
                let restated = Self {
                    restated: Some(sent.reading),
                    environment: sent.environment,
                    ..asked
                };
                (Some(restated), answer(CheckDecision::Final, sent.reading))
            }
        };
        assert!(
            state.is_none_or(|before| next.is_some_and(|after| after.asked == before.asked)),
            "a submission never forgets what was asked"
        );
        assert!(
            state.is_none_or(|before| before.restated.is_none() || next == state),
            "a final restatement never changes"
        );
        assert!(
            check.decision != CheckDecision::Restate || check.reading.verdict == Verdict::Unknown,
            "only an unreadable submission is asked back"
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
                "Archon reads this as {reads}. Finish now as your instructions say, with \
                 exactly this `summary` and `structured`."
            ),
            CheckDecision::Restate => format!(
                "Archon reads this as {reads}. {} Then call `submit_verdict` again with the \
                 corrected `summary` and `structured` and `\"restating\": true`: that \
                 submission is final.",
                how_to_restate(self.reading.note)
            ),
            CheckDecision::Final => format!(
                "Final: Archon reads this stage attempt's restated verdict as {reads}. No \
                 later submission or result changes that to a pass. Finish now as your \
                 instructions say, with the `summary` and `structured` that was restated."
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

    /// The final restatement when it, not the result this completed stage
    /// finished with, is the verdict on record; `None` when the result's own is.
    pub fn restated_over(&self) -> Option<VerdictReading> {
        let stands = self.standing_restatement()?;
        if self.state != StageState::Completed {
            return None;
        }
        let summary = self.summary.as_deref().unwrap_or_default();
        let structured = self.structured.as_ref();
        let (own, _) = match self.only_reviews {
            Some(true) => Verdict::read_review_lead(summary, structured),
            _ => Verdict::read(summary, structured),
        };
        assert_ne!(stands.verdict, Verdict::Pass, "only a non-pass stands");
        (Some(own) != self.verdict).then_some(stands)
    }

    /// Whether the findings behind the verdict on record name the environment
    /// as at fault: the final restatement's when it stands, else the result's.
    pub fn verdict_names_environment(&self) -> bool {
        match (self.restated_over(), self.restatement) {
            (Some(_), Some(restatement)) => restatement.environment,
            _ => self.structured.as_ref().is_some_and(names_environment),
        }
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

    /// A first or repeated submission, sent without the restating mark.
    fn first(reading: VerdictReading) -> Submission {
        Submission {
            reading,
            restating: false,
            environment: false,
        }
    }

    fn restating(reading: VerdictReading) -> Submission {
        Submission {
            restating: true,
            ..first(reading)
        }
    }

    fn asked_for(note: VerdictNote) -> Option<Restatement> {
        Restatement::submit(None, first(unknown(note))).0
    }

    #[test]
    fn a_readable_first_submission_is_accepted_and_keeps_no_state() {
        for reading in [PASS, FAIL] {
            for sent in [first(reading), restating(reading)] {
                let (state, check) = Restatement::submit(None, sent);
                assert_eq!(state, None);
                assert_eq!(check.decision, CheckDecision::Accepted);
                assert_eq!(check.reading, reading);
                assert!(
                    check
                        .message()
                        .contains("Finish now as your instructions say")
                );
            }
        }
    }

    #[test]
    fn each_unreadable_first_submission_is_asked_to_restate() {
        let notes = [
            (VerdictNote::DisputedPass, "\"blocking\": true"),
            (VerdictNote::MissingVerdictLine, "VERDICT: PASS"),
            (VerdictNote::HedgedVerdict, "no other verdict wording"),
        ];
        for (note, how) in notes {
            // Nothing was asked yet, so a restating mark restates nothing.
            for sent in [first(unknown(note)), restating(unknown(note))] {
                let (state, check) = Restatement::submit(None, sent);
                let state = state.expect("the ask is kept");
                assert_eq!((state.asked, state.restated), (note, None));
                assert_eq!(check.decision, CheckDecision::Restate);
                let text = check.message();
                assert!(text.contains(how), "{text}");
                assert!(text.contains("`\"restating\": true`"), "{text}");
                assert!(text.contains("that submission is final"), "{text}");
            }
        }
    }

    #[test]
    fn an_unreadable_repeat_that_restates_nothing_gets_the_ask_again() {
        let asked = asked_for(VerdictNote::DisputedPass);
        let repeats = [
            unknown(VerdictNote::DisputedPass),
            unknown(VerdictNote::MissingVerdictLine),
        ];
        for repeat in repeats {
            let (state, check) = Restatement::submit(asked, first(repeat));
            assert_eq!(state, asked, "the ask stands as it was");
            assert_eq!(check.decision, CheckDecision::Restate);
            assert_eq!(check.reading, repeat);
        }
    }

    #[test]
    fn the_restated_submission_is_final_whatever_it_reads_as() {
        let asked = asked_for(VerdictNote::DisputedPass);
        let still = unknown(VerdictNote::DisputedPass);
        for restated in [PASS, FAIL, still] {
            let (state, check) = Restatement::submit(asked, restating(restated));
            assert_eq!(check.decision, CheckDecision::Final);
            assert_eq!(check.reading, restated);
            assert_eq!(state.unwrap().restated, Some(restated));
            let text = check.message();
            assert!(text.starts_with("Final: "), "{text}");
            assert!(!text.contains("complete_task"), "a woken agent has none");
        }
    }

    #[test]
    fn a_verdict_that_reads_after_the_ask_is_the_restatement_unmarked() {
        let asked = asked_for(VerdictNote::MissingVerdictLine);
        for readable in [PASS, FAIL] {
            let (state, check) = Restatement::submit(asked, first(readable));
            assert_eq!(check.decision, CheckDecision::Final);
            assert_eq!(state.unwrap().restated, Some(readable));
        }
    }

    #[test]
    fn the_restatement_keeps_whether_it_named_the_environment() {
        let asked = asked_for(VerdictNote::DisputedPass);
        for environment in [true, false] {
            let sent = Submission {
                environment,
                ..restating(FAIL)
            };
            let (state, _) = Restatement::submit(asked, sent);
            assert_eq!(state.unwrap().environment, environment);
        }
        let unanswered = Submission {
            environment: true,
            ..first(unknown(VerdictNote::DisputedPass))
        };
        let (state, _) = Restatement::submit(asked, unanswered);
        assert!(!state.unwrap().environment, "only the final one's counts");
    }

    #[test]
    fn a_submission_after_the_final_one_changes_nothing() {
        let asked = asked_for(VerdictNote::HedgedVerdict);
        let (done, _) = Restatement::submit(asked, restating(FAIL));
        let named = Submission {
            environment: true,
            ..restating(PASS)
        };
        for later in [first(PASS), named] {
            let (after, check) = Restatement::submit(done, later);
            assert_eq!(after, done, "the final restatement stands");
            assert_eq!(check.decision, CheckDecision::Final);
            assert_eq!(check.reading, FAIL, "the later pass is not read");
        }
    }

    #[test]
    fn a_final_restatement_that_is_no_pass_stands_over_the_result() {
        let disputed = unknown(VerdictNote::DisputedPass);
        let bound = |restated, terminal| {
            let state = Restatement {
                asked: VerdictNote::DisputedPass,
                restated: Some(restated),
                environment: false,
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
            environment: false,
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
            environment: false,
        };
        for terminal in [PASS, FAIL, unknown(VerdictNote::MissingVerdictLine)] {
            assert_eq!(state.bind(terminal), terminal);
        }
    }
}
