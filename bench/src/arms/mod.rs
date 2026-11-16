//! The four arms (ARCHITECTURE §15). Everything that decides *what* an arm
//! runs is a pure function here, so `--dry-run` prints exactly what a real
//! run would execute and tests can pin the command lines.

pub mod aether;
pub mod archon;
pub mod cli;

use std::path::{Path, PathBuf};

use anyhow::Context;
use archon_aether::wire::{
    AgentConfig, ApprovalPolicy, CreateAgent, NetworkPolicy, Scope, ToolExposure,
};
use archon_core::config::{AetherMode, Config, Toggle};
use archon_core::model::{Effort, ModelRef};
use archon_core::{Timestamp, mcp_tools};

use crate::grade::ResultSource;
use crate::isolation::EnvScrub;
use crate::metrics::Arm;
use crate::process::CommandSpec;
use crate::usage::{PriceArg, Usage};

/// Variables that make a nested `claude` believe it runs inside another
/// Claude Code session; a benchmark started from one must not leak them.
const NESTED_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
];

/// Binaries, models and effort for every arm, fixed for a whole run.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmSettings {
    pub claude_bin: PathBuf,
    pub codex_bin: PathBuf,
    pub archon_bin: PathBuf,
    /// Set only once the private sandbox is validated; without it the
    /// daemon arms fail instead of falling back to a default socket.
    pub sockets: Option<PrivateSockets>,
    pub archon_model: ModelRef,
    pub aether_model: ModelRef,
    /// The `claude --model` value.
    pub claude_model: String,
    /// The `codex exec -m` value.
    pub codex_model: String,
    pub effort: Effort,
    pub prices: Vec<PriceArg>,
    /// Applied to every process the bench starts on an agent's behalf.
    pub env: EnvScrub,
}

/// The benchmark's own daemons, both inside the private sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateSockets {
    pub aether: PathBuf,
    /// Equals `ARCHON_SOCKET`: the orchestrator's MCP proxy dials that one.
    pub archon: PathBuf,
}

impl ArmSettings {
    /// The validated sandbox sockets; the daemon arms need them.
    pub fn private_sockets(&self) -> anyhow::Result<&PrivateSockets> {
        let sockets = self.sockets.as_ref().context(
            "no private sandbox: the daemon arms need --aether-socket and ARCHON_SOCKET \
             validated by the preflight",
        )?;
        assert!(
            sockets.aether.is_absolute() && sockets.archon.is_absolute(),
            "validated sandbox sockets are absolute"
        );
        assert_ne!(sockets.aether, sockets.archon, "the daemons share a socket");
        Ok(sockets)
    }

    /// The model string recorded in results for `arm`.
    pub fn model_label(&self, arm: Arm) -> String {
        match arm {
            Arm::Archon => self.archon_model.to_string(),
            Arm::Aether => self.aether_model.to_string(),
            Arm::Claude => format!("claude-code/{}", self.claude_model),
            Arm::Codex => format!("codex/{}", self.codex_model),
        }
    }

    /// Fill in an estimate from `--price` when the arm reported no cost.
    pub fn price(&self, arm: Arm, mut usage: Usage) -> Usage {
        if usage.cost_usd.is_some() || usage.estimated_cost_usd.is_some() {
            return usage;
        }
        let label = self.model_label(arm);
        let bare = label.split_once('/').map_or(label.as_str(), |(_, m)| m);
        let price = self
            .prices
            .iter()
            .find(|p| p.model == label || p.model == bare);
        usage.estimated_cost_usd = price.map(|p| p.price.estimate(&usage));
        usage
    }
}

/// What one arm produced for one trial, before grading.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmOutcome {
    pub usage: Option<Usage>,
    /// Everything the operator reads at the end (answers, replies, outcomes).
    pub answer: String,
    pub operator_messages: u32,
    pub wall_secs: f64,
    pub setup_secs: Option<f64>,
    pub timed_out: bool,
    /// Harness failure: the arm did not run cleanly.
    pub error: Option<String>,
    pub source: ResultSource,
    /// Only when `timed_out`: `source` plus the last committed branch of each
    /// timed-out item, graded separately and never as the verdict.
    pub timeout_source: Option<ResultSource>,
}

impl ArmOutcome {
    /// An arm that could not even start; graded as-is (it will fail).
    pub fn failed(error: String, source: ResultSource) -> Self {
        Self {
            usage: None,
            answer: String::new(),
            operator_messages: 0,
            wall_secs: 0.0,
            setup_secs: None,
            timed_out: false,
            error: Some(error),
            source,
            timeout_source: None,
        }
    }
}

/// Everything an arm needs to run one trial.
#[derive(Debug, Clone, Copy)]
pub struct TrialInput<'a> {
    pub task: &'a crate::task::BenchTask,
    pub layout: &'a crate::workspace::TrialLayout,
    pub settings: &'a ArmSettings,
    pub timeout: std::time::Duration,
    /// Unique per run and trial; used as the Aether idempotency key.
    pub key: &'a str,
}

/// `claude -p <prompt> --output-format json --model <m> --effort <e>
/// --dangerously-skip-permissions` in the agent's working directory; git
/// discovery stops at the trial `workspace`.
pub fn claude_command(
    settings: &ArmSettings,
    prompt: &str,
    cwd: &Path,
    workspace: &Path,
) -> CommandSpec {
    assert!(!prompt.trim().is_empty(), "claude needs a prompt");
    assert!(
        !prompt.starts_with('-'),
        "a prompt must not parse as a flag"
    );
    let mut spec = CommandSpec::new(&settings.claude_bin, cwd);
    spec.args = [
        "-p",
        prompt,
        "--output-format",
        "json",
        "--model",
        &settings.claude_model,
        "--effort",
        claude_effort(settings.effort),
        "--dangerously-skip-permissions",
        "--no-session-persistence",
    ]
    .map(str::to_owned)
    .to_vec();
    agent_env(&mut spec, settings, workspace);
    spec
}

/// `codex exec --json` with approvals and sandbox bypassed, the parity
/// setting to `claude --dangerously-skip-permissions`. (Codex 0.155 has no
/// `--full-auto`; the workspace-write sandbox would also block `.git`.)
pub fn codex_command(
    settings: &ArmSettings,
    prompt: &str,
    cwd: &Path,
    workspace: &Path,
) -> CommandSpec {
    assert!(!prompt.trim().is_empty(), "codex needs a prompt");
    assert!(
        !prompt.starts_with('-'),
        "a prompt must not parse as a flag"
    );
    let effort = format!(
        "model_reasoning_effort=\"{}\"",
        codex_effort(settings.effort)
    );
    let mut spec = CommandSpec::new(&settings.codex_bin, cwd);
    spec.args = [
        "exec",
        "--json",
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--ephemeral",
        "-m",
        &settings.codex_model,
        "-c",
        &effort,
        "-C",
        &cwd.to_string_lossy(),
        prompt,
    ]
    .map(str::to_owned)
    .to_vec();
    agent_env(&mut spec, settings, workspace);
    spec
}

/// No nested-session markers, nothing pointing at the bench's own checkout,
/// and git discovery that stops at the trial workspace.
fn agent_env(spec: &mut CommandSpec, settings: &ArmSettings, workspace: &Path) {
    assert!(
        spec.cwd.starts_with(workspace),
        "an agent runs inside its trial workspace"
    );
    spec.env_remove = NESTED_SESSION_ENV.iter().map(|s| (*s).to_owned()).collect();
    spec.env = Vec::new();
    settings.env.apply(spec);
    spec.env.push((
        "GIT_CEILING_DIRECTORIES".into(),
        workspace.to_string_lossy().into_owned(),
    ));
}

pub fn claude_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// Codex tops out at `xhigh`, so `max` maps there.
pub fn codex_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh | Effort::Max => "xhigh",
    }
}

/// One plain root agent: the task prompt as its objective, a leaf (no
/// delegation, so it stays a single agent), Archon's MCP tools hidden. It has
/// network and no approvals, like the unattended claude and codex arms.
pub fn aether_request(input: &TrialInput<'_>, deadline: Timestamp) -> CreateAgent {
    let task = &input.task.file;
    CreateAgent {
        role: "archon-bench".into(),
        references: Vec::new(),
        name: Some(format!("bench:{}", input.key)),
        objective: Some(task.prompt.clone()),
        model: Some(input.settings.aether_model.clone()),
        scope: Some(Scope {
            workspace_root: Some(input.layout.agent_cwd(input.task)),
            repository: None,
            network: NetworkPolicy::Allowed,
            labels: [("archon-bench".to_owned(), task.id.clone())].into(),
        }),
        repository_root: None,
        parent: None,
        config: Some(AgentConfig {
            host_context: Default::default(),
            instructions: None,
            effort: Some(input.settings.effort),
            cost_budget: None,
            deadline: Some(deadline),
            tools: ToolExposure::Except {
                names: mcp_tools::qualified_names().into_iter().collect(),
            },
            long_lived: false,
            leaf: true,
            delegation_policy: None,
        }),
        approval: Some(ApprovalPolicy::none()),
        run: None,
        start: true,
        idempotency_key: Some(format!("archon-bench-{}", input.key)),
    }
}

/// The private archond's `config.toml`: the same model everywhere (the
/// allowlist holds only it), effort capped at the run's effort, Lavish off
/// so no review windows open, and worktrees kept for grading.
pub fn archon_config(settings: &ArmSettings) -> Config {
    let mut config = Config::default();
    // The bench's archond uses the Aether it was pointed at, not its own.
    config.aether.mode = AetherMode::Shared;
    config.aether.socket = settings.sockets.as_ref().map(|s| s.aether.clone());
    config.orchestrator.model = settings.archon_model.clone();
    config.orchestrator.effort = settings.effort;
    config.models.allow = vec![settings.archon_model.clone()];
    config.models.max_effort = Some(settings.effort);
    config.lavish.enabled = Toggle::Off;
    config.worktrees.keep_on_success = true;
    config
}

pub fn archon_config_toml(settings: &ArmSettings) -> anyhow::Result<String> {
    let config = archon_config(settings);
    config
        .validate()
        .context("the generated archon config is invalid")?;
    let mut document = toml::Table::try_from(&config)?;
    // Unrelated new defaults can make an older release reject the config.
    // Write only the sections the harness configures.
    document.retain(|name, _| {
        matches!(
            name,
            "aether" | "orchestrator" | "models" | "lavish" | "worktrees"
        )
    });
    let text = toml::to_string(&document).context("serializing the archon config")?;
    let parsed: Config = toml::from_str(&text).context("checking the generated archon config")?;
    assert_eq!(
        parsed, config,
        "omitting defaults must preserve every override"
    );
    Ok(text)
}

/// `archon daemon run` in the foreground, owned by the bench, on the
/// default socket (the orchestrator's MCP proxy can only dial that one).
pub fn archon_daemon_command(
    settings: &ArmSettings,
    archon_dir: &Path,
    socket: &Path,
) -> CommandSpec {
    let mut spec = CommandSpec::new(&settings.archon_bin, archon_dir);
    spec.args = vec!["daemon".into(), "run".into()];
    spec.env = vec![
        (
            "ARCHON_CONFIG_DIR".into(),
            archon_dir.join("config").to_string_lossy().into_owned(),
        ),
        (
            "ARCHON_DATA_DIR".into(),
            archon_dir.join("data").to_string_lossy().into_owned(),
        ),
        (
            "ARCHON_SOCKET".into(),
            socket.to_string_lossy().into_owned(),
        ),
    ];
    settings.env.apply(&mut spec);
    spec
}

#[cfg(test)]
pub(crate) mod tests;
