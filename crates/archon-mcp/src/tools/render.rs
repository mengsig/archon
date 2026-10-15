//! Replies as text for the model. Aether hands native agents only the text
//! half of a tool result, so every id, state and reason the orchestrator
//! acts on is written out; only long free text is clipped, and says so.

mod artifacts;
mod cost;
mod initiatives;
mod lessons;
mod stages;

use std::fmt::Write as _;

use archon_core::autonomy::{AutonomyView, Directive};
use archon_core::catalog::{CatalogListing, Gate, PipelineSummary, StageSummary};
use archon_core::github::GithubWatch;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::model::{ModelPlan, ModelReasons};
use archon_core::notify::Question;
use archon_core::policy::ModelPolicy;
use archon_core::protocol::{
    AetherLinkStatus, DaemonStatus, DiskStatus, ItemDetail, Reply, ReportPublished, gib,
};
use archon_core::queue::{Caps, QueueItem, QueueState};
use archon_core::repo::Repo;
use archon_core::run::{OperatorGate, PipelineRun, RunState, StageRun, StageState};

/// Longest brief, stage summary or error quoted in full.
const CLIP_CHARS: usize = 1_500;
/// Reasons shown on one listing line.
const LINE_CHARS: usize = 200;

pub(super) fn reply(tool: &str, reply: &Reply) -> String {
    match reply {
        Reply::MessageReceipt { receipt } => format!(
            "Recorded message {receipt} for delivery to the orchestrator. Keep working on your stage; do not wait for a reply or end your turn."
        ),
        Reply::Ok => ok(tool).to_owned(),
        Reply::Status(status) => daemon_status(status),
        Reply::Repo(repo) => format!("{}{}", verb(tool), repo_line(repo)),
        Reply::Repos(repos) => listing("repositories", repos.iter().map(repo_line)),
        Reply::Item(item) => format!("{}{}", verb(tool), item_line(item, CLIP_CHARS)),
        Reply::ItemBudgetsOff(item) => format!(
            "{}{}\nNote: {}",
            verb(tool),
            item_line(item, CLIP_CHARS),
            archon_core::protocol::BUDGETS_OFF_NOTE
        ),
        Reply::Items(items) => listing("items", items.iter().map(|i| item_line(i, LINE_CHARS))),
        Reply::ItemDetail(detail) => item_detail(detail),
        Reply::ItemSummaries(rows) => item_summaries(rows),
        Reply::StageDetail(detail) => stages::detail(detail),
        Reply::Catalog(catalog) => catalog_text(catalog),
        Reply::ModelPolicy(policy) => policy_text(policy),
        Reply::Question(question) => question_text(tool, question),
        Reply::Report(report) => report_text(report),
        Reply::Autonomy(view) => autonomy_text(view),
        Reply::Directive(directive) => format!(
            "Recorded directive {}{}: {}. Every trigger restates it until it is cleared.",
            directive.id,
            until(directive),
            directive.text
        ),
        Reply::Cleared(count) => format!("Cleared {count} directive(s)."),
        Reply::GithubComment { url } => format!("Posted {url}"),
        Reply::CostReport(report) => cost::report(report),
        Reply::ModelEvidence(report) => report.text(),
        Reply::Initiatives(all) => initiatives::list(all),
        Reply::Initiative(detail) => initiatives::detail(tool, detail),
        Reply::Lesson(lesson) => lessons::recorded(lesson),
        Reply::Artifacts(listing) => artifacts::listing(listing),
        Reply::ArtifactPage(page) => artifacts::page(page),
        Reply::Questions(_)
        | Reply::Lessons(_)
        | Reply::LessonsWhy(_)
        | Reply::LessonStats(_)
        | Reply::Feedback(_)
        | Reply::LearningWhy(_)
        | Reply::Reports(_)
        | Reply::Welcome(_)
        | Reply::Subscribed { .. }
        | Reply::Feed(_)
        | Reply::Orchestrator(_)
        | Reply::GithubWatcher(_)
        | Reply::Gc(_) => serde_json::to_string(reply)
            .unwrap_or_else(|e| format!("(reply could not be encoded: {e})")),
    }
}

fn item_summaries(rows: &[archon_core::protocol::ItemSummary]) -> String {
    listing(
        "items",
        rows.iter().map(|row| {
            let mut line = item_line(&row.item, LINE_CHARS);
            switched(&mut line, row.switched_to.as_deref());
            if let Some(reason) = &row.waiting_reason {
                let _ = write!(
                    line,
                    "; waiting {}: {:?}; next: {:?}",
                    snake(&reason.code),
                    reason.message,
                    reason.next_action
                );
            }
            line
        }),
    )
}

fn question_text(tool: &str, question: &Question) -> String {
    if tool == "record_chat_answer" {
        return format!(
            "Recorded the operator's chat answer to Q{} ({}); it is no longer an open question.",
            question.number, question.id
        );
    }
    format!(
        "Asked {}. The answer will arrive as an operator_answer trigger with this id; keep \
         working meanwhile.",
        question.id
    )
}

fn ok(tool: &str) -> &'static str {
    match tool {
        "pause" => "Paused. Nothing new starts in that scope until resume.",
        "resume" => "Resumed.",
        "notify" => "Posted to the operator's chat.",
        "cancel_item" => "Cancelled.",
        "update_item" => "Updated.",
        "retry_item" => "Re-queued.",
        _ => "Done.",
    }
}

fn verb(tool: &str) -> &'static str {
    match tool {
        "enqueue" => "Enqueued ",
        "register_repo" => "Registered ",
        "update_item" => "Updated ",
        "cancel_item" => "Cancelled ",
        "retry_item" => "Re-queued ",
        _ => "",
    }
}

fn listing(noun: &str, lines: impl ExactSizeIterator<Item = String>) -> String {
    if lines.len() == 0 {
        return format!("No {noun}.");
    }
    let mut text = format!("{} {noun}:", lines.len());
    for line in lines {
        let _ = write!(text, "\n- {line}");
    }
    text
}

fn repo_line(repo: &Repo) -> String {
    let base = repo.base_branch.as_deref().unwrap_or("HEAD");
    let pipeline = repo.default_pipeline.as_deref().unwrap_or("none");
    let ship = serde_json::to_value(repo.ship)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let ceiling = match repo.autonomy_max.is_empty() {
        true => "the global one".to_owned(),
        false => repo.autonomy_max.describe(),
    };
    format!(
        "repo {} ({}) at {}; ship {ship}; base {base}; default pipeline {pipeline}; autonomy \
         ceiling {ceiling}",
        repo.name,
        repo.id,
        repo.path.display(),
    )
}

fn item_line(item: &QueueItem, reason_chars: usize) -> String {
    let mut line = format!(
        "{} [{}] {:?} pipeline {}, priority {}",
        item.id,
        item.state.kind(),
        item.title,
        item.pipeline,
        snake(&item.priority)
    );
    if let Some(repo) = &item.repo {
        let _ = write!(line, ", repo {repo}");
    }
    if !item.dependencies.is_empty() {
        let deps: Vec<String> = item.dependencies.iter().map(ToString::to_string).collect();
        let _ = write!(line, ", depends on {}", deps.join(", "));
    }
    if !item.tags.is_empty() {
        let _ = write!(line, ", tags {}", item.tags.join(", "));
    }
    if let Some(detail) = state_detail(&item.state) {
        let _ = write!(line, "; {}", clip(&detail, reason_chars));
    }
    line
}

/// Name the catalog pipeline a steer switched the item to, if one did.
fn switched(line: &mut String, to: Option<&str>) {
    if let Some(to) = to {
        let _ = write!(line, "; a steer switched it to pipeline {to}");
    }
}

fn state_detail(state: &QueueState) -> Option<String> {
    match state {
        QueueState::Running { run } => Some(format!("run {run}")),
        QueueState::Blocked { reason } => Some(format!("blocked: {reason}")),
        QueueState::Succeeded { run, summary } => Some(format!("run {run}: {summary}")),
        QueueState::Failed {
            run: Some(run),
            error,
        } => Some(format!("run {run} failed: {error}")),
        QueueState::Failed { run: None, error } => Some(format!("failed: {error}")),
        QueueState::Cancelled {
            reason: Some(reason),
        } => Some(format!("cancelled: {reason}")),
        QueueState::Queued | QueueState::Dispatching | QueueState::Cancelled { reason: None } => {
            None
        }
    }
}

fn item_detail(detail: &ItemDetail) -> String {
    let item = &detail.item;
    let budget = item
        .budget_usd
        .map_or("the configured default".to_owned(), usd);
    let mut text = item_line(item, CLIP_CHARS);
    switched(&mut text, detail.switched_to.as_deref());
    if let Some(reason) = &detail.waiting_reason {
        let _ = write!(
            text,
            "\nWaiting {}: {:?}\nNext: {:?}",
            snake(&reason.code),
            reason.message,
            reason.next_action
        );
    }
    let _ = write!(
        text,
        "\nSpent {} (budget {budget}). Created {}, updated {}.\nBrief: {}\nModel plan: {}\n\
         Autonomy: {}",
        usd(detail.spent_usd),
        item.created_at.to_rfc3339(),
        item.updated_at.to_rfc3339(),
        clip(&item.brief, CLIP_CHARS),
        plan_text(&item.model_plan, item.model_reasons.as_ref()),
        item.autonomy.describe()
    );
    for gate in &detail.gates {
        let _ = write!(text, "\nGate {}", gate_line(gate));
    }
    let latest = detail.runs.last().map(|r| &r.id);
    for run in &detail.runs {
        let _ = write!(text, "\nRun {}", run_line(run));
        // get_stage reads the latest run unless told which.
        let older = (Some(&run.id) != latest).then_some(&run.id);
        for stage in detail.stages.iter().filter(|s| s.run == run.id) {
            let _ = write!(text, "\n  - {}", stage_line(&item.id, older, stage));
            if let Some(delegation) = detail.delegations.get(&stage.id) {
                let _ = write!(text, "\n    cost split: {}", delegation.line());
            }
        }
    }
    if detail.runs.is_empty() {
        text.push_str("\nNo pipeline run yet.");
    }
    for steer in &detail.steers {
        let at = steer.at.to_rfc3339();
        let _ = write!(text, "\nSteered {at} by {}: {}", steer.by, steer.what);
        if let Some(note) = &steer.note {
            let _ = write!(text, " Note: {}", clip(note, CLIP_CHARS));
        }
    }
    text
}

fn plan_text(plan: &ModelPlan, reasons: Option<&ModelReasons>) -> String {
    if plan.0.is_empty() {
        return "(empty)".to_owned();
    }
    let stages: Vec<String> = plan
        .0
        .iter()
        .map(|(stage, choices)| {
            let picks: Vec<String> = choices
                .iter()
                .map(|c| format!("{} {}", c.model, c.effort))
                .collect();
            let reason = reasons
                .and_then(|r| r.get(stage))
                .map_or(String::new(), |r| format!(" ({r})"));
            format!("{stage} = {}{reason}", picks.join(" + "))
        })
        .collect();
    stages.join("; ")
}

fn run_line(run: &PipelineRun) -> String {
    let state = match &run.state {
        RunState::Running { stage_id, attempt } => {
            format!("running stage {stage_id} (attempt {attempt})")
        }
        RunState::AwaitingOperator {
            stage_id,
            attempt,
            subject,
        } => format!(
            "waiting for the operator to approve the {} of stage {stage_id} (attempt {attempt})",
            subject.noun()
        ),
        RunState::Succeeded { summary } => format!("succeeded: {}", clip(summary, CLIP_CHARS)),
        RunState::Failed { stage_id, error } => {
            format!("failed in stage {stage_id}: {}", clip(error, CLIP_CHARS))
        }
        RunState::Cancelled { reason } => {
            format!(
                "cancelled: {}",
                reason.as_deref().unwrap_or("no reason given")
            )
        }
    };
    format!(
        "{} [{state}] branch {} in {}, cost {}",
        run.id,
        run.branch,
        run.workspace.display(),
        usd(run.cost_usd)
    )
}

fn gate_line(gate: &OperatorGate) -> String {
    let state = match (&gate.decision, &gate.withdrawn) {
        (Some(decision), _) => decision.word().to_owned(),
        (None, Some(reason)) => format!("withdrawn: {reason}"),
        (None, None) => "waiting for the operator".to_owned(),
    };
    let mut line = format!(
        "{} of stage {} attempt {} in run {} [{state}]",
        gate.subject.noun(),
        gate.stage_id,
        gate.attempt,
        gate.run
    );
    if let Some(location) = &gate.location {
        let _ = write!(line, ", shown at {location}");
    }
    if let Some(question) = &gate.question {
        let _ = write!(line, ", question {question}");
    }
    line
}

fn autonomy_text(view: &AutonomyView) -> String {
    let scope = view
        .repo
        .as_deref()
        .map_or("Global autonomy".to_owned(), |r| {
            format!("Autonomy for repo {r}")
        });
    let mut text = format!(
        "{scope}: defaults {}; the ceiling allows {}.",
        view.policy.default.describe(),
        view.policy.ceiling.describe_allowed()
    );
    if view.directives.is_empty() {
        text.push_str("\nNo operator directives.");
    }
    for directive in &view.directives {
        let _ = write!(
            text,
            "\nDirective {} ({}{}): {}",
            directive.id,
            snake(&directive.source),
            until(directive),
            directive.text
        );
    }
    text
}

fn until(directive: &Directive) -> String {
    directive
        .expires_at
        .map_or(String::new(), |at| format!(", until {}", at.to_rfc3339()))
}

/// A stage attempt; clipped text says how `get_stage` shows all of it.
/// `run` is the stage's run when it is not the item's latest.
fn stage_line(item: &QueueItemId, run: Option<&PipelineRunId>, stage: &StageRun) -> String {
    assert!(
        run.is_none_or(|run| *run == stage.run),
        "a stage is named by its own run"
    );
    let whole = format!(
        "; {} shows it all",
        stage_ref(item, run, &stage.stage_id, stage.attempt, stage.fan_index)
    );
    let state = match &stage.state {
        StageState::Failed { error } => {
            format!("failed: {}", clip_noting(error, CLIP_CHARS, &whole))
        }
        other => snake(other).replace('_', " "),
    };
    let choice = stage.current_choice();
    let mut line = format!(
        "{} attempt {} slot {}: {} {} [{state}]",
        stage.stage_id, stage.attempt, stage.fan_index, choice.model, choice.effort
    );
    if let Some(verdict) = stage.verdict {
        let _ = write!(line, " verdict {}", snake(&verdict));
    }
    let _ = write!(line, ", cost {}", usd(stage.cost_usd));
    if let Some(agent) = &stage.aether_agent {
        let _ = write!(line, ", agent {agent}");
    }
    if stage.structured.is_some() {
        line.push_str(", structured result (get_stage shows it)");
    }
    if let Some(summary) = &stage.summary {
        let _ = write!(
            line,
            "\n    summary: {}",
            clip_noting(summary, CLIP_CHARS, &whole)
        );
    }
    line
}

/// The `get_stage` call that reads one stage run; `run` only when it is
/// not the item's latest, which `get_stage` reads by default.
fn stage_ref(
    item: &QueueItemId,
    run: Option<&PipelineRunId>,
    stage: &str,
    attempt: u32,
    slot: u32,
) -> String {
    assert!(attempt >= 1, "attempts count from 1");
    assert!(!stage.is_empty(), "a stage run has a stage id");
    let run = run.map_or(String::new(), |run| format!(", run {run}"));
    format!("get_stage with item {item}{run}, stage {stage:?}, attempt {attempt}, slot {slot}")
}

/// One line per pipeline, or a lone pipeline (one asked for by name) in full.
fn catalog_text(catalog: &CatalogListing) -> String {
    let full = catalog.pipelines.len() == 1;
    let lines = catalog.pipelines.iter().map(|pipeline| match full {
        true => pipeline_text(pipeline),
        false => pipeline_line(pipeline),
    });
    let mut text = listing("pipelines", lines);
    if catalog.pipelines.len() > 1 {
        text.push_str("\nName a `pipeline` to see its stages, gates, loops and nudges.");
    }
    for problem in &catalog.problems {
        let place = problem
            .path
            .as_ref()
            .map_or(String::new(), |p| format!(" at {}", p.display()));
        let _ = write!(
            text,
            "\nUnusable pipeline {} ({}{place}): {}",
            problem.name,
            snake(&problem.source),
            problem.error
        );
    }
    text
}

/// What choosing `pipeline` takes: its description and stage ids.
fn pipeline_line(pipeline: &PipelineSummary) -> String {
    let stages: Vec<&str> = pipeline.stages.iter().map(|s| s.id.as_str()).collect();
    assert!(!stages.is_empty(), "a loaded pipeline has stages");
    let mut text = format!(
        "{} ({}): {}; stages {}",
        pipeline.name,
        snake(&pipeline.source),
        pipeline.description,
        stages.join(", ")
    );
    if let Some(tier) = pipeline.nudges.tier {
        let _ = write!(text, "; tier {}", snake(&tier));
    }
    text.push('.');
    text
}

fn pipeline_text(pipeline: &PipelineSummary) -> String {
    let mut text = format!(
        "{} ({}): {} Workspace {}; repo {}.",
        pipeline.name,
        snake(&pipeline.source),
        pipeline.description,
        snake(&pipeline.workspace),
        if pipeline.requires_repo {
            "required"
        } else {
            "optional"
        }
    );
    let nudges = &pipeline.nudges;
    if let Some(tier) = nudges.tier {
        let _ = write!(text, " Nudge: tier {}.", snake(&tier));
    }
    if nudges.independent_reviewers {
        text.push_str(" Nudge: independent reviewers (distinct models or vendors).");
    }
    if let Some(notes) = &nudges.notes {
        let _ = write!(text, " Notes: {notes}");
    }
    for stage in &pipeline.stages {
        let _ = write!(text, "\n    stage {}", stage_summary(stage));
    }
    text
}

fn stage_summary(stage: &StageSummary) -> String {
    let mut text = format!("{} ({}", stage.id, snake(&stage.kind));
    if !stage.roles.is_empty() {
        let _ = write!(text, ", roles {}", stage.roles.join(", "));
    } else if stage.fan_out > 1 {
        let _ = write!(text, ", fan_out {}", stage.fan_out);
    }
    if stage.gate != Gate::None {
        let _ = write!(text, ", gate {}", snake(&stage.gate));
    }
    if let Some(target) = &stage.loops_to {
        let _ = write!(
            text,
            ", on fail back to {target} at most {}x",
            stage.max_loops
        );
    }
    let _ = write!(text, ", timeout {}m)", stage.timeout_minutes);
    if let Some(nudge) = &stage.nudge {
        let _ = write!(text, ": {nudge}");
    }
    text
}

fn policy_text(policy: &ModelPolicy) -> String {
    let denied: Vec<String> = policy.deny.iter().map(ToString::to_string).collect();
    let denied = match denied.is_empty() {
        true => "none".to_owned(),
        false => denied.join(", "),
    };
    let effort = policy
        .max_effort
        .map_or("no cap".to_owned(), |e| e.to_string());
    format!(
        "Allowed models: {}. Denied: {denied}. Max effort: {effort}.",
        policy.describe_allowed()
    )
}

fn daemon_status(status: &DaemonStatus) -> String {
    let aether = match &status.aether {
        AetherLinkStatus::Connected {
            daemon_version,
            protocol_version,
            ..
        } => format!("connected (aetherd {daemon_version}, protocol {protocol_version})"),
        AetherLinkStatus::Disconnected { error, .. } => {
            format!("DISCONNECTED ({error}); nothing dispatches until it reconnects")
        }
    };
    let orchestrator = status.orchestrator.as_ref().map_or("none".to_owned(), |o| {
        format!(
            "{} {} {} [{}], restarts {}",
            o.agent, o.model, o.effort, o.status, o.restarts
        )
    });
    let caps = &status.caps;
    format!(
        "archond {} (pid {}), up since {}. Aether: {aether}. Orchestrator: {orchestrator}.\n\
         Paused: {}.\nAgents running {} (max {}), active runs {} (max {}), queued {}, blocked {}, \
         open questions {}, operator gates waiting {}.\n{} Lavish {}.\nAutonomy defaults {}; the ceiling allows {}; {} \
         directive(s) in force (autonomy_policy lists them).{}{}",
        status.daemon_version,
        status.pid,
        status.started_at.to_rfc3339(),
        pause_text(status),
        status.running_agents,
        cap(caps.max_parallel_agents),
        status.active_runs,
        cap(caps.max_active_runs),
        status.queued_items,
        status.blocked_items,
        status.open_questions,
        status.open_gates,
        spend_text(caps, status.spend_today_usd, status.spend_total_usd),
        if status.lavish_available {
            "available"
        } else {
            "unavailable"
        },
        status.autonomy.default.describe(),
        status.autonomy.ceiling.describe_allowed(),
        status.directives.len(),
        status.github.as_ref().map_or(String::new(), github_text),
        status.disk.as_ref().map_or(String::new(), disk_text)
    )
}

/// Spend against the budgets, or that the budgets are off.
pub(super) fn spend_text(caps: &Caps, today_usd: f64, total_usd: f64) -> String {
    let spend = format!(
        "Budget-counted spend today {}, total {}",
        usd(today_usd),
        usd(total_usd)
    );
    if !caps.budgets_enabled {
        return format!(
            "{spend}. Budgets: OFF (no dollar budget is enforced; spend is still recorded)."
        );
    }
    format!(
        "{spend} (daily budget {}, total budget {}), per-item default {}.",
        budget(caps.budget_usd_daily),
        budget(caps.budget_usd_total),
        budget(caps.budget_usd_per_item)
    )
}

/// Free space where archond writes, and whether it holds dispatch.
fn disk_text(disk: &DiskStatus) -> String {
    let free = match (disk.free_bytes, &disk.error) {
        (Some(free), _) => gib(free),
        (None, error) => format!("unknown ({})", error.as_deref().unwrap_or("no reading")),
    };
    let state = if let Some(why) = &disk.standby {
        format!("archond is in STANDBY and serves reads only ({why})")
    } else if let Some(error) = &disk.disk_full {
        format!("dispatch is HELD after a write failed on a full disk ({error})")
    } else if disk.held {
        "dispatch is HELD for low disk space".to_owned()
    } else {
        return format!("\nDisk: {free} free at {}.", disk.path.display());
    };
    format!(
        "\nDisk: {free} free at {}; {state}. Nothing new starts until the operator frees \
         space (`archon gc`).",
        disk.path.display()
    )
}

fn github_text(watch: &GithubWatch) -> String {
    let view = match watch {
        GithubWatch::Unreachable { error } => {
            return format!("\nGitHub watcher: unreadable ({error}).");
        }
        GithubWatch::Watching(view) => view,
    };
    let archons: Vec<&str> = view
        .repositories
        .iter()
        .filter(|r| r.is_archons() && r.enabled)
        .map(|r| r.repository.as_str())
        .collect();
    let mut text = format!(
        "\nGitHub watcher: {}, {} repositor(ies) route @archon here{}.",
        if view.enabled { "on" } else { "off" },
        archons.len(),
        if archons.is_empty() {
            String::new()
        } else {
            format!(" ({})", archons.join(", "))
        }
    );
    let stale: Vec<&str> = view.stale().map(|r| r.repository.as_str()).collect();
    if !stale.is_empty() {
        let _ = write!(
            text,
            " Still aimed at an old orchestrator: {}; archond repoints them.",
            stale.join(", ")
        );
    }
    text
}

fn pause_text(status: &DaemonStatus) -> String {
    let pause = &status.pause;
    let mut parts = Vec::new();
    if pause.global {
        parts.push("everything (global)".to_owned());
    }
    parts.extend(pause.repos.iter().map(|r| format!("repo {r}")));
    parts.extend(pause.pipelines.iter().map(|p| format!("pipeline {p}")));
    match parts.is_empty() {
        true => "nothing".to_owned(),
        false => parts.join(", "),
    }
}

fn report_text(report: &ReportPublished) -> String {
    match report.published {
        true => format!(
            "Report {} is open for review at {}. The operator's feedback will arrive as a \
             lavish_feedback trigger.",
            report.report, report.location
        ),
        false => format!(
            "Report {} recorded; Lavish is unavailable, so the operator was pointed at {}.",
            report.report, report.location
        ),
    }
}

/// The serde (wire) spelling of a unit-like enum value.
fn snake<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(serde_json::Value::Object(map)) => map
            .values()
            .next()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn usd(amount: f64) -> String {
    format!("${amount:.2}")
}

fn cap(limit: u32) -> String {
    match limit {
        0 => "unlimited".to_owned(),
        n => n.to_string(),
    }
}

fn budget(limit: f64) -> String {
    match limit > 0.0 {
        true => usd(limit),
        false => "unlimited".to_owned(),
    }
}

/// At most `max` characters, with the cut stated.
fn clip(text: &str, max: usize) -> String {
    clip_noting(text, max, "")
}

/// As `clip`, with `note` added to the statement of the cut.
fn clip_noting(text: &str, max: usize, note: &str) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}… [clipped: {total} chars in full{note}]")
}
