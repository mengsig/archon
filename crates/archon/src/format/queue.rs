//! Queue items: the `archon queue list` rows, and the `archon queue show`
//! page with each stage's summary and the pull request a run opened.

use std::fmt::Write as _;

use archon_core::Timestamp;
use archon_core::protocol::{ItemDetail, ItemSummary, WaitingOn};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{
    self, GateDecision, OperatorGate, PipelineRun, RunState, StageRun, StageState,
};
use archon_core::steer::SteerRecord;

use super::{RepoNames, duration, repo_label, table};
use crate::wrap::{sanitize, sanitize_line};

/// Lines of a stage summary `archon queue show` prints without `--full`.
const SUMMARY_LINES: usize = 4;
/// Characters of a stage summary it prints without `--full`.
const SUMMARY_CHARS: usize = 400;

/// One line per item: id, state, stage, age, cost, repo, pipeline, then the
/// title and the pull request, if there is one.
pub fn items(rows: &[ItemSummary], names: &RepoNames, now: Timestamp) -> String {
    if rows.is_empty() {
        return "The queue is empty.".to_owned();
    }
    let cells: Vec<[String; 8]> = rows.iter().map(|row| item_row(row, names, now)).collect();
    let text = table(&cells);
    debug_assert_eq!(text.lines().count(), rows.len(), "one line per item");
    text
}

fn item_row(row: &ItemSummary, names: &RepoNames, now: Timestamp) -> [String; 8] {
    let item = &row.item;
    let mut last = sanitize_line(&item.title);
    if let Some(url) = &row.pr_url {
        let _ = write!(last, "  {}", sanitize_line(url));
    }
    if let Some(risk) = item.risk {
        let _ = write!(last, "  {}", risk.label());
    }
    if let Some(wait) = &row.waiting_on {
        let _ = write!(
            last,
            "  {} — next: {}",
            sanitize_line(&wait.subject),
            sanitize_line(&wait.next_action)
        );
    }
    if row.waiting_on.is_none()
        && let Some(reason) = &row.waiting_reason
    {
        let _ = write!(
            last,
            "  {} — next: {}",
            sanitize_line(&reason.message),
            sanitize_line(&reason.next_action)
        );
    }
    [
        item.id.to_string(),
        row.waiting_on
            .as_ref()
            .map_or_else(|| item.state.kind(), approval_state)
            .to_owned(),
        row.stage
            .as_deref()
            .map_or_else(|| "-".to_owned(), sanitize_line),
        duration((now - item.created_at).num_seconds()),
        archon_core::cost::format_usd(row.spent_usd),
        repo_label(names, item.repo.as_ref()),
        pipeline_label(item, row.switched_to.as_deref()),
        last,
    ]
}

/// The pipeline the item was queued with, then the one a steer switched it
/// to, if any.
fn pipeline_label(item: &QueueItem, switched_to: Option<&str>) -> String {
    let queued = sanitize_line(&item.pipeline);
    match switched_to {
        Some(to) => format!("{queued} → {}", sanitize_line(to)),
        None => queued,
    }
}

fn state_detail(state: &QueueState) -> String {
    match state {
        QueueState::Blocked { reason } => format!("blocked: {}", sanitize_line(reason)),
        QueueState::Succeeded { summary, .. } => {
            format!("succeeded: {}", sanitize_line(summary))
        }
        QueueState::Failed { error, .. } => format!("failed: {}", plain_line(error)),
        QueueState::Cancelled {
            reason: Some(reason),
        } => format!("cancelled: {}", sanitize_line(reason)),
        other => other.kind().to_owned(),
    }
}

/// A gate opens before its question is asked; until then nothing can be
/// answered, so the item is not yet awaiting approval.
fn approval_state(wait: &WaitingOn) -> &'static str {
    if wait.question.is_some() {
        "awaiting approval"
    } else {
        "preparing approval"
    }
}

/// One line of a failure for plain output: no Markdown emphasis.
fn plain_line(text: &str) -> String {
    sanitize_line(&text.replace("**", ""))
}

/// The item, then each run with its pull request, stages and gates, then
/// the brief. `full` prints every stage summary whole.
pub fn item_detail(detail: &ItemDetail, names: &RepoNames, full: bool) -> String {
    let item = &detail.item;
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", item.id, sanitize_line(&item.title));
    if let Some(wait) = &detail.waiting_on {
        let _ = writeln!(
            out,
            "state: {} ({})",
            approval_state(wait),
            sanitize_line(&wait.subject)
        );
        let _ = writeln!(out, "{}", sanitize_line(&wait.prompt));
        let _ = writeln!(out, "next: {}", sanitize_line(&wait.next_action));
    } else {
        let _ = writeln!(out, "state: {}", state_detail(&item.state));
        if let Some(reason) = &detail.waiting_reason {
            let _ = writeln!(out, "waiting: {}", sanitize_line(&reason.message));
            let _ = writeln!(out, "next: {}", sanitize_line(&reason.next_action));
        }
    }
    let _ = writeln!(
        out,
        "repo: {}, pipeline: {}, priority: {:?}",
        repo_label(names, item.repo.as_ref()),
        pipeline_label(item, detail.switched_to.as_deref()),
        item.priority
    );
    if let Some(initiative) = &item.initiative {
        let _ = writeln!(
            out,
            "initiative: {initiative} (archon initiatives show {initiative})"
        );
    }
    if !item.dependencies.is_empty() {
        let deps: Vec<String> = item.dependencies.iter().map(ToString::to_string).collect();
        let _ = writeln!(out, "depends on: {}", deps.join(", "));
    }
    if let Some(onto) = &item.onto {
        let line = format!("onto: {onto}, frozen at enqueue from {}", onto.target);
        let _ = writeln!(out, "{}", sanitize_line(&line));
    }
    if !item.tags.is_empty() {
        let _ = writeln!(out, "tags: {}", sanitize_line(&item.tags.join(", ")));
    }
    if let Some(risk) = item.risk {
        let _ = writeln!(out, "risk: {risk} ({})", risk.meaning());
    }
    let budget = item
        .budget_usd
        .map_or_else(|| "default".to_owned(), archon_core::cost::format_usd);
    let _ = writeln!(
        out,
        "spent: {} (budget {budget})",
        archon_core::cost::format_usd(detail.spent_usd)
    );
    let _ = writeln!(out, "autonomy: {}", item.autonomy.describe());
    if let Some(provider) = &item.routed_provider {
        let _ = writeln!(out, "routed provider: {}", sanitize_line(provider));
    }
    if let Some(switch) = &item.provider_switch {
        let _ = writeln!(out, "provider switch: {}", sanitize_line(switch));
    }
    plan_lines(&mut out, item);
    for run in &detail.runs {
        run_lines(&mut out, detail, run, full);
    }
    steer_lines(&mut out, &detail.steers);
    let _ = write!(out, "\n{}", sanitize(item.brief.trim_end()));
    out
}

/// `plan implement: model effort (reason)` per stage, for an item whose
/// plan was recorded with reasons.
fn plan_lines(out: &mut String, item: &QueueItem) {
    let Some(reasons) = &item.model_reasons else {
        return;
    };
    for (stage, choices) in &item.model_plan.0 {
        let picks: Vec<_> = choices.iter().map(ToString::to_string).collect();
        let reason = reasons
            .get(stage)
            .map_or(String::new(), |r| format!(" ({})", sanitize_line(r)));
        let _ = writeln!(
            out,
            "plan {}: {}{reason}",
            sanitize_line(stage),
            picks.join(" + ")
        );
    }
}

/// `steered 2026-10-14 09:30 UTC by operator: what changed`, oldest first,
/// each with the note it amended the brief with under it.
fn steer_lines(out: &mut String, steers: &[SteerRecord]) {
    debug_assert!(
        steers.is_sorted_by_key(|steer| steer.at),
        "archond lists steers oldest first"
    );
    for steer in steers {
        let _ = writeln!(
            out,
            "steered {} by {}: {}",
            steer.at.format("%Y-%m-%d %H:%M UTC"),
            steer.by,
            sanitize_line(&steer.what)
        );
        if let Some(note) = &steer.note {
            let _ = writeln!(out, "  note: {}", sanitize_line(note));
        }
    }
}

/// `run …: state on branch in workspace`, its pull request, then its stages
/// (each with its summary under it) and gates, indented.
fn run_lines(out: &mut String, detail: &ItemDetail, run: &PipelineRun, full: bool) {
    assert_eq!(
        run.item, detail.item.id,
        "the run belongs to the item shown"
    );
    let on_branch = match run.branch.is_empty() {
        true => String::new(),
        false => format!(" on {}", run.branch),
    };
    let _ = writeln!(
        out,
        "run {}: {}{on_branch} in {}",
        run.id,
        run_state(&run.state, &detail.item.state),
        run.workspace.display()
    );
    let stages: Vec<&StageRun> = detail.stages.iter().filter(|s| s.run == run.id).collect();
    if let Some(url) = run::pr_url(stages.iter().copied()) {
        let _ = writeln!(out, "  pr: {}", sanitize_line(url));
    }
    for merge in detail.merges.iter().filter(|m| m.run == run.id) {
        let _ = writeln!(out, "  merge: {}", sanitize_line(&merge.line()));
    }
    let retries = run::slot_retries(
        stages
            .iter()
            .map(|s| (s.stage_id.as_str(), s.attempt, s.fan_index)),
    );
    for (stage, retry) in stages.into_iter().zip(retries) {
        let _ = writeln!(out, "  {}", stage_line(stage, retry, &run.state));
        if let Some(delegation) = detail.delegations.get(&stage.id) {
            let _ = writeln!(out, "    cost: {}", delegation.split());
            for agent in &delegation.agents {
                let _ = writeln!(out, "      {}", sanitize_line(&agent.line()));
            }
        }
        for warning in detail.stage_warnings.iter().filter(|w| w.stage == stage.id) {
            let _ = writeln!(out, "    health: {}", sanitize_line(&warning.describe()));
        }
        if let Some(review) = archon_pipeline::review::on_record(stage.structured.as_ref()) {
            let _ = writeln!(out, "    {}", sanitize_line(&review));
        }
        for line in archon_pipeline::review::verdict_lines(stage.structured.as_ref()) {
            let _ = writeln!(out, "      {}", sanitize_line(&line));
        }
        let summary = stage.summary.as_deref().unwrap_or_default();
        for line in summary_lines(summary, full) {
            let _ = writeln!(out, "    {line}");
        }
    }
    for gate in detail.gates.iter().filter(|g| g.run == run.id) {
        let _ = writeln!(out, "  {}", gate_line(gate));
    }
}

/// The run's state; a failure the item's state already printed is not
/// repeated.
fn run_state(state: &RunState, item: &QueueState) -> String {
    let item_failure = match item {
        QueueState::Failed { error, .. } => Some(error.as_str()),
        _ => None,
    };
    match state {
        RunState::Running { stage_id, attempt } => {
            format!("running {stage_id} (attempt {attempt})")
        }
        RunState::AwaitingOperator {
            stage_id, subject, ..
        } => format!(
            "waiting after {stage_id} for the operator to approve the {}",
            subject.noun()
        ),
        RunState::Succeeded { .. } => "succeeded".to_owned(),
        RunState::Failed { stage_id, error } => {
            match item_failure == Some(&format!("stage {stage_id}: {error}")) {
                true => format!("failed at {stage_id}"),
                false => format!("failed at {stage_id}: {}", plain_line(error)),
            }
        }
        RunState::Cancelled { reason } => match reason {
            Some(reason) => format!("cancelled: {}", sanitize_line(reason)),
            None => "cancelled".to_owned(),
        },
    }
}

/// `gate propose #1 (direction): approved; note: …` and where the page is.
fn gate_line(gate: &OperatorGate) -> String {
    let state = match (&gate.decision, &gate.withdrawn) {
        (Some(GateDecision::Approve { note: Some(note) }), _) => format!("approved: {note}"),
        (Some(GateDecision::Approve { note: None }), _) => "approved".to_owned(),
        (Some(GateDecision::Revise { feedback }), _) => format!("revision requested: {feedback}"),
        (Some(GateDecision::Reject { reason }), _) => format!("rejected: {reason}"),
        (None, Some(reason)) => format!("withdrawn: {reason}"),
        (None, None) => "waiting for the operator".to_owned(),
    };
    let state = sanitize_line(&state);
    let page = gate
        .location
        .as_ref()
        .map(|at| format!(" at {}", sanitize_line(at)))
        .unwrap_or_default();
    format!(
        "gate {} #{} ({}{page}): {state}",
        gate.stage_id,
        gate.attempt,
        gate.subject.noun()
    )
}

/// `review attempt 2 slot 1: completed, verdict pass, $0.42, model (effort),
/// agent agt_…`, with ` retry N` after the slot for the slot's `N`th retry.
/// A failure its run already names is not repeated.
fn stage_line(stage: &StageRun, retry: u32, run: &RunState) -> String {
    let run_failure = match run {
        RunState::Failed { error, .. } => Some(error.as_str()),
        _ => None,
    };
    let state = match &stage.state {
        StageState::Spawning => "spawning".to_owned(),
        StageState::Running => "running".to_owned(),
        StageState::Completed => "completed".to_owned(),
        StageState::Failed { error } if run_failure == Some(error.as_str()) => "failed".to_owned(),
        StageState::Failed { error } => format!("failed: {}", plain_line(error)),
        StageState::Cancelled => "cancelled".to_owned(),
        StageState::TimedOut => "timed out".to_owned(),
    };
    let verdict = stage
        .verdict
        .map(|v| format!(", verdict {v:?}").to_lowercase())
        .unwrap_or_default();
    let agent = stage
        .aether_agent
        .as_ref()
        .map_or_else(|| "no agent yet".to_owned(), |a| format!("agent {a}"));
    format!(
        "{} attempt {} slot {}{}: {state}{verdict}, {}, {}, {agent}",
        stage.stage_id,
        stage.attempt,
        stage.fan_index,
        run::retry_label(retry),
        archon_core::cost::format_usd(stage.cost_usd),
        attempt_choice(stage.current_choice()),
    )
}

/// `model (effort)`, and the tier it resolved when it asked for one.
fn attempt_choice(choice: &archon_core::model::ModelChoice) -> String {
    let tier = choice
        .tier
        .as_ref()
        .map_or(String::new(), |t| format!(", {t}"));
    format!("{} ({}{tier})", choice.model, choice.effort)
}

/// A stage summary's lines: all of them with `full`, else the first
/// [`SUMMARY_LINES`] within [`SUMMARY_CHARS`], and a note when clipped.
fn summary_lines(summary: &str, full: bool) -> Vec<String> {
    let clean = sanitize(summary.trim());
    let all: Vec<&str> = clean.lines().collect();
    if full {
        return all.into_iter().map(str::to_owned).collect();
    }
    let mut kept = Vec::new();
    let mut budget = SUMMARY_CHARS;
    let mut cut = false;
    for line in all.iter().take(SUMMARY_LINES) {
        let chars = line.chars().count();
        if chars > budget {
            let head: String = line.chars().take(budget).collect();
            kept.push(format!("{}…", head.trim_end()));
            cut = true;
            break;
        }
        budget -= chars;
        kept.push((*line).to_owned());
    }
    let hidden = all.len() - kept.len();
    if hidden > 0 || cut {
        let more = match hidden {
            0 => String::new(),
            1 => "1 more line; ".to_owned(),
            n => format!("{n} more lines; "),
        };
        kept.push(format!("[{more}--full shows the whole summary]"));
    }
    assert!(
        kept.len() <= SUMMARY_LINES + 1,
        "a clipped summary stays short"
    );
    kept
}

#[cfg(test)]
mod tests {
    use archon_core::aether_ids::AetherAgentId;
    use archon_core::autonomy::Autonomy;
    use archon_core::id::{PipelineRunId, QuestionId, QueueItemId, RepoId, StageRunId};
    use archon_core::model::{Effort, ModelChoice, ModelPlan};
    use archon_core::queue::{Priority, QueueItem};
    use archon_core::run::{GateSubject, Verdict};
    use chrono::TimeZone as _;

    use super::super::tests::assert_inert;
    use super::*;

    fn at(hour: u32, minute: u32) -> Timestamp {
        chrono::Utc
            .with_ymd_and_hms(2026, 9, 29, hour, minute, 0)
            .unwrap()
    }

    fn queue_item(repo: Option<&RepoId>, title: &str, pipeline: &str) -> QueueItem {
        QueueItem {
            onto: None,
            id: QueueItemId::new(),
            repo: repo.cloned(),
            pipeline: pipeline.into(),
            title: title.into(),
            brief: String::new(),
            priority: Priority::Normal,
            dependencies: vec![],
            tags: vec![],
            model_plan: ModelPlan::default(),
            model_reasons: None,
            budget_usd: None,
            risk: None,
            state: QueueState::Queued,
            autonomy: Autonomy::default(),
            origin: None,
            stage_timeouts: Default::default(),
            fresh_retries: 0,
            routed_provider: None,
            provider_switch: None,
            initiative: None,
            created_at: archon_core::now(),
            updated_at: archon_core::now(),
        }
    }

    fn summary(item: QueueItem) -> ItemSummary {
        ItemSummary {
            waiting_on: None,
            waiting_reason: None,
            item,
            stage: None,
            spent_usd: 0.0,
            pr_url: None,
            switched_to: None,
        }
    }

    fn pipeline_run(item: &QueueItem, state: RunState) -> PipelineRun {
        PipelineRun {
            id: PipelineRunId::new(),
            item: item.id.clone(),
            pipeline: "direct".into(),
            shipped: false,
            aether_run: None,
            workspace: "/w".into(),
            team_root: None,
            branch: "archon/x".into(),
            state,
            cost_usd: 0.0,
            started_at: at(9, 0),
            finished_at: None,
        }
    }

    fn stage_run(run: &PipelineRun, stage_id: &str, summary: &str) -> StageRun {
        StageRun {
            id: StageRunId::new(),
            run: run.id.clone(),
            stage_id: stage_id.into(),
            kind: None,
            attempt: 1,
            fan_index: 0,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            choice: ModelChoice {
                model: "claude-code/opus".parse().unwrap(),
                effort: Effort::High,
                tier: None,
            },
            aether_agent: Some(AetherAgentId::parse("agt_one").unwrap()),
            aether_task: None,
            state: StageState::Completed,
            verdict: None,
            summary: Some(summary.into()),
            structured: None,
            cost_usd: 0.5,
            started_at: at(9, 0),
            finished_at: None,
        }
    }

    #[test]
    fn items_show_stage_age_cost_and_pull_request() {
        let repo = RepoId::new();
        let names: RepoNames = [(repo.clone(), "api".to_owned())].into();
        let mut running = summary(queue_item(Some(&repo), "add a greeting", "direct"));
        running.item.state = QueueState::Running {
            run: PipelineRunId::new(),
        };
        running.item.created_at = at(9, 55);
        running.stage = Some("review".into());
        running.spent_usd = 1.2;
        let mut shipped = summary(queue_item(Some(&repo), "fix typo", "adaptive"));
        shipped.item.state = QueueState::Succeeded {
            run: PipelineRunId::new(),
            summary: "done".into(),
        };
        shipped.item.created_at = at(7, 30);
        shipped.spent_usd = 4.1;
        shipped.pr_url = Some("https://github.com/o/r/pull/12".into());
        let (a, b) = (running.item.id.clone(), shipped.item.id.clone());

        let text = items(&[running, shipped], &names, at(10, 0));
        let expected = format!(
            "{a}  running    review  5m      $1.20  api  direct    add a greeting\n\
             {b}  succeeded  -       2h 30m  $4.10  api  adaptive  fix typo  \
             https://github.com/o/r/pull/12"
        );
        assert_eq!(text, expected);
        assert_eq!(items(&[], &names, at(10, 0)), "The queue is empty.");
    }

    #[test]
    fn operator_wait_is_the_primary_status_with_a_prompt_and_action() {
        let mut detail = detail_with_stages();
        detail.item.state = QueueState::Running {
            run: detail.runs[0].id.clone(),
        };
        let wait = archon_core::protocol::WaitingOn {
            subject: "proposal".into(),
            stage: "propose".into(),
            question: Some(QuestionId::new()),
            prompt: format!("Approve this direction?\nSpoof{}", " evidence".repeat(200)),
            next_action: "archon answer q_test approve".into(),
        };
        detail.waiting_on = Some(wait.clone());
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(
            text.lines()
                .nth(1)
                .unwrap()
                .starts_with("state: awaiting approval (proposal)")
        );
        assert_eq!(
            text.lines().nth(2),
            Some(sanitize_line(&wait.prompt).as_str())
        );
        assert_eq!(
            text.lines().nth(3),
            Some("next: archon answer q_test approve")
        );
        let mut row = summary(detail.item.clone());
        row.waiting_on = Some(wait);
        let listing = items(&[row], &RepoNames::new(), archon_core::now());
        assert!(listing.contains("awaiting approval"));
        assert!(listing.contains("proposal — next:"));
        assert!(!listing.contains("evidence"));
        assert!(listing.len() < 300);
        assert!(listing.contains("archon answer q_test approve"));
        assert_eq!(listing.lines().count(), 1);
        let value = serde_json::to_value(&detail).unwrap();
        assert_eq!(value["item"]["state"]["state"], "running");
        assert_eq!(value["waiting_on"]["subject"], "proposal");
    }

    #[test]
    fn a_gate_not_yet_asked_is_preparing_not_awaiting_approval() {
        let mut detail = detail_with_stages();
        detail.item.state = QueueState::Running {
            run: detail.runs[0].id.clone(),
        };
        detail.waiting_on = Some(archon_core::protocol::WaitingOn {
            subject: "report".into(),
            stage: "report".into(),
            question: None,
            prompt: "The change report is being prepared for approval".into(),
            next_action: "wait until `archon questions` lists its question".into(),
        });
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert_eq!(
            text.lines().nth(1),
            Some("state: preparing approval (report)")
        );
        let mut row = summary(detail.item.clone());
        row.waiting_on = detail.waiting_on.clone();
        let listing = items(&[row], &RepoNames::new(), archon_core::now());
        assert!(listing.contains("preparing approval"), "{listing}");
        assert!(!listing.contains("awaiting approval"), "{listing}");
    }

    #[test]
    fn listed_titles_cannot_rewrite_the_screen_or_fake_a_row() {
        let spoof = "fix typo\x1b[2K\x1b[1G\nqi_FAKE  succeeded  api  direct  done";
        let mut row = summary(queue_item(None, spoof, "direct"));
        row.pr_url = Some("https://x/\x1b[2Kpull/1\nqi_FAKE".into());
        let text = items(&[row], &RepoNames::new(), archon_core::now());
        assert_inert(&text);
        assert_eq!(text.lines().count(), 1, "one item, one row: {text:?}");
        assert!(
            text.ends_with(
                "fix typo[2K[1G qi_FAKE  succeeded  api  direct  done  \
                 https://x/[2Kpull/1 qi_FAKE"
            ),
            "{text}"
        );
    }

    fn detail_with_stages() -> ItemDetail {
        let item = queue_item(None, "add a greeting", "direct");
        let run = pipeline_run(
            &item,
            RunState::Succeeded {
                summary: "shipped".into(),
            },
        );
        let long: Vec<String> = (1..=6).map(|n| format!("finding {n}")).collect();
        let mut review = stage_run(&run, "review", &long.join("\n"));
        review.verdict = Some(Verdict::Pass);
        review.fan_index = 1;
        let mut ship = stage_run(&run, "ship", "opened a pull request");
        ship.aether_agent = None;
        ship.structured = Some(serde_json::json!({"pr_url": "https://github.com/o/r/pull/12"}));
        ItemDetail {
            default_efforts: Default::default(),
            steers: Vec::new(),
            waiting_on: None,
            waiting_reason: None,
            stages: vec![review, ship],
            runs: vec![run],
            item,
            spent_usd: 1.0,
            gates: vec![],
            delegations: Default::default(),
            switched_to: None,
            merges: Vec::new(),
            stage_warnings: Vec::new(),
        }
    }

    /// A refused swarm plan goes back to its planner in the same slot; the
    /// second run is labelled its retry, not a second `attempt 1 slot 0`.
    #[test]
    fn a_rerun_of_a_slot_is_labelled_its_retry() {
        let mut detail = detail_with_stages();
        let run = &detail.runs[0];
        let mut first = stage_run(run, "plan", "refused plan");
        first.state = StageState::Failed {
            error: "stage `plan` passed, but the plan is refused".into(),
        };
        let second = stage_run(run, "plan", "fixed plan");
        detail.stages = vec![first, second];
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(
            text.contains("\n  plan attempt 1 slot 0: failed: "),
            "{text}"
        );
        assert!(
            text.contains("\n  plan attempt 1 slot 0 retry 1: completed"),
            "{text}"
        );
    }

    #[test]
    fn a_failure_is_printed_once_and_without_markdown() {
        let mut detail = detail_with_stages();
        let error = "**`implement` slot 0: timed out after 1 minute**";
        detail.runs[0].state = RunState::Failed {
            stage_id: "implement".into(),
            error: error.into(),
        };
        detail.item.state = QueueState::Failed {
            run: Some(detail.runs[0].id.clone()),
            error: format!("stage implement: {error}"),
        };
        let mut implement = stage_run(&detail.runs[0], "implement", "");
        implement.state = StageState::Failed {
            error: error.into(),
        };
        detail.stages = vec![implement];
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert_eq!(
            text.matches("timed out after 1 minute").count(),
            1,
            "{text}"
        );
        assert!(
            text.contains("state: failed: stage implement: `implement` slot 0: timed out"),
            "{text}"
        );
        assert!(
            text.contains(": failed at implement on archon/x in /w\n"),
            "{text}"
        );
        assert!(
            text.contains("implement attempt 1 slot 0: failed, "),
            "{text}"
        );
        assert!(!text.contains("**"), "{text}");
    }

    #[test]
    fn a_run_without_a_branch_names_no_branch() {
        let mut detail = detail_with_stages();
        detail.runs[0].branch.clear();
        let text = item_detail(&detail, &RepoNames::new(), false);
        let run = &detail.runs[0].id;
        assert!(
            text.contains(&format!("run {run}: succeeded in /w\n")),
            "{text}"
        );
        assert!(
            !text.contains("  in /w") && !text.contains(" on  "),
            "{text}"
        );
    }

    #[test]
    fn item_detail_shows_each_stage_its_summary_and_the_pull_request() {
        let detail = detail_with_stages();
        let text = item_detail(&detail, &RepoNames::new(), false);
        let run = &detail.runs[0].id;
        let expected = format!(
            "run {run}: succeeded on archon/x in /w\n\
             \x20 pr: https://github.com/o/r/pull/12\n\
             \x20 review attempt 1 slot 1: completed, verdict pass, $0.50, \
             claude-code/opus (high), agent agt_one\n\
             \x20   finding 1\n\
             \x20   finding 2\n\
             \x20   finding 3\n\
             \x20   finding 4\n\
             \x20   [2 more lines; --full shows the whole summary]\n\
             \x20 ship attempt 1 slot 0: completed, $0.50, claude-code/opus (high), \
             no agent yet\n\
             \x20   opened a pull request\n"
        );
        assert!(text.contains(&expected), "{text}");
        assert!(text.starts_with(&format!("{}  add a greeting\n", detail.item.id)));

        let full = item_detail(&detail, &RepoNames::new(), true);
        assert!(
            full.contains("    finding 4\n    finding 5\n    finding 6\n"),
            "{full}"
        );
        assert!(!full.contains("--full"), "{full}");
    }

    #[test]
    fn the_risk_level_shows_in_the_list_and_the_detail() {
        let mut detail = detail_with_stages();
        detail.item.risk = Some(archon_core::risk::RiskLevel::High);
        let text = item_detail(&detail, &RepoNames::new(), false);
        let said = "\nrisk: high (Needs at least two human reviewers who read the functional code changes.)\n";
        assert!(text.contains(said), "{text}");
        let row = summary(detail.item.clone());
        let listed = items(&[row], &RepoNames::new(), at(10, 0));
        assert!(listed.ends_with("  risk: high"), "{listed}");
    }

    #[test]
    fn a_switched_pipeline_is_named_after_the_one_queued() {
        let mut detail = detail_with_stages();
        detail.switched_to = Some("adaptive\nqi_FAKE".into());
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(
            text.contains("\nrepo: -, pipeline: direct → adaptive qi_FAKE, priority: Normal\n"),
            "{text}"
        );
        let mut row = summary(detail.item.clone());
        row.switched_to = detail.switched_to.clone();
        let listing = items(&[row], &RepoNames::new(), archon_core::now());
        assert!(
            listing.contains("  direct → adaptive qi_FAKE  add a greeting"),
            "{listing}"
        );
        assert_inert(&listing);
    }

    #[test]
    fn item_detail_lists_every_steer_with_who_made_it_and_its_note() {
        use archon_core::steer::{SteerRecord, SteeredBy};
        let at = chrono::Utc
            .with_ymd_and_hms(2026, 10, 14, 9, 30, 0)
            .unwrap();
        let mut detail = detail_with_stages();
        detail.steers = vec![
            SteerRecord {
                at,
                by: SteeredBy::Orchestrator,
                what: "amended the brief".into(),
                note: Some("Use the v2 client.\n\x1b[2Kqi_FAKE".into()),
            },
            SteerRecord {
                at: at + chrono::Duration::minutes(5),
                by: SteeredBy::Operator,
                what: "removed stage `review`".into(),
                note: None,
            },
        ];
        let text = item_detail(&detail, &RepoNames::new(), false);
        let expected = "steered 2026-10-14 09:30 UTC by orchestrator: amended the brief\n\
                        \x20 note: Use the v2 client. [2Kqi_FAKE\n\
                        steered 2026-10-14 09:35 UTC by operator: removed stage `review`\n";
        assert!(text.contains(expected), "{text}");
        assert_inert(&text);
        detail.steers.clear();
        let plain = item_detail(&detail, &RepoNames::new(), false);
        assert!(!plain.contains("steered"), "{plain}");
    }

    /// A stage that delegated shows how its cost splits and to which agents it
    /// went; a declared review shows as Archon holds it, a skip with its reason.
    #[test]
    fn item_detail_shows_a_stages_delegated_cost_and_the_review_on_record() {
        use archon_core::protocol::{DelegatedAgent, StageDelegation};
        let mut detail = detail_with_stages();
        let lead = detail.stages[0].aether_agent.clone().unwrap();
        let helper = |id: &str, effort, cost_usd| DelegatedAgent {
            agent: AetherAgentId::parse(id).unwrap(),
            parent: lead.clone(),
            model: "codex/astra".parse().unwrap(),
            effort,
            cost_usd,
        };
        let agents = vec![
            helper("agt_two", Some(Effort::Medium), 0.3),
            helper("agt_three", None, 0.2),
        ];
        let split = StageDelegation::of(0.5, agents);
        detail.stages[0].cost_usd = split.total_usd();
        detail.stages[0].structured = Some(serde_json::json!({"review": {
            "verdict": "pass", "reviewer_model": "codex/astra", "reviewer_agent": "agt_two"}}));
        detail
            .delegations
            .insert(detail.stages[0].id.clone(), split);
        let text = item_detail(&detail, &RepoNames::new(), false);
        let expected = "\x20 review attempt 1 slot 1: completed, verdict pass, $1.00, \
             claude-code/opus (high), agent agt_one\n\
             \x20   cost: $0.50 own + $0.50 for 2 delegated agent(s)\n\
             \x20     agt_two codex/astra (medium) $0.30\n\
             \x20     agt_three codex/astra (effort unknown) $0.20\n\
             \x20   REVIEW: pass, by codex/astra (agent agt_two)\n\
             \x20   finding 1\n";
        assert!(text.contains(expected), "{text}");

        let skipped = serde_json::json!({"review": {"verdict": "skipped", "reason": "a typo"}});
        detail.stages[0].structured = Some(skipped);
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(text.contains("\n    REVIEW: skipped: a typo\n"), "{text}");
        let unchecked = serde_json::json!({"review": {"verdict": "pass", "reviewer_model": "a/b"}});
        detail.stages[0].structured = Some(unchecked);
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(
            !text.contains("REVIEW:"),
            "an unchecked pass is none: {text}"
        );
    }

    #[test]
    fn item_detail_shows_reasons_only_beside_current_plan_choices() {
        let mut detail = detail_with_stages();
        let choice = detail.stages[0].choice.clone();
        detail
            .item
            .model_plan
            .0
            .insert("review".into(), vec![choice]);
        detail.item.model_reasons = Some([("review".into(), "Check supplied fix".into())].into());
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(
            text.contains("plan review: claude-code/opus high (Check supplied fix)"),
            "{text}"
        );
        assert_eq!(text.matches("Check supplied fix").count(), 1);
        assert!(!text.contains("routed provider"), "{text}");
        detail.item.routed_provider = Some("codex".into());
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(text.contains("\nrouted provider: codex\n"), "{text}");
        detail.item.provider_switch = Some("stage review moved from claude-code to codex".into());
        let text = item_detail(&detail, &RepoNames::new(), false);
        assert!(
            text.contains("\nprovider switch: stage review moved from claude-code to codex\n"),
            "{text}"
        );
    }

    #[test]
    fn long_summary_lines_are_clipped_by_characters() {
        let long = "x".repeat(SUMMARY_CHARS + 50);
        let lines = summary_lines(&format!("short\n{long}"), false);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "short");
        assert_eq!(lines[1].chars().count(), SUMMARY_CHARS - "short".len() + 1);
        assert!(lines[1].ends_with('…'));
        assert_eq!(lines[2], "[--full shows the whole summary]");
        assert_eq!(summary_lines(&format!("short\n{long}"), true)[1], long);
        assert!(summary_lines("  \n ", false).is_empty());
        assert_eq!(summary_lines("one\ntwo", false), ["one", "two"]);
    }

    fn rejected_with_escapes(run: &PipelineRun) -> OperatorGate {
        OperatorGate {
            run: run.id.clone(),
            item: run.item.clone(),
            stage_id: "propose".into(),
            attempt: 1,
            subject: GateSubject::Proposal,
            summary: String::new(),
            page: None,
            location: Some("http://lavish\x1b]8;;http://evil\x07/p/1".into()),
            report: None,
            question: None,
            decision: Some(GateDecision::Reject {
                reason: "no\nrun x: succeeded".into(),
            }),
            withdrawn: None,
            opened_at: archon_core::now(),
            decided_at: None,
        }
    }

    #[test]
    fn item_detail_strips_escapes_from_agent_text() {
        let mut item = queue_item(None, "title\x1b]52;c;cm0gLXJm\x07", "direct");
        item.brief = "line one\x1b[2J\nline two\r".into();
        item.tags = vec!["t\x1b[8m".into()];
        item.state = QueueState::Cancelled {
            reason: Some("gone\u{9b}2J".into()),
        };
        let run = pipeline_run(
            &item,
            RunState::Failed {
                stage_id: "implement".into(),
                error: "boom\x1b[1A\x1b[2K".into(),
            },
        );
        let mut stage = stage_run(&run, "implement", "did it\x1b[2J\nrun x: succeeded\x07");
        stage.structured = Some(serde_json::json!({"pr_url": "https://x\x1b[2K\n/1"}));
        let gate = rejected_with_escapes(&run);
        let detail = ItemDetail {
            default_efforts: Default::default(),
            steers: Vec::new(),
            waiting_on: None,
            waiting_reason: None,
            item,
            runs: vec![run],
            stages: vec![stage],
            spent_usd: 0.0,
            gates: vec![gate],
            delegations: Default::default(),
            switched_to: None,
            merges: Vec::new(),
            stage_warnings: Vec::new(),
        };
        let text = item_detail(&detail, &RepoNames::new(), true);
        assert_inert(&text);
        assert!(text.contains("title]52;c;cm0gLXJm\n"), "{text}");
        assert!(text.contains("state: cancelled: gone2J\n"), "{text}");
        assert!(text.contains("tags: t[8m\n"), "{text}");
        assert!(
            text.contains("failed at implement: boom[1A[2K on"),
            "{text}"
        );
        assert!(text.contains("  pr: https://x[2K /1\n"), "{text}");
        assert!(
            text.contains("    did it[2J\n    run x: succeeded\n"),
            "{text}"
        );
        assert!(
            text.contains("at http://lavish]8;;http://evil/p/1): rejected: no run x: succeeded\n"),
            "{text}"
        );
        assert!(text.ends_with("\nline one[2J\nline two"), "{text}");
    }

    #[test]
    fn gates_say_what_was_decided() {
        let mut gate = OperatorGate {
            run: PipelineRunId::new(),
            item: QueueItemId::new(),
            stage_id: "propose".into(),
            attempt: 1,
            subject: GateSubject::Proposal,
            summary: "Use a cache".into(),
            page: None,
            location: Some("http://lavish/p/1".into()),
            report: None,
            question: None,
            decision: None,
            withdrawn: None,
            opened_at: archon_core::now(),
            decided_at: None,
        };
        let waiting = gate_line(&gate);
        assert_eq!(
            waiting,
            "gate propose #1 (direction at http://lavish/p/1): waiting for the operator"
        );
        gate.decision = Some(GateDecision::Revise {
            feedback: "no cache".into(),
        });
        assert!(gate_line(&gate).ends_with("): revision requested: no cache"));
    }
}
