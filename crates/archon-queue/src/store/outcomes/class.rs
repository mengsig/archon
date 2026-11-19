//! A failed run's failure class, read from the records the run left (its
//! settled stages and the stage it failed under), never from its error's words.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::catalog::StageKind;
use archon_core::cost::{BUDGET_UNENFORCEABLE, BUDGET_USED_UP};
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::outcome::FailureClass;
use archon_core::run::{
    REVIEW_NOT_ON_RECORD, SHIP_STEP, STATED_FAILURE, StageOutcome, StageRun, StageState,
    TalliedFails, UNRESUMABLE_RUN, Verdict, review_field, review_tally_key,
};
use rusqlite::{Connection, OptionalExtension};

use super::super::codec::{SqlContext, from_json, text_column};

/// Why the run that failed under `stage_id` with `error` failed. The daemon's
/// ship step runs no stage, so it is told first ([`ship_step_failed`]). Then
/// the records of the attempt that ended the run: the stage run the run failed
/// in the words of, then the blocker or escalation the driver stops a run on,
/// then how a stage was stopped. Only then is `error` read, and only for the
/// prefixes the daemon writes on purpose. `failed_reviews` is
/// [`failed_reviews`] of `stages`, which may come in any order.
pub(in crate::store) fn failure_class(
    stage_id: &str,
    error: &str,
    stages: &[StageRun],
    failed_reviews: &BTreeSet<StageRunId>,
) -> FailureClass {
    // Recovery dropped the rows it could not read: the rest explain nothing.
    if error.starts_with(UNRESUMABLE_RUN) {
        return FailureClass::Infrastructure;
    }
    let ending = ending_attempt(stage_id, stages);
    // The ship step ends a run past every stage: one named `ship` explains nothing.
    if ship_step_failed(stage_id, error, &ending) {
        return daemon_prefixed_class(error).unwrap_or(FailureClass::Ship);
    }
    let recorded = ended_by(error, &ending)
        .and_then(own_stop)
        .or_else(|| driver_stop(&ending).and_then(own_stop))
        .or_else(|| stopped_class(&ending));
    if let Some(class) = recorded.or_else(|| daemon_prefixed_class(error)) {
        return class;
    }
    if ending.is_empty() {
        return FailureClass::Other;
    }
    if let Some(class) = stated_stop(error, &ending) {
        return class;
    }
    if gate_said(error, REVIEW_NOT_ON_RECORD) {
        return FailureClass::Review;
    }
    judged_class(stage_id, &ending, failed_reviews)
}

/// Whether the daemon's own ship step failed the run, whatever a stage of
/// another kind named `ship` too recorded (`ending`): unless the run failed in
/// that stage's own words, or in the driver's word on it, the failure is the
/// delivery's. A ship stage of that name is read by its records, as any stage.
fn ship_step_failed(stage_id: &str, error: &str, ending: &[&StageRun]) -> bool {
    if stage_id != SHIP_STEP {
        return false;
    }
    assert!(
        ending.iter().all(|s| s.stage_id == SHIP_STEP),
        "the ending attempt is the named stage's"
    );
    // The pipeline's own ship agent, by its kind or (1.4.x) its id: its records decide.
    let kind = ending.iter().find_map(|s| s.kind);
    if !ending.is_empty() && matches!(kind, None | Some(StageKind::Ship)) {
        return false;
    }
    let own_words = ending
        .iter()
        .any(|s| failed_in_words(s, error) && ends_a_run(s));
    // A blocker the driver clipped out of a slot opens as the slot's error does.
    let clipped_stop = ends_run_class(error).is_some() && driver_stop(ending).is_some();
    !own_words && !clipped_stop && !driver_names_stage(error, stage_id)
}

/// Whether the daemon ends a run in this failed stage run's own words: on its
/// blocker or escalation, at once, or on a spawn it gave up on, which has no
/// agent. An agent's plain task failure goes to the gate, which may pass it over.
fn ends_a_run(stage: &StageRun) -> bool {
    match &stage.state {
        StageState::Failed { error } => {
            StageOutcome::ends_run(error) || stage.aether_agent.is_none()
        }
        _ => false,
    }
}

/// Whether `error` is the driver's word on the stage `stage_id` itself, which
/// it opens with the stage's quoted id: "stage `id` failed…", "stage `id` was
/// aborted…", "stage `id`: the operator asked…", "team stage `id` has no roles…".
fn driver_names_stage(error: &str, stage_id: &str) -> bool {
    assert!(!stage_id.is_empty(), "the driver names a stage by its id");
    let named = error.strip_prefix("team ").unwrap_or(error);
    // The ship step names the record stage it missed with a comma after the id.
    named
        .strip_prefix("stage `")
        .and_then(|rest| rest.strip_prefix(stage_id))
        .and_then(|rest| rest.strip_prefix('`'))
        .is_some_and(|rest| rest.starts_with([' ', ':']))
}

/// The stop a completed stage run stated in the result the gate failed it
/// for ([`STATED_FAILURE`]): a blocker or an escalation, in its agent's words.
fn stated_stop(error: &str, ending: &[&StageRun]) -> Option<FailureClass> {
    if !gate_said(error, STATED_FAILURE) {
        return None;
    }
    let stated = |s: &&StageRun| match (&s.state, &s.summary) {
        (StageState::Completed, Some(summary)) => ends_run_class(summary.trim_start()),
        _ => None,
    };
    ending.iter().find_map(stated)
}

/// The stage run of the `ending` attempt whose own failure the run failed in
/// the words of, whole or as the driver clips them: the one that ended it,
/// whatever its siblings recorded. The driver read the lowest slot first.
fn ended_by<'a>(error: &str, ending: &[&'a StageRun]) -> Option<&'a StageRun> {
    ending.iter().copied().find(|s| failed_in_words(s, error))
}

/// Whether the stage run failed in the words the run failed in (`error`),
/// whole or as the driver clips a long error into the run.
fn failed_in_words(stage: &StageRun, error: &str) -> bool {
    let quoted = error.trim();
    match &stage.state {
        StageState::Failed { error } if !quoted.is_empty() => {
            error.trim() == quoted || StageOutcome::run_stop_error(error) == quoted
        }
        _ => false,
    }
}

/// The stage run the driver stops a run on, whose long error it clips: the
/// lowest slot that failed with a blocker or an escalation, flagged or not.
fn driver_stop<'a>(ending: &[&'a StageRun]) -> Option<&'a StageRun> {
    let stops = |s: &StageRun| match &s.state {
        StageState::Failed { error } => StageOutcome::ends_run(error),
        _ => false,
    };
    ending.iter().copied().find(|s| stops(s))
}

/// The class the stage run's own record gives a stop of its making: the
/// blocker or escalation it named, else how it was stopped.
fn own_stop(stage: &StageRun) -> Option<FailureClass> {
    said_stop(stage).or_else(|| stop_of(stage))
}

/// The class of the stop the stage run named in failing its own task: it
/// blocked or escalated, whatever its words share with the daemon's. A
/// failure flagged infrastructure is the backend's, in its words.
fn said_stop(stage: &StageRun) -> Option<FailureClass> {
    match &stage.state {
        StageState::Failed { error } if !stage.infrastructure_failure => {
            ends_run_class(error.trim_start())
        }
        _ => None,
    }
}

/// The class the daemon names by the prefix it puts on a run's `error` on
/// purpose: its budget stop, or a blocker or escalation of its own.
fn daemon_prefixed_class(error: &str) -> Option<FailureClass> {
    if error.starts_with(BUDGET_USED_UP) || error.starts_with(BUDGET_UNENFORCEABLE) {
        return Some(FailureClass::Budget);
    }
    ends_run_class(error)
}

/// The class `error` names by the prefix that ends a run at once.
fn ends_run_class(error: &str) -> Option<FailureClass> {
    if error.starts_with(StageOutcome::ESCALATE_PREFIX) {
        return Some(FailureClass::Escalated);
    }
    let blocked = error.starts_with(StageOutcome::BLOCKED_PREFIX);
    blocked.then_some(FailureClass::Blocked)
}

/// Whether a run that failed under `stage_id` with `error` can be classed
/// without a record of that stage: the ship step, or a stop its prefix names.
pub(in crate::store) fn classed_without_its_stage(stage_id: &str, error: &str) -> bool {
    ship_step_failed(stage_id, error, &[]) || daemon_prefixed_class(error).is_some()
}

/// Whether the gate failed the stage under a headline opening with `said`, as
/// the driver writes it into the run: `stage `id` failed[..]: {said}`.
fn gate_said(error: &str, said: &str) -> bool {
    let headline = error.split("\n\n").next().unwrap_or(error);
    let Some(at) = headline.find(said) else {
        return false;
    };
    let prefix = &headline[..at];
    prefix.starts_with("stage `") && prefix.contains("` failed") && prefix.ends_with(": ")
}

/// The newest stage run of each slot of the stage's last attempt, lowest slot
/// first, as the driver reads the group: a retry replaces a slot in its attempt.
fn ending_attempt<'a>(stage_id: &str, stages: &'a [StageRun]) -> Vec<&'a StageRun> {
    let of_stage = || stages.iter().filter(|s| s.stage_id == stage_id);
    let Some(last) = of_stage().map(|s| s.attempt).max() else {
        return Vec::new();
    };
    let mut slots: BTreeMap<_, &StageRun> = BTreeMap::new();
    for stage in of_stage().filter(|s| s.attempt == last) {
        let newer = slots
            .get(&stage.fan_index)
            .is_none_or(|seen| (stage.started_at, &stage.id) > (seen.started_at, &seen.id));
        if newer {
            slots.insert(stage.fan_index, stage);
        }
    }
    let ending: Vec<&StageRun> = slots.into_values().collect();
    assert!(!ending.is_empty(), "the last attempt has a stage run");
    assert!(ending.iter().all(|s| s.attempt == last));
    assert!(
        ending.is_sorted_by_key(|s| s.fan_index),
        "lowest slot first"
    );
    ending
}

/// A deadline, the agent backend or a cost cap stopped a stage of the `ending`
/// attempt before it judged the work; none for a stage that left no record.
/// A slot the records flag outranks the cost stop read in a sibling's words.
fn stopped_class(ending: &[&StageRun]) -> Option<FailureClass> {
    let stops: Vec<FailureClass> = ending.iter().filter_map(|s| stop_of(s)).collect();
    assert!(stops.len() <= ending.len(), "a stage run stops once");
    let by_rank = [
        FailureClass::Timeout,
        FailureClass::Infrastructure,
        FailureClass::Budget,
    ];
    by_rank.into_iter().find(|class| stops.contains(class))
}

/// How the stage run was stopped before it judged the work, by its own
/// record. The daemon flags every failed agent, so Aether's cost stop, which
/// has no other mark, is read in the stopped run before that flag.
fn stop_of(stage: &StageRun) -> Option<FailureClass> {
    match &stage.state {
        StageState::TimedOut => Some(FailureClass::Timeout),
        StageState::Failed { error } if StageOutcome::is_cost_budget_stop(error) => {
            Some(FailureClass::Budget)
        }
        _ => {
            let infra = stage.infrastructure_failure || never_spawned(stage);
            infra.then_some(FailureClass::Infrastructure)
        }
    }
}

/// A stage run from before the daemon flagged its own spawn failures (1.6.3):
/// it has no agent, so the error is the daemon's, and opens as the daemon
/// opens the spawn it gave up on.
fn never_spawned(stage: &StageRun) -> bool {
    let StageState::Failed { error } = &stage.state else {
        return false;
    };
    let gave_up = [
        StageOutcome::DISK_FULL_SPAWN,
        StageOutcome::SPAWN_KEPT_FAILING,
    ];
    stage.aether_agent.is_none() && gave_up.iter().any(|opening| error.starts_with(opening))
}

/// What the stage that ended the run was for. A review that passed did not
/// fail the run, so it is not counted as a review failure.
fn judged_class(
    stage_id: &str,
    ending: &[&StageRun],
    failed_reviews: &BTreeSet<StageRunId>,
) -> FailureClass {
    assert!(!ending.is_empty());
    assert!(ending.iter().all(|s| s.stage_id == stage_id));
    let all_passed = ending.iter().all(|s| passed(s));
    let kind = ending.iter().find_map(|s| s.kind);
    let reviews = ending.iter().any(|s| holds_review(s, failed_reviews));
    if reviews && !all_passed {
        return FailureClass::Review;
    }
    match kind {
        Some(StageKind::Test) => FailureClass::Verification,
        Some(StageKind::Ship) => FailureClass::Ship,
        Some(_) => FailureClass::Other,
        None => unrecorded_kind(stage_id),
    }
}

/// Whether the stage ran a review, by what it recorded: a review kind, a
/// review-only run's lead, a result carrying a review whose verdict reads pass
/// or fail, or a reviewer's failing verdict in its tally. A recorded kind always
/// wins over the id: only a kindless (1.4.x) stage is read by it, and only with a verdict.
/// A stage run with no name, which no daemon writes, is read by its records alone.
pub(in crate::store) fn holds_review(
    stage: &StageRun,
    failed_reviews: &BTreeSet<StageRunId>,
) -> bool {
    let recorded = stage.kind == Some(StageKind::Review)
        || stage.only_reviews == Some(true)
        || nested_verdict(stage).is_some()
        || failed_reviews.contains(&stage.id);
    recorded || kindless_review(stage)
}

/// A stage run from before Archon kept kinds, named a review and holding the
/// verdict a review returns.
fn kindless_review(stage: &StageRun) -> bool {
    let judged = matches!(stage.verdict, Some(Verdict::Pass | Verdict::Fail));
    stage.kind.is_none() && stage.stage_id.contains("review") && judged
}

/// Whether a reviewer's verdict on the stage's work is on record: a review
/// stage's own pass or fail, the verdict of the review its result carries
/// (unless Archon refused that review), or a failing one in its tally. Being
/// assigned a review (`only_reviews`) is no such record, and neither is a name
/// the stage run does not have.
pub(in crate::store) fn review_on_record(
    stage: &StageRun,
    failed_reviews: &BTreeSet<StageRunId>,
) -> bool {
    let judged = matches!(stage.verdict, Some(Verdict::Pass | Verdict::Fail));
    let own = judged && (stage.kind == Some(StageKind::Review) || kindless_review(stage));
    let carried = nested_verdict(stage).is_some() && !stage.review_refused();
    own || carried || failed_reviews.contains(&stage.id)
}

/// The stage runs among `stages` whose review tally, which archond keeps as
/// each reviewer settles, holds a failing verdict; a tally lists no pass.
pub(in crate::store) fn failed_reviews(
    conn: &Connection,
    stages: &[StageRun],
) -> Result<BTreeSet<StageRunId>, ArchonError> {
    let mut failed = BTreeSet::new();
    for stage in stages {
        let key = review_tally_key(&stage.id);
        let Some(raw) = stored_tally(conn, &key)? else {
            continue;
        };
        let tally: TalliedFails = from_json(&raw, "review tally")?;
        if tally.reviewers() > 0 {
            failed.insert(stage.id.clone());
        }
    }
    assert!(failed.len() <= stages.len(), "a tally is of one stage run");
    assert!(failed.iter().all(|id| stages.iter().any(|s| &s.id == id)));
    Ok(failed)
}

/// The tally stored under `key`, if any; one that is not text is corrupt.
fn stored_tally(conn: &Connection, key: &str) -> Result<Option<String>, ArchonError> {
    assert!(key.starts_with("review_rounds:"), "a review tally's key");
    let stored = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
            text_column(row, 0)
        })
        .optional()
        .ctx("read a review tally")?;
    match stored {
        None => Ok(None),
        Some(Some(raw)) => Ok(Some(raw)),
        Some(None) => Err(ArchonError::Corrupt(format!("{key} holds no text"))),
    }
}

fn nested_review(stage: &StageRun) -> Option<&serde_json::Value> {
    stage.structured.as_ref()?.get("review")
}

/// The verdict the review a stage's result carries states, as the gate reads
/// it: one line, and no more of it than the gate reads of a field.
fn nested_word(stage: &StageRun) -> Option<String> {
    let review = nested_review(stage).filter(|r| r.is_object())?;
    let verdict = review.get("verdict").and_then(serde_json::Value::as_str)?;
    Some(review_field(verdict))
}

/// The pass or fail the review a stage's result carries says, if it says one.
fn nested_verdict(stage: &StageRun) -> Option<Verdict> {
    Verdict::of_phrase(&nested_word(stage)?)
}

/// Whether the stage passed as the gate reads it: its own verdict, and the
/// verdict of the review it carries unless Archon refused that review.
fn passed(stage: &StageRun) -> bool {
    if stage.state != StageState::Completed || stage.verdict != Some(Verdict::Pass) {
        return false;
    }
    if nested_review(stage).is_none_or(|r| !r.is_object()) {
        return true;
    }
    let skipped = nested_word(stage).is_some_and(|v| v.eq_ignore_ascii_case("skipped"));
    let review_passed = skipped || nested_verdict(stage) == Some(Verdict::Pass);
    review_passed && !stage.review_refused()
}

/// Stage runs from before Archon kept their stage's kind (1.4.x) name it only
/// by its id; a review among them is read by [`judged_class`]. A stage run
/// with no id, which no daemon writes, names no kind: its failure is `other`.
fn unrecorded_kind(stage_id: &str) -> FailureClass {
    if stage_id.is_empty() {
        return FailureClass::Other;
    }
    if stage_id.contains("test") {
        FailureClass::Verification
    } else if stage_id.contains("ship") {
        FailureClass::Ship
    } else {
        FailureClass::Other
    }
}
