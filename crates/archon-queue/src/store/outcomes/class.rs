//! A failed run's failure class, read from the records the run left (its
//! settled stages and the stage it failed under), never from its error's words.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::catalog::StageKind;
use archon_core::cost::{BUDGET_UNENFORCEABLE, BUDGET_USED_UP};
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::outcome::FailureClass;
use archon_core::run::{
    PipelineRun, REVIEW_NOT_ON_RECORD, RunState, SHIP_STEP, STATED_FAILURE, StageOutcome, StageRun,
    StageState, TalliedFails, UNRESUMABLE_RUN, Verdict, review_field, review_tally_key,
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
    run: &PipelineRun,
    stages: &[StageRun],
    failed_reviews: &BTreeSet<StageRunId>,
) -> FailureClass {
    let RunState::Failed { stage_id, error } = &run.state else {
        unreachable!("only a failed run has a failure class");
    };
    assert!(
        stages.iter().all(|s| s.run == run.id),
        "the run's own stage runs"
    );
    // Recovery dropped the rows it could not read: the rest explain nothing.
    if error.starts_with(UNRESUMABLE_RUN) {
        return FailureClass::Infrastructure;
    }
    let stages = flag_wrapped_spawns(run, stages);
    let ending = ending_attempt(stage_id, &stages);
    // The ship step ends a run past every stage: one named `ship` explains nothing.
    if ship_step_failed(stage_id, error, &ending) {
        return daemon_prefixed_class(error).unwrap_or(FailureClass::Ship);
    }
    let recorded = match ended_by(error, &ending) {
        // The run ended in these slots' words: a sibling's stop did not end it.
        Some(stage) => own_stop(stage).or_else(|| stopped_class(&in_words(error, &ending))),
        None => driver_stop(&ending)
            .and_then(own_stop)
            .or_else(|| stopped_class(&ending)),
    };
    if let Some(class) = recorded.or_else(|| daemon_prefixed_class(error)) {
        return class;
    }
    if ending.is_empty() {
        return FailureClass::Other;
    }
    if let Some(class) = stated_stop(stage_id, error, &ending) {
        return class;
    }
    if gate_said(error, stage_id, REVIEW_NOT_ON_RECORD) {
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
fn stated_stop(stage_id: &str, error: &str, ending: &[&StageRun]) -> Option<FailureClass> {
    if !gate_said(error, stage_id, STATED_FAILURE) {
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
    ending.iter().copied().find(|s| ended_in_words(s, error))
}

/// Whether the run ended in this stage run's own words: it failed in them,
/// and the daemon ends a run in a slot's words ([`ends_a_run`]).
fn ended_in_words(stage: &StageRun, error: &str) -> bool {
    failed_in_words(stage, error) && ends_a_run(stage)
}

/// The stage runs of the `ending` attempt the run ended in the words of.
fn in_words<'a>(error: &str, ending: &[&'a StageRun]) -> Vec<&'a StageRun> {
    let said: Vec<&StageRun> = ending
        .iter()
        .copied()
        .filter(|s| ended_in_words(s, error))
        .collect();
    assert!(
        !said.is_empty(),
        "read only for a run that ended in a slot's words"
    );
    assert!(said.len() <= ending.len());
    said
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

/// Whether the gate failed the stage `stage_id` under a headline opening with
/// `said`, as the driver writes it: `stage `id` failed[ after N of M fix loop(s)]: {said}`.
fn gate_said(error: &str, stage_id: &str, said: &str) -> bool {
    assert!(!said.is_empty(), "the gate's reason has words");
    let Some(rest) = error
        .strip_prefix("stage `")
        .and_then(|rest| rest.strip_prefix(stage_id))
        .and_then(|rest| rest.strip_prefix("` failed"))
    else {
        return false;
    };
    let rest = fix_loops_spent(rest).unwrap_or(rest);
    rest.strip_prefix(": ")
        .is_some_and(|headline| headline.starts_with(said))
}

/// `rest` past the driver's ` after N of M fix loop(s)`, if it opens with it.
fn fix_loops_spent(rest: &str) -> Option<&str> {
    let rest = past_digits(rest.strip_prefix(" after ")?)?;
    let rest = past_digits(rest.strip_prefix(" of ")?)?;
    rest.strip_prefix(" fix loop(s)")
}

/// `text` past the whole number it opens with, if it opens with one.
fn past_digits(text: &str) -> Option<&str> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then_some(&text[end..])
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
/// opens the spawn it gave up on ([`flag_wrapped_spawns`] reads it wrapped).
fn never_spawned(stage: &StageRun) -> bool {
    let StageState::Failed { error } = &stage.state else {
        return false;
    };
    stage.aether_agent.is_none() && gave_up_spawn(error)
}

/// Whether `error` opens as the daemon opens a spawn it gave up on.
fn gave_up_spawn(error: &str) -> bool {
    let openings = [
        StageOutcome::DISK_FULL_SPAWN,
        StageOutcome::SPAWN_KEPT_FAILING,
    ];
    openings.iter().any(|opening| error.starts_with(opening))
}

/// `stages`, each agentless spawn 1.6.3 gave up on in a workspace gone missing,
/// or refused for that workspace before any agent ran, flagged as 1.6.4 flags it. Its error wraps the spawn's: `workspace missing:
/// <path>; the engine said: …`, `<path>` being the run's or a worker's tree.
fn flag_wrapped_spawns(run: &PipelineRun, stages: &[StageRun]) -> Vec<StageRun> {
    let mut flagged = stages.to_vec();
    for stage in flagged.iter_mut().filter(|s| s.aether_agent.is_none()) {
        let wrapped = match &stage.state {
            StageState::Failed { error } => {
                engine_said(run, error).is_some_and(gave_up_spawn) || bare_missing(run, error)
            }
            _ => false,
        };
        stage.infrastructure_failure |= wrapped;
    }
    assert_eq!(
        flagged.len(),
        stages.len(),
        "flags, never drops, a stage run"
    );
    flagged
}

/// Whether `error` is the daemon's bare refusal of the run's or a worker's
/// tree gone missing before a spawn: `workspace missing: <path>`.
fn bare_missing(run: &PipelineRun, error: &str) -> bool {
    let Some(path) = error
        .strip_prefix(StageOutcome::WORKSPACE_MISSING)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return false;
    };
    if path == run.workspace.display().to_string() {
        return true;
    }
    let Some(root) = &run.team_root else {
        return false;
    };
    let worker = path.strip_prefix(&format!("{}/", root.display()));
    worker.is_some_and(worker_dir)
}

/// Whether `dir` is a worker's directory, `<stage>-<role>`, both of `[a-z0-9-]`.
fn worker_dir(dir: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    !dir.is_empty() && dir.bytes().all(allowed)
}

/// The error the daemon wrapped in its missing-workspace error, where the
/// path it names is the run's worktree or a worker's under its team root.
fn engine_said<'a>(run: &PipelineRun, error: &'a str) -> Option<&'a str> {
    let path_on = error
        .strip_prefix(StageOutcome::WORKSPACE_MISSING)?
        .strip_prefix(' ')?;
    let run_tree = run.workspace.display().to_string();
    let in_run_tree = path_on
        .strip_prefix(run_tree.as_str())
        .and_then(|rest| rest.strip_prefix(StageOutcome::ENGINE_SAID));
    if in_run_tree.is_some() {
        return in_run_tree;
    }
    let root = format!("{}/", run.team_root.as_ref()?.display());
    let (worker, said) = path_on
        .strip_prefix(root.as_str())?
        .split_once(StageOutcome::ENGINE_SAID)?;
    worker_dir(worker).then_some(said)
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
