//! Deciding one finished stage group (ARCHITECTURE §6.4 step 2): did the
//! gate pass, and if not, what feedback goes back to the looped-to stage.

use archon_core::catalog::{Gate, StageKind};
use archon_core::run::{RecordRepair, StageOutcome, StageRun, StageState, Verdict};

use crate::review::Evidence;
use crate::ship::ShipRecord;
use crate::spec::{Owed, Scenarios, StageSpec};

/// Feedback handed to a looped-to stage is capped so a verbose reviewer
/// cannot blow up the next agent's objective.
pub const MAX_FEEDBACK_CHARS: usize = 32_000;

const SHIP_REFUSAL: &str = "the ship record archond delivers is not on record";
const REVIEW_REFUSAL: &str = archon_core::run::REVIEW_NOT_ON_RECORD;
/// Most characters of an unreadable verdict quoted in a gate's refusal.
const MAX_QUOTED_VERDICT_CHARS: usize = 160;

/// The refusal a failed run's `error` reports when the run failed only
/// because the gate refused the ship record or review a finished stage owes;
/// `None` for every other failure. The error is `stage `id` failed[..]: {headline}`.
pub fn record_refusal(error: &str) -> Option<&str> {
    let headline_block = error.split("\n\n").next().unwrap_or(error);
    [SHIP_REFUSAL, REVIEW_REFUSAL].iter().find_map(|marker| {
        let at = headline_block.find(marker)?;
        let prefix = &headline_block[..at];
        let is_stage_failure =
            prefix.starts_with("stage `") && prefix.contains("` failed") && prefix.ends_with(": ");
        is_stage_failure.then(|| headline_block[at..].trim())
    })
}

/// Whether `refusal`, as [`record_refusal`] gives it, refuses the ship
/// record rather than the review: the work and its review may well stand.
pub fn refuses_ship_record(refusal: &str) -> bool {
    refusal.starts_with(SHIP_REFUSAL)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GroupResult {
    /// Summary of the group: the completed members' summaries joined.
    Pass { summary: String },
    /// `headline` is one line; `report` details every failing member.
    Fail { headline: String, report: String },
    /// A completed stage handed in a broken delivery record: no `structured`
    /// object, or a review that names no reviewer. One repair turn may fix it.
    Unrecorded { headline: String, report: String },
    /// A strict stage's scenario list fell short: a product defect, which no
    /// loop back to an earlier stage can fix, so the run fails.
    Defect { headline: String, report: String },
}

/// Evaluate a stage group whose every slot is terminal. `members` holds the
/// latest stage run per slot, by fan index; `owed` as for `recorded`. The
/// order is fixed: a stated failure fails on its merits, whatever its gate
/// or record; then a broken record is `Unrecorded`, before the gate and
/// scenarios that would read it; then the gate, scenarios and record. An
/// outcome from before repair turns keeps the order that decided it then:
/// gate, scenarios, record, with no stated failure read.
pub(crate) fn evaluate(
    stage: &StageSpec,
    attempt: u32,
    members: &[&StageRun],
    owed: Owed,
) -> GroupResult {
    assert_eq!(members.len(), stage.fan_out as usize, "one member per slot");
    assert!(
        members.iter().all(|m| m.state.is_terminal()),
        "only finished groups are evaluated"
    );
    if owed != Owed::default()
        && members.iter().all(|m| decided_since_repairs(m))
        && repairable(stage, members, owed)
        && let unrecorded @ GroupResult::Unrecorded { .. } = recorded(stage, attempt, members, owed)
    {
        return unrecorded;
    }
    let total = members.len();
    let passes = members.iter().filter(|m| member_passes(stage, m)).count();
    let ok = match stage.gate {
        Gate::None | Gate::AllPass => passes == total,
        Gate::Majority => passes * 2 > total,
        Gate::Any => passes >= 1,
    };
    if ok
        && stage.scenarios == Scenarios::Strict
        && let Some(defect) = scenario_defect(stage, attempt, members)
    {
        return defect;
    }
    if ok && owed != Owed::default() {
        return recorded(stage, attempt, members, owed);
    }
    if ok {
        return GroupResult::Pass {
            summary: joined_summary(stage, members),
        };
    }
    let headline = match stage.gate {
        Gate::None if stage.kind == StageKind::Team => team_headline(stage, members),
        Gate::None => format!("{} of {total} agent(s) did not complete", total - passes),
        gate => format!(
            "gate {} failed: {passes} of {total} passed",
            gate_name(gate)
        ),
    };
    // Bold labels, not headings: feedback is embedded at any heading depth.
    let mut report = format!("**Stage `{}` attempt {attempt}: {headline}**\n", stage.id);
    for member in members.iter().filter(|m| !member_passes(stage, m)) {
        report.push('\n');
        report.push_str(&member_report(stage, member));
    }
    GroupResult::Fail {
        headline,
        report: clip(&report, MAX_FEEDBACK_CHARS),
    }
}

/// The strict stage's scenario shortfall, when its passing member has one.
fn scenario_defect(stage: &StageSpec, attempt: u32, members: &[&StageRun]) -> Option<GroupResult> {
    let [member] = members else {
        panic!("validated: a strict scenario stage has one agent")
    };
    assert_eq!(member.state, StageState::Completed, "it passed its gate");
    let short = crate::scenarios::shortfall(member.structured.as_ref())?;
    let summary = member.summary.as_deref().unwrap_or_default().trim();
    let report = format!(
        "**Stage `{}` attempt {attempt}: {}**\n\n{}\n\n{summary}\n",
        stage.id, short.headline, short.detail
    );
    Some(GroupResult::Defect {
        headline: short.headline,
        report: clip(&report, MAX_FEEDBACK_CHARS),
    })
}

/// A stage that passed its gate passes only with what it owes on record: the
/// ship record archond delivers, and the review, whose line then ends its summary.
fn recorded(stage: &StageSpec, attempt: u32, members: &[&StageRun], owed: Owed) -> GroupResult {
    let [member] = members else {
        panic!("validated: one agent holds a pipeline's review or ship record")
    };
    assert_eq!(member.state, StageState::Completed, "it passed its gate");
    let summary = joined_summary(stage, members);
    let structured = member.structured.as_ref();
    // A stated failure is refused on its merits, whatever its gate or record.
    if decided_since_repairs(member) && states_failure(member) {
        let headline = "its agent stated a failure".to_owned();
        let report = format!(
            "**Stage `{}` attempt {attempt}: {headline}**\n\n{summary}\n",
            stage.id
        );
        let report = clip(&report, MAX_FEEDBACK_CHARS);
        return GroupResult::Fail { headline, report };
    }
    let broken = repairable(stage, members, owed);
    let refused = |headline: String, unrecorded: bool| {
        let report = format!(
            "**Stage `{}` attempt {attempt}: {headline}**\n\n{summary}\n",
            stage.id
        );
        let report = clip(&report, MAX_FEEDBACK_CHARS);
        match unrecorded {
            true => GroupResult::Unrecorded { headline, report },
            false => GroupResult::Fail { headline, report },
        }
    };
    if owed.ship_record && crate::ship::no_change_reason(structured).is_some() {
        return GroupResult::Pass { summary };
    }
    if owed.ship_record
        && let Err(why) = ShipRecord::read(structured)
    {
        // A repair that still hands in no valid ship record has none to loop on.
        let spent = member.record_repair == Some(RecordRepair::Spent);
        return refused(format!("{SHIP_REFUSAL}: {why}"), broken || spent);
    }
    let Some(duty) = owed.review else {
        return GroupResult::Pass { summary };
    };
    match Evidence::read(structured, duty) {
        Ok(evidence) => GroupResult::Pass {
            summary: format!("{summary}\n\n{}", evidence.line()),
        },
        Err(why) => refused(format!("{REVIEW_REFUSAL}: {why}"), broken),
    }
}

/// Whether the one agent holding a record completed with a broken record.
/// [`recorded`] refuses a stated fail on its merits before any repair.
fn repairable(stage: &StageSpec, members: &[&StageRun], owed: Owed) -> bool {
    let [member] = members else {
        return false;
    };
    assert_eq!(stage.fan_out, 1, "one agent holds a record");
    member.state == StageState::Completed && broken_record(member.structured.as_ref(), owed)
}

/// Whether `member`'s outcome was recorded since stages have repair turns:
/// one from before is replayed by the rules that decided it then.
fn decided_since_repairs(member: &StageRun) -> bool {
    assert!(member.state.is_terminal(), "only an outcome is decided");
    member.record_repair.is_some()
}

/// Whether a reply of a stage that owes a record is decided as sent, unverified:
/// it states a failure or has no `structured`, neither of which the gate passes.
pub(crate) fn decided_as_sent(
    owed: Owed,
    summary: &str,
    structured: Option<&serde_json::Value>,
) -> bool {
    if owed == Owed::default() {
        return false;
    }
    structured.is_none() || StageOutcome::states_failure(summary, structured)
}

/// Whether `member` stated its failure anywhere, even beside a pass
/// elsewhere: in the reply as its agent sent it, or its persisted verdict.
fn states_failure(member: &StageRun) -> bool {
    let summary = member.summary.as_deref().unwrap_or_default();
    let stated = StageOutcome::states_failure(summary, member.structured.as_ref());
    stated || member.verdict == Some(Verdict::Fail)
}

/// Whether a completed stage's record is broken rather than refused on its
/// merits: it has no `structured` object, or its owed review names no reviewer.
fn broken_record(structured: Option<&serde_json::Value>, owed: Owed) -> bool {
    assert!(owed != Owed::default(), "only an owing stage has a record");
    let Some(structured) = structured else {
        return true;
    };
    let reviews = owed.review.is_some_and(|duty| duty.reviewers > 0);
    reviews && crate::review::names_no_reviewer(structured)
}

/// Which roles of a team group did not complete, and how.
fn team_headline(stage: &StageSpec, members: &[&StageRun]) -> String {
    let outcomes: Vec<String> = members
        .iter()
        .filter(|m| m.state != StageState::Completed)
        .map(|m| role_outcome(stage, m))
        .collect();
    assert!(
        !outcomes.is_empty(),
        "a failed team group has a failed role"
    );
    outcomes.join(", ")
}

/// How one team role ended, e.g. "role `api` failed".
pub(crate) fn role_outcome(stage: &StageSpec, member: &StageRun) -> String {
    let role = stage
        .roles
        .get(member.fan_index as usize)
        .expect("validated: one role per team slot");
    let how = match &member.state {
        StageState::Completed => "completed",
        StageState::Failed { .. } => "failed",
        StageState::TimedOut => "timed out",
        StageState::Cancelled => "was cancelled",
        StageState::Spawning | StageState::Running => "is still running",
    };
    format!("role `{role}` {how}")
}

/// A slot passes when its agent completed and, for gated stages, its
/// verdict is `pass`. `unknown` counts as fail.
fn member_passes(stage: &StageSpec, member: &StageRun) -> bool {
    if member.state != StageState::Completed {
        return false;
    }
    stage.gate == Gate::None || verdict_of(member) == Verdict::Pass
}

/// The persisted verdict, else the one [`Verdict::extract`] reads from the
/// summary and structured result.
pub(crate) fn verdict_of(member: &StageRun) -> Verdict {
    member.verdict.unwrap_or_else(|| {
        Verdict::extract(
            member.summary.as_deref().unwrap_or_default(),
            member.structured.as_ref(),
        )
    })
}

pub(crate) fn gate_name(gate: Gate) -> &'static str {
    match gate {
        Gate::None => "none",
        Gate::AllPass => "all_pass",
        Gate::Majority => "majority",
        Gate::Any => "any",
    }
}

/// How a slot is named in summaries: its role on a team stage, else `#n`.
fn slot_label(stage: &StageSpec, fan_index: u32) -> String {
    match stage.roles.get(fan_index as usize) {
        Some(role) => format!("role {role}"),
        None => format!("#{}", fan_index + 1),
    }
}

/// Completed members' summaries; fan-out slots are labelled `[#n]`, team
/// slots `[role <name>]`.
pub(crate) fn joined_summary(stage: &StageSpec, members: &[&StageRun]) -> String {
    let completed: Vec<&StageRun> = members
        .iter()
        .copied()
        .filter(|m| m.state == StageState::Completed)
        .collect();
    let text = |m: &StageRun| m.summary.as_deref().unwrap_or_default().trim().to_owned();
    if members.len() == 1 {
        return completed.first().map(|m| text(m)).unwrap_or_default();
    }
    completed
        .iter()
        .map(|m| format!("[{}] {}", slot_label(stage, m.fan_index), text(m)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn member_report(stage: &StageSpec, member: &StageRun) -> String {
    let label = format!("**`{}` {}", stage.id, slot_label(stage, member.fan_index));
    let summary = member.summary.as_deref().unwrap_or_default().trim();
    match &member.state {
        StageState::Completed => {
            let verdict = match verdict_of(member) {
                Verdict::Fail => "fail".to_owned(),
                _ => unreadable_verdict(member),
            };
            let findings = findings_text(member.structured.as_ref());
            format!("{label}: verdict {verdict}**\n\n{summary}\n{findings}")
        }
        StageState::Failed { error } => format!("{label}: failed**\n\n{}\n", error.trim()),
        StageState::TimedOut => format!("{label}: timed out after {}**\n", ran_for(member)),
        StageState::Cancelled if summary.is_empty() => {
            format!("{label}: cancelled before it finished**\n")
        }
        StageState::Cancelled => format!("{label}: cancelled before it finished**\n\n{summary}\n"),
        StageState::Spawning | StageState::Running => {
            unreachable!("evaluate() asserts every member is terminal")
        }
    }
}

/// Why a completed member's verdict counts as fail, quoting what it said:
/// a pass beside a blocking finding, or words that read as no verdict.
fn unreadable_verdict(member: &StageRun) -> String {
    let summary = member.summary.as_deref().unwrap_or_default();
    let Some(said) = Verdict::stated(summary, member.structured.as_ref()) else {
        return "missing (counts as fail)".to_owned();
    };
    let quoted: String = said.chars().take(MAX_QUOTED_VERDICT_CHARS).collect();
    let structured = member.structured.as_ref();
    let worded = structured.and_then(Verdict::as_worded);
    let worded_pass = worded.or_else(|| Verdict::from_summary(summary)) == Some(Verdict::Pass);
    if worded_pass && structured.is_some_and(Verdict::lists_blocking) {
        return format!("{quoted:?} beside a blocking finding (counts as fail)");
    }
    format!("{quoted:?} unreadable as pass or fail (counts as fail)")
}

/// How long the member ran, from its own record: the deadline it had may be
/// an item's override, not the pipeline's `timeout_minutes`.
fn ran_for(member: &StageRun) -> String {
    match member.finished_at {
        Some(end) => match (end - member.started_at).num_minutes() {
            1 => "1 minute".to_owned(),
            minutes => format!("{minutes} minutes"),
        },
        None => "its deadline".to_owned(),
    }
}

/// `structured.findings` as a Markdown list; tolerant of missing fields
/// because agents write it by hand.
fn findings_text(structured: Option<&serde_json::Value>) -> String {
    let Some(findings) = structured
        .and_then(|s| s.get("findings"))
        .and_then(|f| f.as_array())
        .filter(|f| !f.is_empty())
    else {
        return String::new();
    };
    let field = |f: &serde_json::Value, key: &str| {
        f.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut out = String::from("\nFindings:\n");
    for finding in findings {
        let severity = field(finding, "severity");
        let path = field(finding, "path");
        let mut line = format!("- [{}] {}", or_dash(&severity), field(finding, "title"));
        if !path.is_empty() {
            line.push_str(&format!(" ({path})"));
        }
        out.push_str(&line);
        out.push('\n');
        let detail = field(finding, "detail");
        if !detail.is_empty() {
            out.push_str(&format!("  {}\n", detail.replace('\n', "\n  ")));
        }
    }
    out
}

fn or_dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

pub(crate) use archon_core::run::clip;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_refusal_names_only_record_refusals() {
        let review =
            format!("stage `implement` failed: {REVIEW_REFUSAL}: no review object\n\nreport");
        assert_eq!(
            record_refusal(&review),
            Some("the review this pipeline declares is not on record: no review object")
        );
        assert!(!refuses_ship_record(record_refusal(&review).unwrap()));
        let ship = format!("stage `a` failed after 2 of 2 fix loop(s): {SHIP_REFUSAL}: bad tree");
        assert!(record_refusal(&ship).unwrap().ends_with("bad tree"));
        assert!(refuses_ship_record(record_refusal(&ship).unwrap()));
        assert_eq!(
            record_refusal("stage `a` failed: 1 of 1 agent(s) did not complete"),
            None
        );
        let quoted =
            format!("stage `a` failed: gate any failed: the agent wrote \"{REVIEW_REFUSAL}\"");
        assert_eq!(record_refusal(&quoted), None);
    }

    #[test]
    fn clip_keeps_short_text_and_cuts_on_char_boundaries() {
        assert_eq!(clip("abc", 3), "abc");
        let cut = clip("ééé", 2);
        assert!(cut.starts_with("éé\n"), "{cut}");
        assert!(cut.ends_with("[… truncated by Archon]"));
    }

    #[test]
    fn findings_render_with_missing_fields() {
        let structured = serde_json::json!({"findings": [
            {"severity": "high", "title": "off by one", "path": "src/a.rs:3", "detail": "loop\nends early"},
            {"title": "nit"}
        ]});
        let text = findings_text(Some(&structured));
        assert!(text.contains("- [high] off by one (src/a.rs:3)"), "{text}");
        assert!(text.contains("  loop\n  ends early"), "{text}");
        assert!(text.contains("- [-] nit"), "{text}");
        assert_eq!(findings_text(None), "");
    }

    /// A completed review slot that returned `summary` and `structured`.
    fn completed(summary: &str, structured: serde_json::Value) -> StageRun {
        serde_json::from_value(serde_json::json!({
            "id": "stg_01M58B56K8HTQT169S0FYFQWDP",
            "run": "prun_01M58B56J7J5HAT01706PQRAVW",
            "stage_id": "review", "kind": "review", "attempt": 1, "fan_index": 0,
            "choice": {"model": {"provider": "claude-code", "model": "claude-sonnet-5-5"}, "effort": "medium"},
            "infrastructure_failure": false, "host_context": "full",
            "state": {"state": "completed"}, "cost_usd": 0.0,
            "started_at": "2026-10-18T20:27:24Z", "finished_at": "2026-10-18T20:57:24Z",
            "summary": summary, "structured": structured
        }))
        .unwrap()
    }

    #[test]
    fn a_fail_word_fails_the_gate_and_only_a_pass_is_named_beside_its_blocker() {
        let blocking = serde_json::json!([{"title": "data loss", "blocking": true}]);
        let said = |word: &str| serde_json::json!({"verdict": word, "findings": blocking});
        let rejected = completed("Reviewed.", said("changes_requested"));
        assert_eq!(verdict_of(&rejected), Verdict::Fail);
        let line = completed(
            "VERDICT: needs work",
            serde_json::json!({"findings": blocking}),
        );
        assert_eq!(verdict_of(&line), Verdict::Fail);

        let passed = completed("Reviewed.", said("approved"));
        assert_eq!(verdict_of(&passed), Verdict::Unknown);
        let why = unreadable_verdict(&passed);
        assert_eq!(
            why,
            "\"approved\" beside a blocking finding (counts as fail)"
        );
        let lined = completed("VERDICT: PASS", serde_json::json!({"findings": blocking}));
        assert!(unreadable_verdict(&lined).contains("beside a blocking finding"));

        let unclear = completed("Reviewed.", said("maybe"));
        assert_eq!(verdict_of(&unclear), Verdict::Unknown);
        let why = unreadable_verdict(&unclear);
        assert_eq!(why, "\"maybe\" unreadable as pass or fail (counts as fail)");
    }

    #[test]
    fn timed_out_report_names_the_time_the_stage_ran_not_the_pipeline_default() {
        let member: StageRun = serde_json::from_value(serde_json::json!({
            "id": "stg_01M58B56K8HTQT169S0FYFQWDP",
            "run": "prun_01M58B56J7J5HAT01706PQRAVW",
            "stage_id": "implement", "kind": "work", "attempt": 1, "fan_index": 0,
            "choice": {"model": {"provider": "claude-code", "model": "claude-sonnet-5-5"}, "effort": "medium"},
            "infrastructure_failure": false, "host_context": "full",
            "state": {"state": "timed_out"}, "cost_usd": 0.0,
            "started_at": "2026-10-18T20:27:24Z", "finished_at": "2026-10-18T22:57:24Z"
        }))
        .unwrap();
        assert_eq!(ran_for(&member), "150 minutes");
        let mut short = member.clone();
        short.finished_at = Some("2026-10-18T20:28:54Z".parse().unwrap());
        assert_eq!(ran_for(&short), "1 minute");
    }
}
