use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::model::{Effort, ModelChoice};
use archon_core::run::{StageRun, StageState, Verdict};

use super::{FailureClass, advice, of_error, of_stage};

pub(crate) fn stage(state: StageState, effort: Effort) -> StageRun {
    StageRun {
        only_reviews: None,
        id: StageRunId::new(),
        run: PipelineRunId::new(),
        stage_id: "implement".into(),
        kind: None,
        attempt: 1,
        fan_index: 0,
        choice: ModelChoice::pinned("a/work".parse().unwrap(), effort),
        model_segments: Vec::new(),
        infrastructure_failure: false,
        record_repair: None,
        host_context: Default::default(),
        aether_agent: None,
        aether_task: None,
        state,
        verdict: None,
        verdict_note: None,
        sizing: Default::default(),
        restatement: None,
        summary: None,
        structured: None,
        cost_usd: 0.0,
        started_at: archon_core::now(),
        finished_at: None,
    }
}

fn failed(error: &str) -> StageRun {
    let error = error.to_owned();
    stage(StageState::Failed { error }, Effort::Medium)
}

#[test]
fn the_work_falling_short_is_structural() {
    let capped = failed("ESCALATE: the review failed 3 rounds; findings: x");
    assert_eq!(of_stage(&capped), Some(FailureClass::Structural));
    let checks = failed("make ci failed: 2 tests failed");
    assert_eq!(of_stage(&checks), Some(FailureClass::Structural));
    let missing = failed("workspace missing: /archon/worktrees/demo");
    assert_eq!(of_stage(&missing), Some(FailureClass::Structural));
    let timed_out = stage(StageState::TimedOut, Effort::Medium);
    assert_eq!(of_stage(&timed_out), Some(FailureClass::Structural));
    let mut verdict = stage(StageState::Completed, Effort::Medium);
    verdict.verdict = Some(Verdict::Fail);
    assert_eq!(of_stage(&verdict), Some(FailureClass::Structural));
}

#[test]
fn the_world_around_the_work_failing_is_environmental() {
    let mut crashed = failed("the engine exited with status 137");
    crashed.infrastructure_failure = true;
    assert_eq!(of_stage(&crashed), Some(FailureClass::Environmental));
    for error in [
        "UsageLimit: you have hit your limit",
        "the provider quota is exhausted",
        "model codex/x is not in the catalog",
        "RateLimit: slow down",
    ] {
        assert_eq!(of_error(error), FailureClass::Environmental, "{error}");
    }
}

#[test]
fn a_stop_from_outside_is_external() {
    let cancelled = stage(StageState::Cancelled, Effort::Medium);
    assert_eq!(of_stage(&cancelled), Some(FailureClass::External));
    let blocked = failed("BLOCKED: the provider quota is exhausted; raise it");
    assert_eq!(of_stage(&blocked), Some(FailureClass::External));
}

#[test]
fn a_stage_that_did_not_fail_has_no_class() {
    assert_eq!(of_stage(&stage(StageState::Completed, Effort::Low)), None);
    assert_eq!(of_stage(&stage(StageState::Running, Effort::Low)), None);
}

#[test]
fn the_advice_names_what_ran_and_never_claims_a_change() {
    let medium = ModelChoice::pinned("a/work".parse().unwrap(), Effort::Medium);
    let high = ModelChoice::pinned("a/work".parse().unwrap(), Effort::High);
    let step = advice(FailureClass::Structural, Some(&medium));
    assert!(step.contains("structural"), "{step}");
    assert!(
        step.contains("one effort step up from medium on a/work"),
        "{step}"
    );
    let stronger = advice(FailureClass::Structural, Some(&high));
    assert!(
        stronger.contains("a stronger model than a/work"),
        "{stronger}"
    );
    let same = advice(FailureClass::Environmental, Some(&high));
    assert!(
        same.contains("same model and effort (a/work at high)"),
        "{same}"
    );
    let external = advice(FailureClass::External, None);
    assert!(external.contains("external") && external.contains("same model and effort"));
    assert!(
        same.contains("Archon changes no model or effort itself"),
        "{same}"
    );
}

#[test]
fn a_recorded_marker_outranks_the_flag_and_free_text() {
    let mut lost = failed("workspace missing: /archon/worktrees/demo; the engine said: gone");
    lost.infrastructure_failure = true;
    assert_eq!(of_stage(&lost), Some(FailureClass::Structural));
    let capped = failed(
        "ESCALATE: the review failed 3 rounds. Open blocking findings: the catalog is stale; \
         rate limit tests fail",
    );
    assert_eq!(of_stage(&capped), Some(FailureClass::Structural));
    let quoting = failed("make ci failed: test expected `BLOCKED: x` in the reply");
    assert_eq!(of_stage(&quoting), Some(FailureClass::Structural));
}

#[test]
fn a_model_name_with_a_newline_keeps_the_advice_one_line() {
    let odd = ModelChoice::pinned("a/wo\nrk".parse().unwrap(), Effort::Low);
    let line = advice(FailureClass::Structural, Some(&odd));
    assert!(!line.contains('\n'), "{line}");
    assert!(line.contains("one effort step up from low"), "{line}");
}
