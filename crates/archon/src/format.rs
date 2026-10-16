//! Plain-text views of daemon replies for the CLI and chat. Pure. Text an
//! agent or a repository wrote (titles, briefs, questions, errors, notes)
//! goes through [`sanitize`] or [`sanitize_line`] before it is shown.

use std::collections::HashMap;
use std::fmt::Write as _;

use archon_core::Timestamp;
use archon_core::autonomy::{AutonomyView, Directive, DirectiveSource, MAX_DIRECTIVES};
use archon_core::catalog::{CatalogListing, Gate, PipelineSource, PipelineSummary};
use archon_core::github::{GithubWatch, GithubWatcherView, WatchTarget, WatchedRepo};
use archon_core::id::RepoId;
use archon_core::merge::MergeRequest;
use archon_core::notify::Question;
use archon_core::protocol::{
    AetherLinkStatus, DaemonStatus, DiskStatus, McpHealth, McpStatus, OrchestratorInfo,
    PauseTarget, ReportPublished, ReportState, ReportSummary, gib,
};
use archon_core::queue::{Caps, PauseState};
use archon_core::repo::{Attribution, Repo, ShipMode};

use crate::wrap::{sanitize, sanitize_line};

mod cost;
mod queue;

pub use cost::cost;
pub use queue::{item_detail, items};

/// Registry names by id, for showing items and pauses by repo name.
pub type RepoNames = HashMap<RepoId, String>;

pub fn repo_names(repos: &[Repo]) -> RepoNames {
    repos
        .iter()
        .map(|r| (r.id.clone(), r.name.clone()))
        .collect()
}

fn repo_label(names: &RepoNames, id: Option<&RepoId>) -> String {
    match id {
        None => "-".to_owned(),
        Some(id) => names.get(id).cloned().unwrap_or_else(|| id.to_string()),
    }
}

/// One `key: value` line per fact; `aether: connected|disconnected` is what
/// scripts and the end-to-end test look for.
pub fn status(status: &DaemonStatus, names: &RepoNames) -> String {
    let mut out = String::new();
    if !status.waiting_items.is_empty() {
        let _ = writeln!(
            out,
            "{}\n",
            items(&status.waiting_items, names, archon_core::now())
        );
    }
    if let Some(next) = status.waiting_next_offset {
        let more = status.waiting_total.saturating_sub(next);
        let _ = writeln!(
            out,
            "{more} more waiting item(s) not shown; `archon queue list` lists every item\n"
        );
    }
    let uptime = archon_core::now() - status.started_at;
    let _ = writeln!(
        out,
        "archond: {} (pid {}), up {}",
        status.daemon_version,
        status.pid,
        duration(uptime.num_seconds())
    );
    if let Some(recovery) = &status.recovery {
        let _ = writeln!(
            out,
            "recovery: {}; changes are refused and dispatch held until it is done",
            recovery.describe()
        );
    }
    if let Some(upgrade) = &status.upgrade {
        let _ = writeln!(out, "upgrade: {upgrade}");
    }
    for line in status.versions.iter().flat_map(|versions| versions.lines()) {
        let _ = writeln!(out, "versions: {line}");
    }
    link_lines(&mut out, status);
    let _ = writeln!(
        out,
        "queue: {} queued, {} blocked, {} active runs, {} running agents",
        status.queued_items, status.blocked_items, status.active_runs, status.running_agents
    );
    let _ = writeln!(out, "caps: {}", caps_line(&status.caps));
    let _ = writeln!(out, "budgets: {}", budgets_line(&status.caps));
    let _ = writeln!(
        out,
        "budget-counted spend: ${:.2} today, ${:.2} total",
        status.spend_today_usd, status.spend_total_usd
    );
    let _ = writeln!(out, "pause: {}", pause_line(&status.pause, names));
    let _ = writeln!(
        out,
        "autonomy default: {}",
        status.autonomy.default.describe()
    );
    let _ = writeln!(
        out,
        "autonomy ceiling: {}",
        status.autonomy.ceiling.describe_allowed()
    );
    let _ = writeln!(
        out,
        "directives: {} in force",
        directive_count(status.directives.len())
    );
    for directive in &status.directives {
        let _ = writeln!(out, "directive: {}", directive_line(directive));
    }
    merge_lines(&mut out, &status.merge_granted, &status.merges_waiting);
    let gates = match status.open_gates {
        0 => String::new(),
        1 => ", 1 gate waiting for approval".to_owned(),
        n => format!(", {n} gates waiting for approval"),
    };
    let _ = writeln!(out, "questions: {} open{gates}", status.open_questions);
    let lavish = if status.lavish_available {
        "available"
    } else {
        "not available"
    };
    let _ = write!(out, "lavish: {lavish}");
    out
}

/// Aether, the orchestrator and what archond depends on, one line each.
fn link_lines(out: &mut String, status: &DaemonStatus) {
    let _ = writeln!(out, "aether: {}", aether_link(&status.aether));
    let orchestrator = status
        .orchestrator
        .as_ref()
        .map_or_else(|| "not running".to_owned(), orchestrator_line);
    let _ = writeln!(out, "orchestrator: {orchestrator}");
    if let Some(mcp) = &status.mcp {
        let _ = writeln!(out, "mcp: {}", mcp_line(mcp, archon_core::now()));
    }
    let stale = status.health.as_ref().map(|h| h.stale(archon_core::now()));
    if let Some(stale) = stale.filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "health: STALE {}", sanitize_line(&stale.join("; ")));
    }
    if status.held_news > 0 {
        let _ = writeln!(out, "news: {}", held_news_line(status.held_news));
    }
    if let Some(github) = &status.github {
        let _ = writeln!(out, "github: {}", github_line(github));
    }
    for provider in status.quota.iter().flat_map(|q| &q.providers) {
        let _ = writeln!(out, "quota {}", sanitize_line(&provider.describe()));
    }
    if let Some(disk) = &status.disk {
        let _ = writeln!(out, "disk: {}", disk_line(disk));
        for usage in &disk.storage {
            let size = usage.bytes.map_or_else(
                || {
                    format!(
                        "unknown ({})",
                        sanitize_line(usage.error.as_deref().unwrap_or("no reading"))
                    )
                },
                |bytes| format!("{bytes} bytes"),
            );
            let _ = writeln!(out, "storage {}: {size}", usage.kind);
        }
    }
}

/// Free space where archond writes, and the disk hold or standby, if any.
pub fn disk_line(disk: &DiskStatus) -> String {
    let path = sanitize_line(&disk.path.display().to_string());
    let mut line = match (disk.free_bytes, &disk.error) {
        (Some(free), _) => format!("{} free at {path}", gib(free)),
        (None, error) => {
            let why = error.as_deref().unwrap_or("no reading");
            format!("free space unknown at {path} ({})", sanitize_line(why))
        }
    };
    if disk.min_free_bytes > 0 {
        let _ = write!(line, ", holds below {}", gib(disk.min_free_bytes));
    }
    let gc = "; run `archon gc`";
    if let Some(why) = &disk.standby {
        let _ = write!(line, "; STANDBY, reads only: {}{gc}", sanitize_line(why));
    } else if let Some(error) = &disk.disk_full {
        let _ = write!(
            line,
            "; dispatch HELD after a full disk: {}{gc}",
            sanitize_line(error)
        );
    } else if disk.held {
        let _ = write!(line, "; dispatch HELD for low disk space{gc}");
    }
    if let Some(last) = &disk.gc {
        let ago = duration((archon_core::now() - last.at).num_seconds());
        let _ = write!(
            line,
            "; last gc {ago} ago: {}",
            sanitize_line(&last.headline)
        );
    }
    line
}

pub fn aether_link(link: &AetherLinkStatus) -> String {
    match link {
        AetherLinkStatus::Connected {
            daemon_version,
            protocol_version,
            ..
        } => format!("connected (aetherd {daemon_version}, protocol {protocol_version})"),
        AetherLinkStatus::Disconnected { error, retry_at } => {
            let retry = retry_at
                .map(|at| {
                    format!(
                        "; retrying at {}",
                        at.with_timezone(&chrono::Local).format("%H:%M:%S")
                    )
                })
                .unwrap_or_default();
            format!("disconnected ({error}{retry})")
        }
    }
}

/// The `archon` MCP server as Aether sees it, and archond's heals of it.
/// `ok` and `FAIL` lead so scripts can match them.
pub fn mcp_line(mcp: &McpStatus, now: Timestamp) -> String {
    let health = match &mcp.health {
        McpHealth::Ready { tools } => format!("ok ({tools} tools)"),
        McpHealth::Failed { error } => format!("FAIL {}", sanitize_line(error)),
        McpHealth::Unknown => "not started yet (starts when an agent first uses it)".to_owned(),
        McpHealth::NotLoaded => "FAIL aetherd has not loaded the archon server".to_owned(),
        McpHealth::Unreachable { error } => format!("unknown ({})", sanitize_line(error)),
    };
    let failing = match mcp.failed_refreshes {
        0 => String::new(),
        1 => ", 1 refresh failed".to_owned(),
        n => format!(", {n} refreshes failed in a row"),
    };
    let unlinked = match mcp.failed_relinks {
        0 => String::new(),
        1 => ", 1 orchestrator restart failed".to_owned(),
        n => format!(", {n} orchestrator restarts failed in a row"),
    };
    let healed = mcp.last_heal.map_or_else(String::new, |at| {
        format!(", last healed {} ago", duration((now - at).num_seconds()))
    });
    format!("{health}{failing}{unlinked}{healed}")
}

/// Why the orchestrator has not heard about recent events yet.
fn held_news_line(held: u32) -> String {
    let events = match held {
        1 => "1 event".to_owned(),
        n => format!("{n} events"),
    };
    format!("{events} held for the orchestrator, delivered once it is reachable")
}

pub fn orchestrator_line(info: &OrchestratorInfo) -> String {
    let restarts = match info.restarts {
        0 => String::new(),
        1 => ", restarted once".to_owned(),
        n => format!(", restarted {n} times"),
    };
    format!(
        "{} on {} ({} effort), {}{restarts}; {}",
        info.agent,
        info.model,
        info.effort,
        info.status,
        info.describe_freshness()
    )
}

fn limit(value: u32) -> String {
    match value {
        0 => "unlimited".to_owned(),
        n => n.to_string(),
    }
}

fn dollars(value: f64) -> String {
    if value <= 0.0 {
        "unlimited".to_owned()
    } else {
        format!("${value:.2}")
    }
}

pub fn caps_line(caps: &Caps) -> String {
    let mut line = format!(
        "{} agents, {} per repo, {} active runs",
        limit(caps.max_parallel_agents),
        limit(caps.max_per_repo),
        limit(caps.max_active_runs)
    );
    for (pipeline, max) in &caps.max_per_pipeline {
        let _ = write!(line, ", {pipeline} {}", limit(*max));
    }
    line
}

pub fn budgets_line(caps: &Caps) -> String {
    if !caps.budgets_enabled {
        return "off (no dollar budget is enforced; spend is still recorded)".to_owned();
    }
    format!(
        "{} per item, {} daily, {} total",
        dollars(caps.budget_usd_per_item),
        dollars(caps.budget_usd_daily),
        dollars(caps.budget_usd_total)
    )
}

fn pause_line(pause: &PauseState, names: &RepoNames) -> String {
    let mut parts = Vec::new();
    if pause.global {
        parts.push("everything".to_owned());
    }
    for repo in &pause.repos {
        parts.push(format!("repo {}", repo_label(names, Some(repo))));
    }
    for pipeline in &pause.pipelines {
        parts.push(format!("pipeline {pipeline}"));
    }
    if parts.is_empty() {
        return "nothing paused".to_owned();
    }
    parts.join(", ")
}

fn duration(total_secs: i64) -> String {
    let secs = total_secs.max(0);
    match (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

pub fn ship(mode: ShipMode) -> &'static str {
    match mode {
        ShipMode::Pr => "pr",
        ShipMode::Local => "local",
    }
}

pub fn repo_line(repo: &Repo) -> String {
    let mut line = format!(
        "{}  {}  ship {}",
        repo.name,
        repo.path.display(),
        ship(repo.ship)
    );
    if let Some(base) = &repo.base_branch {
        let _ = write!(line, ", base {base}");
    }
    if let Some(integration) = &repo.tiers.integration_branch {
        let _ = write!(line, ", integration {integration}");
    }
    if let Some(pipeline) = &repo.default_pipeline {
        let _ = write!(line, ", pipeline {pipeline}");
    }
    if !repo.autonomy_max.is_empty() {
        let _ = write!(line, ", autonomy max {}", repo.autonomy_max.describe());
    }
    if repo.grant_merge {
        line.push_str(", grant_merge on");
    }
    if repo.attribution != Attribution::default() {
        let _ = write!(line, ", attribution {}", repo.attribution.name());
    }
    if let Some(user) = &repo.gh_user {
        let _ = write!(line, ", gh_user {user}");
    }
    line
}

/// `dir_…  text  (operator, until 2026-09-28 18:00 UTC)`.
pub fn directive_line(directive: &Directive) -> String {
    let source = match directive.source {
        DirectiveSource::Operator => "operator",
        DirectiveSource::Orchestrator => "via the orchestrator",
    };
    let until = directive
        .expires_at
        .map(|at| format!(", until {}", at.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default();
    format!(
        "{}  {}  ({source}{until})",
        directive.id,
        sanitize_line(&directive.text)
    )
}

/// `17/20`: directives in force against the most allowed at once.
fn directive_count(in_force: usize) -> String {
    format!("{in_force}/{MAX_DIRECTIVES}")
}

/// `archon autonomy`: the defaults and ceiling in force, the structural
/// limits, then directives.
pub fn autonomy(view: &AutonomyView) -> String {
    let scope = view
        .repo
        .as_ref()
        .map_or_else(|| "global".to_owned(), |repo| format!("repo {repo}"));
    let mut out = format!(
        "autonomy ({scope})\ndefault: {}\nceiling: {}\n",
        view.policy.default.describe(),
        view.policy.ceiling.describe_allowed()
    );
    merge_lines(&mut out, &view.merge_granted, &[]);
    if !view.limits.is_empty() {
        out.push_str("limits:\n");
    }
    for limit in &view.limits {
        let _ = writeln!(out, "  {}", limit.describe());
    }
    let count = directive_count(view.directives.len());
    if view.directives.is_empty() {
        let _ = write!(out, "directives {count}: none");
        return out;
    }
    let _ = write!(out, "directives {count}:");
    for directive in &view.directives {
        let _ = write!(out, "\n  {}", directive_line(directive));
    }
    out
}

/// The watcher in one line; a target left on an old orchestrator is named.
fn github_line(watch: &GithubWatch) -> String {
    let view = match watch {
        GithubWatch::Unreachable { error } => {
            return format!("unknown ({})", sanitize_line(error));
        }
        GithubWatch::Watching(view) => view,
    };
    let routed = view
        .repositories
        .iter()
        .filter(|r| r.enabled && r.is_archons())
        .count();
    let state = if view.enabled { "on" } else { "off" };
    let mut line = match routed {
        1 => format!("{state}, 1 repository routes @archon to the orchestrator"),
        n => format!("{state}, {n} repositories route @archon to the orchestrator"),
    };
    let stale: Vec<&str> = view.stale().map(|r| r.repository.as_str()).collect();
    if !stale.is_empty() {
        let _ = write!(
            line,
            "; MISMATCH {} still aimed at an old orchestrator, repointing",
            sanitize_line(&stale.join(", "))
        );
    }
    if let Some(operators) = &view.operator_users {
        let _ = write!(
            line,
            "; operator logins: {}",
            sanitize_line(&operators.join(", "))
        );
    }
    line
}

/// `archon github-watcher`: the settings, then one row per repository, then
/// each operator login a repository's allowlist leaves out.
pub fn github_watcher(view: &GithubWatcherView) -> String {
    let orchestrator = view
        .orchestrator
        .as_ref()
        .map_or_else(|| "none running".to_owned(), ToString::to_string);
    let mut out = format!(
        "github watcher: {} (revision {}), polls every {}s\nmention: {}\nlabel: {}\n\
         orchestrator: {orchestrator}\noperator logins: {}\n",
        if view.enabled { "on" } else { "off" },
        view.revision,
        view.poll_interval_secs,
        sanitize_line(&view.mention_regex),
        sanitize_line(&view.label_regex),
        operator_logins(view.operator_users.as_deref()),
    );
    if view.repositories.is_empty() {
        out.push_str("repositories: none; add one with `archon github-watcher add <owner/name>`");
        return out;
    }
    let rows: Vec<[String; 5]> = view.repositories.iter().map(watched_row).collect();
    out.push_str(&table(&rows));
    for repo in &view.repositories {
        for login in &repo.excluded_operators {
            let _ = write!(
                out,
                "\noperator login {} is not on {}'s allowlist: its requests are not delivered \
                 there at all",
                sanitize_line(login),
                sanitize_line(&repo.repository),
            );
        }
    }
    out
}

/// Whose own GitHub requests carry the operator's authority.
fn operator_logins(operator_users: Option<&[String]>) -> String {
    match operator_users {
        None => "none (every GitHub request is an external request)".to_owned(),
        Some(users) => format!(
            "{} (their own requests are your instructions)",
            sanitize_line(&users.join(", "))
        ),
    }
}

fn watched_row(repo: &WatchedRepo) -> [String; 5] {
    let target = match &repo.target {
        WatchTarget::Orchestrator => "-> orchestrator".to_owned(),
        WatchTarget::Stale { agent: Some(agent) } => format!("-> {agent} (stale, repointing)"),
        WatchTarget::Stale { agent: None } => "-> nobody (repointing)".to_owned(),
        WatchTarget::Unmanaged { mode } => {
            format!("{} dispatch, not Archon's", sanitize_line(mode))
        }
    };
    let polled = repo.last_poll_at.map_or_else(
        || "not polled yet".to_owned(),
        |at| {
            format!(
                "polled {} ago",
                duration((archon_core::now() - at).num_seconds())
            )
        },
    );
    let mut health = format!(
        "{polled}, {} dispatched, {} ignored",
        repo.dispatched, repo.ignored
    );
    if let Some(error) = &repo.last_error {
        let _ = write!(health, ", error: {}", sanitize_line(error));
    }
    [
        sanitize_line(&repo.repository),
        if repo.enabled { "on" } else { "off" }.to_owned(),
        target,
        requesters(repo.allowed_users.as_deref()),
        health,
    ]
}

/// Who may dispatch; Aether checks write access either way.
fn requesters(allowed_users: Option<&[String]>) -> String {
    match allowed_users {
        None => "any writer".to_owned(),
        Some(users) => format!("only {}", sanitize_line(&users.join(", "))),
    }
}

pub fn repos(repos: &[Repo]) -> String {
    if repos.is_empty() {
        return "No repositories registered. Add one with `archon repo add <path>`.".to_owned();
    }
    repos.iter().map(repo_line).collect::<Vec<_>>().join("\n")
}

/// One row per report: id, state, where it was shown, title; then how to
/// show a closed one again.
pub fn reports(reports: &[ReportSummary]) -> String {
    if reports.is_empty() {
        return "No reports.".to_owned();
    }
    let rows: Vec<[String; 4]> = reports
        .iter()
        .map(|report| {
            let path = report.path.display().to_string();
            [
                report.report.to_string(),
                report.state.as_str().to_owned(),
                sanitize_line(report.location.as_deref().unwrap_or(&path)),
                sanitize_line(&report.title),
            ]
        })
        .collect();
    let mut out = table(&rows);
    if reports.iter().any(|r| r.state != ReportState::Open) {
        out.push_str(concat!(
            "\n`archon reports reopen <rpt_…>` shows one again. ",
            "A disconnected or ended report is kept while its file exists, ",
            "any other one for 7 days after its last change."
        ));
    }
    out
}

/// What `archon reports reopen` did.
pub fn reopened(published: &ReportPublished) -> String {
    let location = sanitize_line(&published.location);
    match published.published {
        true => format!("Reopened {} at {location}", published.report),
        false => format!(
            "Lavish did not open {}; the report is at {location}",
            published.report
        ),
    }
}

/// Left-aligned columns, the last one unpadded.
fn table<const N: usize>(rows: &[[String; N]]) -> String {
    assert!(N > 0, "a table has columns");
    let mut widths = [0usize; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|row| {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i + 1 == N {
                    line.push_str(cell);
                } else {
                    let _ = write!(line, "{cell:<width$}  ", width = widths[i]);
                }
            }
            line.trim_end().to_owned()
        })
        .collect();
    lines.join("\n")
}

/// What a pause or resume applies to, e.g. `repo api`.
pub fn pause_target(target: &PauseTarget) -> String {
    match target {
        PauseTarget::Global { hard: false } => "dispatch".to_owned(),
        PauseTarget::Global { hard: true } => "dispatch and running stage agents (hard)".to_owned(),
        PauseTarget::Repo { name } => format!("repo {}", name.0),
        PauseTarget::Pipeline { name } => format!("pipeline {name}"),
    }
}

/// Questions by their permanent numbers, options indented under each.
pub fn questions<'a>(open: impl IntoIterator<Item = &'a Question>) -> String {
    let mut out = Vec::new();
    for question in open {
        out.push(question_lines(question));
    }
    if out.is_empty() {
        return "No open questions.".to_owned();
    }
    out.join("\n")
}

/// `Q7`, or the id of a question without a number (from an older archond),
/// which `archon answer` accepts too.
fn question_label(question: &Question) -> String {
    match question.number {
        0 => question.id.to_string(),
        n => format!("Q{n}"),
    }
}

/// `Q7  text  [qi_…]`, the text's further lines under its first, then one
/// line per option.
fn question_lines(question: &Question) -> String {
    let label = question_label(question);
    let item = question
        .item
        .as_ref()
        .map(|item| format!("  [{item}]"))
        .unwrap_or_default();
    let indent = " ".repeat(label.chars().count() + 2);
    let text = sanitize(question.text.trim()).replace('\n', &format!("\n{indent}"));
    let mut out = format!("{label}  {text}{item}");
    for (j, option) in question.options.iter().enumerate() {
        let _ = write!(out, "\n      {}) {}", j + 1, sanitize_line(option));
    }
    debug_assert!(!out.contains('\x1b'));
    out
}

/// The repositories with `grant_merge`, then each merge waiting for the operator.
fn merge_lines(out: &mut String, granted: &[String], waiting: &[MergeRequest]) {
    if !granted.is_empty() {
        let names = sanitize_line(&granted.join(", "));
        let _ = writeln!(out, "grant_merge: on for {names}");
    }
    for merge in waiting {
        let line = sanitize_line(&merge.line());
        let _ = writeln!(out, "merge waiting: {} {line}", merge.item);
    }
}

/// What `archon merge` did, or why it merged nothing.
pub fn merge(merge: &MergeRequest) -> String {
    let mut out = format!("{}: {}", merge.item, sanitize_line(&merge.line()));
    if let Some(checks) = &merge.checks {
        let _ = write!(out, "\n  checks: {}", sanitize_line(checks));
    }
    out
}

/// What `archon answer` did: the question's number and id, the answer, and
/// the first line of the question so a wrong target is noticed.
pub fn answered(question: &Question, answer: &str) -> String {
    assert!(!answer.trim().is_empty(), "an answer says something");
    let label = match question.number {
        0 => question.id.to_string(),
        n => format!("Q{n} ({})", question.id),
    };
    let asked = sanitize(question.text.trim());
    let first = asked.lines().next().unwrap_or_default();
    format!("Answered {label}: {}\n  {first}", sanitize_line(answer))
}

fn source(source: PipelineSource) -> &'static str {
    match source {
        PipelineSource::Builtin => "built-in",
        PipelineSource::User => "user",
        PipelineSource::Repo => "repo",
    }
}

fn gate_name(gate: Gate) -> Option<&'static str> {
    match gate {
        Gate::None => None,
        Gate::AllPass => Some("all_pass"),
        Gate::Majority => Some("majority"),
        Gate::Any => Some("any"),
    }
}

/// `implement → review×3 (all_pass, ↺ implement ≤2) → ship`.
pub fn stage_chain(pipeline: &PipelineSummary) -> String {
    let stages: Vec<String> = pipeline
        .stages
        .iter()
        .map(|stage| {
            let mut text = stage.id.clone();
            if stage.fan_out > 1 {
                let _ = write!(text, "×{}", stage.fan_out);
            }
            let mut notes = Vec::new();
            if let Some(gate) = gate_name(stage.gate) {
                notes.push(gate.to_owned());
            }
            if let Some(target) = &stage.loops_to {
                notes.push(format!("↺ {target} ≤{}", stage.max_loops));
            }
            if !notes.is_empty() {
                let _ = write!(text, " ({})", notes.join(", "));
            }
            text
        })
        .collect();
    stages.join(" → ")
}

pub fn catalog(listing: &CatalogListing) -> String {
    let mut out = Vec::new();
    for pipeline in &listing.pipelines {
        out.push(format!(
            "{}  ({})  {}",
            pipeline.name,
            source(pipeline.source),
            sanitize_line(&pipeline.description)
        ));
        out.push(format!("    {}", stage_chain(pipeline)));
    }
    out.extend(problems(listing));
    out.join("\n")
}

pub fn problems(listing: &CatalogListing) -> Vec<String> {
    listing
        .problems
        .iter()
        .map(|p| {
            let path = p
                .path
                .as_ref()
                .map(|p| format!(" at {}", p.display()))
                .unwrap_or_default();
            format!(
                "broken {} ({}){path}: {}",
                p.name,
                source(p.source),
                sanitize_line(&p.error)
            )
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use archon_core::autonomy::{AutonomyConfig, AutonomyPolicy};
    use archon_core::config::LimitsConfig;
    use archon_core::ops::{ProgramVersions, Versions};
    use archon_core::protocol::GcStatus;

    use super::*;

    pub(crate) fn status_with(aether: AetherLinkStatus) -> DaemonStatus {
        DaemonStatus {
            waiting_items: vec![],
            daemon_version: "0.1.0".into(),
            pid: 42,
            started_at: archon_core::now(),
            aether,
            orchestrator: None,
            caps: Caps::default(),
            pause: PauseState::default(),
            running_agents: 0,
            active_runs: 0,
            queued_items: 2,
            blocked_items: 0,
            open_questions: 1,
            spend_today_usd: 1.5,
            spend_total_usd: 3.0,
            lavish_available: false,
            autonomy: AutonomyPolicy::resolve(&AutonomyConfig::default(), None, None),
            directives: vec![],
            open_gates: 2,
            held_news: 0,
            mcp: None,
            github: None,
            disk: None,
            versions: None,
            upgrade: None,
            merges_waiting: Vec::new(),
            merge_granted: Vec::new(),
            waiting_total: 0,
            waiting_next_offset: None,
            health: None,
            quota: None,
            recovery: None,
        }
    }

    #[test]
    fn status_says_when_the_orchestrator_runs_a_stale_prompt() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "x".into(),
            retry_at: None,
        });
        up.orchestrator = Some(
            serde_json::from_value(serde_json::json!({
                "agent": "agt_0001", "model": "claude-code/opus", "effort": "high",
                "status": "ready", "restarts": 6, "generation": 7,
                "freshness": {"created_by": "1.2.3", "prompt_outdated": true,
                              "tools_outdated": true, "new_lessons": 1},
            }))
            .unwrap(),
        );
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains(
                "restarted 6 times; generation 7, created by 1.2.3; prompt outdated; \
                 tools outdated; 1 new lesson\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn status_shows_recovery_progress_until_it_is_done() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "x".into(),
            retry_at: None,
        });
        let done = status(&up, &RepoNames::new());
        assert!(!done.contains("recovery:"), "{done}");

        up.recovery = Some(archon_core::protocol::RecoveryProgress {
            runs_done: 40,
            runs_total: Some(120),
        });
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains("\nrecovery: recovering 40/120 runs; changes are refused"),
            "{text}"
        );
    }

    #[test]
    fn status_shows_versions_only_where_they_differ_and_the_upgrade_under_way() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "x".into(),
            retry_at: None,
        });
        let program = |running: &str, installed: &str| ProgramVersions {
            running: Some(running.into()),
            installed: Some(installed.into()),
            latest: None,
        };
        up.versions = Some(Versions {
            archon: program("1.2.2", "1.2.3"),
            aether: program("1.0.7", "1.0.7"),
        });
        up.upgrade = Some("a drained restart: draining, 2 still running, 60s left".into());
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains("\nversions: archon running 1.2.2, installed 1.2.3\n"),
            "{text}"
        );
        assert!(!text.contains("versions: aether"), "{text}");
        assert!(
            text.contains("\nupgrade: a drained restart: draining"),
            "{text}"
        );
    }

    #[test]
    fn status_marks_stale_sections_and_the_waiting_items_left_out() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "x".into(),
            retry_at: None,
        });
        let fresh = archon_core::protocol::ProbeHealth {
            as_of: Some(archon_core::now()),
            last_error: None,
            stale: false,
        };
        let mut health = archon_core::protocol::StatusHealth {
            aether: fresh.clone(),
            mcp: fresh.clone(),
            github: None,
        };
        up.health = Some(health.clone());
        let text = status(&up, &RepoNames::new());
        assert!(!text.contains("health:"), "{text}");
        assert!(!text.contains("more waiting"), "{text}");

        health.aether = archon_core::protocol::ProbeHealth {
            stale: true,
            ..fresh
        };
        up.health = Some(health);
        (up.waiting_total, up.waiting_next_offset) = (75, Some(50));
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains("\nhealth: STALE aether: last answer 0s ago, still asking\n"),
            "{text}"
        );
        assert!(text.contains("25 more waiting item(s) not shown"), "{text}");
    }

    #[test]
    fn status_shows_each_provider_s_quota_headroom() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "x".into(),
            retry_at: None,
        });
        assert!(!status(&up, &RepoNames::new()).contains("quota"));
        let reset = "2026-10-16T18:30:00Z".parse().unwrap();
        up.quota = Some(archon_core::protocol::QuotaStatus {
            as_of: archon_core::now(),
            providers: vec![archon_core::protocol::ProviderQuotaStatus {
                provider: "codex".into(),
                headroom: Some(1.25),
                reserve_percent: 0,
                next_reset: Some(reset),
                windows: vec![],
            }],
        });
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains(
                "\nquota codex: headroom 1.25, reserve 0%, next reset 2026-10-16 18:30 UTC\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn status_says_when_budgets_are_off() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "x".into(),
            retry_at: None,
        });
        let text = status(&up, &RepoNames::new());
        assert!(text.contains("\nbudgets: unlimited per item"), "{text}");
        up.caps.budgets_enabled = false;
        let text = status(&up, &RepoNames::new());
        assert!(text.contains("\nbudgets: off (no dollar budget"), "{text}");
    }

    #[test]
    fn status_shows_free_disk_space_and_why_dispatch_is_held() {
        let gib = 1u64 << 30;
        let mut disk = DiskStatus {
            path: "/data/archon".into(),
            free_bytes: Some(40 * gib),
            error: None,
            min_free_bytes: 10 * gib,
            held: false,
            disk_full: None,
            standby: None,
            gc: None,
            storage: vec![],
        };
        let mut with_disk = status_with(AetherLinkStatus::Disconnected {
            error: "down".into(),
            retry_at: None,
        });
        with_disk.disk = Some(disk.clone());
        let text = status(&with_disk, &RepoNames::new());
        assert!(
            text.contains("\ndisk: 40.0 GiB free at /data/archon, holds below 10.0 GiB\n"),
            "{text}"
        );
        disk.free_bytes = Some(gib / 2);
        disk.held = true;
        assert!(disk_line(&disk).ends_with("; dispatch HELD for low disk space; run `archon gc`"));
        disk.disk_full = Some("database or disk is full".into());
        assert!(disk_line(&disk).contains("HELD after a full disk: database or disk is full"));
        disk.standby = Some("store: disk full: x".into());
        assert!(disk_line(&disk).contains("STANDBY, reads only: store: disk full: x"));
        disk.gc = Some(GcStatus {
            at: archon_core::now() - chrono::TimeDelta::minutes(3),
            bytes_freed: 5 * gib,
            pressure: true,
            headline: "Gc (disk pressure) freed 5.0 GiB: removed 2 worktree(s)".into(),
        });
        assert!(
            disk_line(&disk).contains(
                "; last gc 3m ago: Gc (disk pressure) freed 5.0 GiB: removed 2 worktree(s)"
            ),
            "{}",
            disk_line(&disk)
        );
        disk.free_bytes = None;
        disk.error = Some("permission denied".into());
        disk.min_free_bytes = 0;
        let unknown = disk_line(&disk);
        assert!(unknown.starts_with("free space unknown at /data/archon (permission denied);"));
    }

    #[test]
    fn status_shows_allocated_bytes_by_storage_kind() {
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "down".into(),
            retry_at: None,
        });
        up.disk = Some(
            serde_json::from_value(serde_json::json!({
                "path": "/data", "free_bytes": 1000000, "min_free_bytes": 0, "held": false,
                "storage": [{"kind": "reports_lavish", "path": "/data/reports", "bytes": 8192}]
            }))
            .unwrap(),
        );
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains("storage reports_lavish: 8192 bytes"),
            "{text}"
        );
    }

    #[test]
    fn status_shows_the_mcp_servers_health_and_last_heal() {
        let mut down = status_with(AetherLinkStatus::Disconnected {
            error: "down".into(),
            retry_at: None,
        });
        assert!(
            !status(&down, &RepoNames::new()).contains("\nmcp:"),
            "an older archond reports nothing"
        );
        let healed = archon_core::now() - chrono::TimeDelta::minutes(5);
        down.mcp = Some(McpStatus {
            health: McpHealth::Ready { tools: 22 },
            last_heal: Some(healed),
            failed_refreshes: 0,
            failed_relinks: 0,
        });
        let text = status(&down, &RepoNames::new());
        assert!(
            text.contains("\nmcp: ok (22 tools), last healed 5m ago\n"),
            "{text}"
        );
        let failing = McpStatus {
            health: McpHealth::Failed {
                error: "Broken pipe (os error 32)".into(),
            },
            last_heal: None,
            failed_refreshes: 3,
            failed_relinks: 0,
        };
        assert_eq!(
            mcp_line(&failing, archon_core::now()),
            "FAIL Broken pipe (os error 32), 3 refreshes failed in a row"
        );
        let json = serde_json::to_value(&failing).unwrap();
        assert_eq!(json["health"]["state"], "failed");
        assert_eq!(json["health"]["error"], "Broken pipe (os error 32)");
        assert_eq!(json["failed_refreshes"], 3);
        let unlinked = McpStatus {
            health: McpHealth::Ready { tools: 22 },
            last_heal: Some(healed),
            failed_refreshes: 0,
            failed_relinks: 2,
        };
        assert_eq!(
            mcp_line(&unlinked, archon_core::now()),
            "ok (22 tools), 2 orchestrator restarts failed in a row, last healed 5m ago"
        );
        let mut older = json;
        older.as_object_mut().unwrap().remove("failed_relinks");
        let parsed: McpStatus = serde_json::from_value(older).unwrap();
        assert_eq!(parsed.failed_relinks, 0, "an older archond counts none");
    }

    fn watched(name: &str, target: WatchTarget) -> WatchedRepo {
        WatchedRepo {
            repository: name.into(),
            enabled: true,
            target,
            allowed_users: None,
            excluded_operators: Vec::new(),
            last_poll_at: None,
            last_error: None,
            dispatched: 0,
            ignored: 0,
        }
    }

    #[test]
    fn status_names_repositories_aimed_at_an_old_orchestrator() {
        let old = archon_core::aether_ids::AetherAgentId::parse("agt_0001").unwrap();
        let mut view = GithubWatcherView {
            revision: 3,
            enabled: true,
            mention_regex: archon_core::github::MENTION_REGEX.into(),
            label_regex: archon_core::github::LABEL_REGEX.into(),
            poll_interval_secs: 120,
            orchestrator: None,
            operator_users: None,
            repositories: vec![
                watched("o/a", WatchTarget::Orchestrator),
                watched("o/b", WatchTarget::Stale { agent: Some(old) }),
                watched(
                    "o/t",
                    WatchTarget::Unmanaged {
                        mode: "thread".into(),
                    },
                ),
            ],
        };
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "down".into(),
            retry_at: None,
        });
        up.github = Some(GithubWatch::Watching(Box::new(view.clone())));
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains(
                "\ngithub: on, 2 repositories route @archon to the orchestrator; \
                 MISMATCH o/b still aimed at an old orchestrator, repointing\n"
            ),
            "{text}"
        );
        view.repositories[2].last_error = Some("rate\nlimited".into());
        let listing = github_watcher(&view);
        assert!(
            listing.contains("o/b  on  -> agt_0001 (stale, repointing)"),
            "{listing}"
        );
        assert!(
            listing.contains("thread dispatch, not Archon's"),
            "{listing}"
        );
        assert!(listing.contains("error: rate limited"), "{listing}");
    }

    #[test]
    fn the_watcher_listing_shows_each_repositorys_allowlist() {
        let mut narrowed = watched("example-org/example-repo", WatchTarget::Orchestrator);
        narrowed.allowed_users = Some(vec!["monalisa".into(), "hubot".into()]);
        let view = GithubWatcherView {
            revision: 5,
            enabled: true,
            mention_regex: archon_core::github::MENTION_REGEX.into(),
            label_regex: archon_core::github::LABEL_REGEX.into(),
            poll_interval_secs: 120,
            orchestrator: None,
            operator_users: None,
            repositories: vec![narrowed, watched("o/open", WatchTarget::Orchestrator)],
        };
        let listing = github_watcher(&view);
        let rows: Vec<&str> = listing.lines().skip(5).collect();
        assert_eq!(rows.len(), 2, "{listing}");
        assert!(
            rows[0].starts_with(
                "example-org/example-repo  on  -> orchestrator  only monalisa, hubot  \
                 not polled yet"
            ),
            "{listing}"
        );
        assert!(
            rows[1].starts_with("o/open                    on  -> orchestrator  any writer"),
            "{listing}"
        );
    }

    #[test]
    fn the_watcher_listing_shows_operator_logins_and_where_an_allowlist_bars_them() {
        let mut narrowed = watched("example-org/example-repo", WatchTarget::Orchestrator);
        narrowed.allowed_users = Some(vec!["monalisa".into()]);
        let mut view = GithubWatcherView {
            revision: 6,
            enabled: true,
            mention_regex: archon_core::github::MENTION_REGEX.into(),
            label_regex: archon_core::github::LABEL_REGEX.into(),
            poll_interval_secs: 120,
            orchestrator: None,
            operator_users: None,
            repositories: vec![narrowed, watched("o/open", WatchTarget::Orchestrator)],
        };
        let none = github_watcher(&view);
        assert_eq!(
            none.lines().nth(4),
            Some("operator logins: none (every GitHub request is an external request)"),
            "{none}"
        );
        assert_eq!(none.lines().count(), 7, "{none}");
        let mut up = status_with(AetherLinkStatus::Disconnected {
            error: "down".into(),
            retry_at: None,
        });
        up.github = Some(GithubWatch::Watching(Box::new(view.clone())));
        assert!(!status(&up, &RepoNames::new()).contains("operator logins"));

        view.operator_users = Some(vec!["octocat".into(), "monalisa".into()]);
        view.repositories[0].excluded_operators = vec!["octocat".into()];
        let set = github_watcher(&view);
        assert_eq!(
            set.lines().nth(4),
            Some("operator logins: octocat, monalisa (their own requests are your instructions)"),
            "{set}"
        );
        assert_eq!(
            set.lines().last(),
            Some(
                "operator login octocat is not on example-org/example-repo's allowlist: its requests \
                 are not delivered there at all"
            ),
            "{set}"
        );
        assert_eq!(set.lines().count(), 8, "one note, for the one barred login");
        up.github = Some(GithubWatch::Watching(Box::new(view.clone())));
        let text = status(&up, &RepoNames::new());
        assert!(
            text.contains("to the orchestrator; operator logins: octocat, monalisa\n"),
            "{text}"
        );
        view.repositories.clear();
        let empty = github_watcher(&view);
        assert!(
            empty.contains("\noperator logins: octocat, monalisa ("),
            "{empty}"
        );
    }

    #[test]
    fn status_flags_news_held_for_the_orchestrator() {
        let mut held = status_with(AetherLinkStatus::Disconnected {
            error: "down".into(),
            retry_at: None,
        });
        assert!(!status(&held, &RepoNames::new()).contains("news:"));
        held.held_news = 3;
        let text = status(&held, &RepoNames::new());
        assert!(
            text.contains(
                "\nnews: 3 events held for the orchestrator, delivered once it is reachable\n"
            ),
            "{text}"
        );
        held.held_news = 1;
        assert!(status(&held, &RepoNames::new()).contains("news: 1 event held"));
    }

    #[test]
    fn status_says_whether_aether_is_connected() {
        let down = status_with(AetherLinkStatus::Disconnected {
            error: "connection refused".into(),
            retry_at: None,
        });
        let text = status(&down, &RepoNames::new());
        assert!(
            text.contains("\naether: disconnected (connection refused)\n"),
            "{text}"
        );
        assert!(text.contains("queue: 2 queued"));
        assert!(text.contains("caps: 8 agents, unlimited per repo"));
        assert!(text.contains("pause: nothing paused"));
        assert!(text.contains("\nautonomy default: review=standard, proposal=off,"));
        assert!(text.contains("\ndirectives: 0/20 in force\n"), "{text}");
        assert!(text.contains("questions: 1 open, 2 gates waiting for approval"));
        let up = status_with(AetherLinkStatus::Connected {
            daemon_version: "0.9.0".into(),
            protocol_version: 14,
            instance_id: "i".into(),
        });
        assert!(
            status(&up, &RepoNames::new())
                .contains("aether: connected (aetherd 0.9.0, protocol 14)")
        );
    }

    fn question(number: u64, text: &str, options: &[&str]) -> Question {
        Question {
            id: archon_core::id::QuestionId::new(),
            number,
            text: text.into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            item: None,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        }
    }

    /// No byte a terminal would act on: C0/C1 controls (ESC, BEL, CR, CSI)
    /// or bidi overrides. Newlines are layout.
    pub(super) fn assert_inert(text: &str) {
        let bad = |c: char| (c.is_control() && c != '\n') || ('\u{202a}'..='\u{202e}').contains(&c);
        assert!(!text.chars().any(bad), "{text:?}");
    }

    fn report(state: ReportState, location: Option<&str>) -> ReportSummary {
        ReportSummary {
            report: archon_core::id::ReportId::new(),
            title: "Findings\x1b[31m".into(),
            path: "/w/report.html".into(),
            location: location.map(str::to_owned),
            state,
            item: None,
            reopen_question: None,
            updated_at: archon_core::now(),
        }
    }

    #[test]
    fn reports_show_state_and_location_and_how_to_reopen() {
        assert_eq!(reports(&[]), "No reports.");
        let open = report(ReportState::Open, Some("http://127.0.0.1:4455/s/1"));
        let text = reports(std::slice::from_ref(&open));
        assert!(text.starts_with(open.report.as_str()), "{text}");
        assert!(
            text.contains("  open  http://127.0.0.1:4455/s/1  Findings"),
            "{text}"
        );
        assert!(!text.contains('\x1b') && !text.contains("reopen"), "{text}");
        let closed = report(ReportState::Disconnected, None);
        let text = reports(&[open, closed]);
        assert!(text.contains("  disconnected  /w/report.html"), "{text}");
        let (table, hint) = text.rsplit_once('\n').unwrap();
        assert_eq!(table.lines().count(), 2, "one row per report: {text}");
        assert!(
            hint.starts_with("`archon reports reopen <rpt_…>` shows one again. ")
                && hint.contains("kept while its file exists"),
            "{hint}"
        );

        let published = ReportPublished {
            report: archon_core::id::ReportId::new(),
            published: false,
            location: "/w/report.html".into(),
            run: None,
        };
        let text = reopened(&published);
        assert!(text.starts_with("Lavish did not open rpt_"), "{text}");
        assert!(text.ends_with("the report is at /w/report.html"), "{text}");
    }

    #[test]
    fn questions_show_permanent_numbers_and_strip_escapes() {
        let spoofed = question(
            7,
            "Approve deploy?\x1b[2K\x1b[1GApprove the README typo fix?",
            &["yes\x1b]8;;http://evil\x07", "re\u{202e}tcejer"],
        );
        let text = questions([&spoofed]);
        assert_inert(&text);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "Q7  Approve deploy?[2K[1GApprove the README typo fix?",
                "      1) yes]8;;http://evil",
                "      2) retcejer",
            ]
        );
        let multi = question(12, "Which one?\nSee the page.", &[]);
        assert_eq!(questions([&multi]), "Q12  Which one?\n     See the page.");
        let old = question(0, "asked by an older archond", &[]);
        assert!(questions([&old]).starts_with(&format!("{}  asked", old.id)));
        assert_eq!(questions([]), "No open questions.");
    }

    #[test]
    fn answered_names_the_question_it_resolved() {
        let asked = question(7, "Approve gate C?\x1b[2K\nPage: http://lavish/p/1", &[]);
        let text = answered(&asked, "yes\x1b[31m");
        assert_inert(&text);
        assert_eq!(
            text,
            format!("Answered Q7 ({}): yes[31m\n  Approve gate C?[2K", asked.id)
        );
    }

    #[test]
    fn autonomy_view_lists_policy_and_directives() {
        let policy = AutonomyPolicy::resolve(&AutonomyConfig::default(), None, None);
        let mut view = AutonomyView {
            repo: Some("api".into()),
            policy,
            directives: vec![],
            merge_granted: Vec::new(),
            limits: LimitsConfig::default().structural(),
        };
        let text = autonomy(&view);
        assert!(text.starts_with("autonomy (repo api)\ndefault: review=standard"));
        assert!(
            text.contains("\nceiling: review=strict|standard|none, proposal=lavish|off,"),
            "the ceiling lists what it allows: {text}"
        );
        assert!(
            text.contains(
                "\nlimits:\n  max_delegation_depth = 2 (default 2, ceiling 4; default)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "\n  initiative_contract_bytes = 2048 (default 2048, ceiling 16384; default)\n"
            ),
            "{text}"
        );
        assert!(text.contains("\n  max_directives = 20 (fixed)\n"), "{text}");
        assert!(text.ends_with("directives 0/20: none"), "{text}");
        let now = archon_core::now();
        let directive =
            Directive::new("auto-merge today", DirectiveSource::Operator, Some(8), now).unwrap();
        view.directives.push(directive.clone());
        let text = autonomy(&view);
        assert!(text.contains("\ndirectives 1/20:\n"), "{text}");
        let line = text.lines().last().unwrap();
        assert!(line.contains(&directive.id.to_string()) && line.contains("(operator, until "));
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(duration(42), "42s");
        assert_eq!(duration(3 * 60), "3m");
        assert_eq!(duration(2 * 3600 + 5 * 60), "2h 5m");
        assert_eq!(duration(3 * 86_400 + 3600), "3d 1h");
    }
}
