//! Replies as text for the model. Aether hands native agents only the text
//! half of a tool result, so every id, state and reason the orchestrator
//! acts on is written out; only long free text is clipped, and says so.

mod artifacts;
mod cost;
mod initiatives;
mod lessons;
mod stages;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use archon_core::autonomy::{AutonomyView, Directive, MAX_DIRECTIVES};
use archon_core::catalog::{CatalogListing, Gate, PipelineSummary, StageSummary};
use archon_core::github::GithubWatch;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::integration::IntegrationStatus;
use archon_core::merge::MergeRequest;
use archon_core::model::{Effort, ModelPlan, ModelReasons};
use archon_core::notify::Question;
use archon_core::policy::ModelPolicy;
use archon_core::protocol::{
    AetherLinkStatus, DaemonStatus, DiskStatus, ItemDetail, OrchestratorInfo, QuotaStatus, Reply,
    ReportPublished, StatusHealth, gib,
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
        Reply::Guide { text } => text.clone(),
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
        Reply::Merge(request) => merge_text(request),
        Reply::Promotion(promotion) => promotion.describe(),
        Reply::Settings(view) => view.describe(),
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
        | Reply::UpgradeStarted { .. }
        | Reply::Versions(_)
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
        "refresh_item" => "Refresh queued.",
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
        "refresh_item" => "Refresh queued as ",
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
        plan_text(
            &item.model_plan,
            item.model_reasons.as_ref(),
            &detail.default_efforts
        ),
        item.autonomy.describe()
    );
    if let Some(provider) = &item.routed_provider {
        let _ = write!(
            text,
            "\nRouted provider: {provider} (tier-only choices, by provider weights)"
        );
    }
    if let Some(switch) = &item.provider_switch {
        let _ = write!(text, "\nProvider switch: {switch}");
    }
    if let Some(onto) = &item.onto {
        let _ = write!(
            text,
            "\nOnto: {onto}, frozen at enqueue from {}",
            onto.target
        );
    }
    if !item.stage_timeouts.is_empty() {
        let timeouts: Vec<String> = (item.stage_timeouts.iter())
            .map(|(stage, minutes)| format!("{stage} {minutes}m"))
            .collect();
        let _ = write!(text, "\nStage timeouts: {}", timeouts.join(", "));
    }
    for gate in &detail.gates {
        let _ = write!(text, "\nGate {}", gate_line(gate));
    }
    if let Some(merge) = detail.merges.last() {
        let _ = write!(text, "\nMerge: {}", merge.line());
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

fn plan_text(
    plan: &ModelPlan,
    reasons: Option<&ModelReasons>,
    defaults: &BTreeMap<String, Effort>,
) -> String {
    if plan.0.is_empty() {
        return "(empty)".to_owned();
    }
    let stages: Vec<String> = plan
        .0
        .iter()
        .map(|(stage, choices)| {
            let picks: Vec<String> = choices.iter().map(ToString::to_string).collect();
            let reason = reasons
                .and_then(|r| r.get(stage))
                .map_or(String::new(), |r| format!(" ({r})"));
            let above = defaults
                .get(stage)
                .filter(|default| choices.iter().any(|c| c.effort > **default))
                .map_or(String::new(), |default| {
                    format!(" [above default {default}]")
                });
            format!("{stage} = {}{above}{reason}", picks.join(" + "))
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
    if !view.merge_granted.is_empty() {
        let _ = write!(
            text,
            "\ngrant_merge is on for {}: request_merge merges there without asking the \
             operator, when the operator asked you to merge.",
            view.merge_granted.join(", ")
        );
    }
    if !view.limits.is_empty() {
        text.push_str("\nStructural limits:");
    }
    for limit in &view.limits {
        let _ = write!(text, "\n  {}", limit.describe());
    }
    let _ = write!(
        text,
        "\nDirectives {}/{MAX_DIRECTIVES} in force.",
        view.directives.len()
    );
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

/// Where a merge request stands, and what to tell the operator.
fn merge_text(request: &MergeRequest) -> String {
    let mut text = format!(
        "Merge {} of {}: {}",
        request.id,
        request.item,
        request.line()
    );
    if let Some(checks) = &request.checks {
        let _ = write!(text, "\nChecks: {checks}.");
    }
    if let Some(number) = request.waiting_on() {
        let _ = write!(
            text,
            "\nTell the operator: Q{number} asks them to confirm; they answer `archon answer \
             {number} yes` (or `yes ignore-checks`, or `no`), `/answer {number} yes` in archon chat, \
             or in this chat when it is dedicated. You cannot answer it. \
             Its outcome comes as merge_finished."
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
        "{} attempt {} slot {}: {choice} [{state}]",
        stage.stage_id, stage.attempt, stage.fan_index
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
    } else if let Some(planner) = &stage.roles_from {
        let _ = write!(text, ", one role per task of the plan of {planner}");
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
    if let Some(tier) = stage.tier {
        let _ = write!(text, ", default tier {tier}");
    }
    let _ = write!(
        text,
        ", timeout {}m, effort {})",
        stage.timeout_minutes, stage.effort
    );
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

/// The orchestrator, and whether a restart is owed for its prompt, tools
/// or lessons.
fn orchestrator_text(o: &OrchestratorInfo) -> String {
    format!(
        "{} {} {} [{}], restarts {}; {}",
        o.agent,
        o.model,
        o.effort,
        o.status,
        o.restarts,
        o.describe_freshness()
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
    let orchestrator = status
        .orchestrator
        .as_ref()
        .map_or("none".to_owned(), orchestrator_text);
    let caps = &status.caps;
    format!(
        "archond {} (pid {}), up since {}. Aether: {aether}. Orchestrator: {orchestrator}.\n\
         Paused: {}.\nAgents running {} (max {}), active runs {} (max {}), queued {}, blocked {}, \
         open questions {}, operator gates waiting {}.\n{} Lavish {}.\nAutonomy defaults {}; the ceiling allows {}; directives {}/{MAX_DIRECTIVES} \
         in force (autonomy_policy lists them).{}{}{}",
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
        status.disk.as_ref().map_or(String::new(), disk_text),
        upkeep_text(status)
    ) + &quota_text(status.quota.as_ref())
        + &integration_text(&status.integration)
        + &stale_text(status.health.as_ref())
}

/// Each provider's quota headroom, reserve and next reset, as Aether last
/// reported them.
pub(super) fn quota_text(quota: Option<&QuotaStatus>) -> String {
    let Some(quota) = quota.filter(|q| !q.providers.is_empty()) else {
        return String::new();
    };
    let providers: Vec<String> = quota.providers.iter().map(|p| p.describe()).collect();
    format!(
        "\nProvider quota as of {}: {}.",
        quota.as_of.to_rfc3339(),
        providers.join(" | ")
    )
}

/// Each watched integration branch: its health, last green commit and the
/// merges since.
pub(super) fn integration_text(branches: &[IntegrationStatus]) -> String {
    branches
        .iter()
        .map(|branch| format!("\nIntegration branch {}.", branch.describe()))
        .collect()
}

/// The sections that show Aether's last answer rather than its latest.
fn stale_text(health: Option<&StatusHealth>) -> String {
    let stale = health.map(|h| h.stale(archon_core::now()));
    match stale.filter(|s| !s.is_empty()) {
        Some(stale) => format!(
            "\nSTALE (Aether's last answers, not current): {}.",
            stale.join("; ")
        ),
        None => String::new(),
    }
}

/// Start-up recovery or an upgrade under way, and versions that differ:
/// running against installed, or against the latest release once checked.
fn upkeep_text(status: &DaemonStatus) -> String {
    let mut text = String::new();
    if let Some(recovery) = &status.recovery {
        text.push_str(&format!(
            "\nRecovery under way (changes refused, dispatch held until it is done): {}.",
            recovery.describe()
        ));
    }
    if let Some(upgrade) = &status.upgrade {
        text.push_str(&format!(
            "\nUpgrade under way (dispatch held while it drains): {upgrade}."
        ));
    }
    for line in status.versions.iter().flat_map(|versions| versions.lines()) {
        text.push_str(&format!("\nVersions: {line}."));
    }
    text
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
    let mut text = format!("\nDisk: {free} free at {}.", disk.path.display());
    let state = if let Some(why) = &disk.standby {
        Some(format!(
            "archond is in STANDBY and serves reads only ({why})"
        ))
    } else if let Some(error) = &disk.disk_full {
        Some(format!(
            "dispatch is HELD after a write failed on a full disk ({error})"
        ))
    } else if disk.held {
        Some("dispatch is HELD for low disk space".to_owned())
    } else {
        None
    };
    if let Some(state) = state {
        text.push_str(&format!(
            " {state}. Nothing new starts until the operator frees space (`archon gc`)."
        ));
    }
    for usage in &disk.storage {
        let size = usage.bytes.map_or_else(
            || {
                format!(
                    "unknown ({})",
                    usage.error.as_deref().unwrap_or("no reading")
                )
            },
            |bytes| format!("{bytes} bytes"),
        );
        text.push_str(&format!("\nStorage {}: {size}.", usage.kind));
    }
    if let Some(gc) = &disk.gc {
        text.push_str(&format!(
            "\nLast GC at {} freed {} bytes: {}",
            gc.at, gc.bytes_freed, gc.headline
        ));
    }
    text
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

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[test]
    fn queue_status_says_when_the_orchestrator_is_stale() {
        let mut info: OrchestratorInfo = serde_json::from_value(serde_json::json!({
            "agent": "agt_0001", "model": "claude-code/opus", "effort": "high",
            "status": "ready", "restarts": 6, "generation": 7,
            "freshness": {"created_by": "1.2.3", "prompt_outdated": true,
                          "tools_outdated": false, "new_lessons": 3},
        }))
        .unwrap();
        let text = orchestrator_text(&info);
        assert!(
            text.ends_with("generation 7, created by 1.2.3; prompt outdated; 3 new lessons"),
            "{text}"
        );
        info.freshness = None;
        let text = orchestrator_text(&info);
        assert!(
            text.contains("created before Archon recorded its prompt"),
            "{text}"
        );
    }

    #[test]
    fn queue_status_includes_storage_and_last_gc_even_without_disk_pressure() {
        let disk: DiskStatus = serde_json::from_value(serde_json::json!({
            "path": "/data", "free_bytes": 1000000, "min_free_bytes": 0, "held": false,
            "storage": [{"kind": "reports_lavish", "path": "/data/reports", "bytes": 8192}],
            "gc": {"at": archon_core::now(), "bytes_freed": 4096, "pressure": false, "headline": "one report"}
        })).unwrap();
        let text = disk_text(&disk);
        assert!(
            text.contains("Storage reports_lavish: 8192 bytes"),
            "{text}"
        );
        assert!(text.contains("freed 4096 bytes: one report"), "{text}");
    }

    #[test]
    fn queue_status_names_stale_sections_with_their_age_and_says_nothing_when_fresh() {
        let probe = |as_of: Option<archon_core::Timestamp>, error: Option<&str>, stale| {
            archon_core::protocol::ProbeHealth {
                as_of,
                last_error: error.map(str::to_owned),
                stale,
            }
        };
        let earlier =
            archon_core::Timestamp::from_timestamp(archon_core::now().timestamp() - 42, 0).unwrap();
        let mut health = StatusHealth {
            aether: probe(Some(earlier), None, true),
            mcp: probe(None, Some("unavailable: not connected"), true),
            github: None,
        };
        let text = stale_text(Some(&health));
        assert!(text.contains("STALE"), "{text}");
        assert!(text.contains("aether: last answer 4"), "{text}");
        assert!(text.contains("still asking"), "{text}");
        assert!(
            text.contains("mcp: never answered, latest ask failed: unavailable"),
            "{text}"
        );
        health.aether.stale = false;
        health.mcp = probe(Some(earlier), None, false);
        assert_eq!(stale_text(Some(&health)), "");
        assert_eq!(stale_text(None), "");
    }
}
