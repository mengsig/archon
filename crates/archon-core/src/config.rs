//! `~/.config/archon/config.toml` and `<repo>/.archon/config.toml`
//! (ARCHITECTURE §9). Every field is optional; unknown keys are refused so a
//! typo can never silently disable a limit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::age::Age;
use crate::agent_message::{AGENT_MESSAGES_CEILING, DEFAULT_AGENT_MESSAGES};
use crate::autonomy::AutonomyConfig;
use crate::error::ArchonError;
use crate::initiative::InitiativeLimits;
use crate::model::{Effort, ModelRef};
use crate::policy::ModelPolicy;
use crate::queue::Caps;
use crate::repo::ShipMode;

pub const CONFIG_FILE_NAME: &str = "config.toml";

/// `limits.min_free_gb` above this (1 PiB) is surely a typo, and its bytes
/// must fit in `u64`.
pub const MAX_MIN_FREE_GB: u64 = 1 << 20;

/// `limits.max_delegation_depth` by default, and the most it may be: every
/// level multiplies the agents running outside Archon's caps.
pub const DEFAULT_DELEGATION_DEPTH: u32 = 2;
pub const DELEGATION_DEPTH_CEILING: u32 = 4;

/// `artifacts.max_item_mb` above this (1 PiB) is surely a typo.
pub const MAX_ARTIFACT_MB: u64 = 1 << 30;

/// The keys holding filesystem paths, which `Config::load` resolves.
const PATH_KEYS: [&str; 3] = [
    "[aether] socket",
    "[orchestrator] instructions_file",
    "[worktrees] root",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub aether: AetherConfig,
    pub orchestrator: OrchestratorConfig,
    pub models: ModelsConfig,
    pub limits: LimitsConfig,
    pub dispatcher: DispatcherConfig,
    pub chat: ChatConfig,
    pub worktrees: WorktreeConfig,
    pub artifacts: ArtifactsConfig,
    pub lavish: LavishConfig,
    pub github: GithubConfig,
    pub autonomy: AutonomyConfig,
    pub learning: LearningConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LearningConfig {
    pub enabled: bool,
    pub distill_every: u32,
    pub distill_daily_budget_usd: f64,
    pub explore_rate: f64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            distill_every: 20,
            distill_daily_budget_usd: 3.0,
            explore_rate: 0.1,
        }
    }
}

impl LearningConfig {
    pub fn validate(&self) -> Result<(), ArchonError> {
        if !self.distill_daily_budget_usd.is_finite() || self.distill_daily_budget_usd < 0.0 {
            return Err(ArchonError::Config(
                "learning.distill_daily_budget_usd must be finite and nonnegative".into(),
            ));
        }
        if self.distill_every == 0 || self.distill_every > 100 {
            return Err(ArchonError::Config(
                "learning.distill_every must be 1..100".into(),
            ));
        }
        if !self.explore_rate.is_finite() || !(0.0..=0.1).contains(&self.explore_rate) {
            return Err(ArchonError::Config(
                "learning.explore_rate must be between 0 and 0.1".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AetherConfig {
    /// Whose Aether daemon Archon uses (ARCHITECTURE §4.7).
    pub mode: AetherMode,
    /// Shared mode only. Unset: Aether's own socket resolution.
    pub socket: Option<PathBuf>,
    /// `host:port` for TCP instead of the Unix socket.
    pub address: Option<String>,
    pub token_env: String,
    pub host_context: HostContext,
}

impl Default for AetherConfig {
    fn default() -> Self {
        Self {
            mode: AetherMode::default(),
            socket: None,
            address: None,
            token_env: "AETHER_TOKEN".into(),
            host_context: HostContext::Full,
        }
    }
}

/// `dedicated`: Archon runs its own `aetherd` under its own directories, so
/// plain Aether never sees Archon's MCP server. `shared`: the operator's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AetherMode {
    #[default]
    Dedicated,
    Shared,
}

/// Host context requested for newly created agents; full preserves host tools.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HostContext {
    #[default]
    Full,
    Lean,
}

impl HostContext {
    pub fn for_model(self, model: &crate::model::ModelRef) -> Self {
        assert!(!model.provider.is_empty(), "a model names its engine");
        assert!(!model.model.is_empty(), "a model names its variant");
        if model.provider == "claude-code" {
            self
        } else {
            Self::Full
        }
    }

    pub fn is_full(&self) -> bool {
        *self == Self::Full
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lean => "lean",
        }
    }
}

impl AetherConfig {
    /// `socket` and `address` choose the operator's daemon, which only
    /// shared mode uses; in dedicated mode they would be silently ignored.
    pub fn validate(&self) -> Result<(), ArchonError> {
        let endpoint_set = self.socket.is_some() || self.address.is_some();
        if self.mode == AetherMode::Dedicated && endpoint_set {
            return Err(ArchonError::Config(
                "[aether] socket and address need mode = \"shared\"; in the default \
                 dedicated mode Archon runs its own aetherd"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestratorConfig {
    pub model: ModelRef,
    pub effort: Effort,
    pub name: String,
    /// Extra instructions appended to the built-in orchestrator prompt.
    pub instructions_file: Option<PathBuf>,
    pub trigger_coalesce_ms: u64,
    /// Wake the orchestrator for every success, so it reports it in its
    /// chat; off, a routine success waits for its next turn.
    pub wake_on_success: bool,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            model: ModelRef::new("claude-code", "claude-opus-5-5"),
            effort: Effort::High,
            name: "Archon".into(),
            instructions_file: None,
            trigger_coalesce_ms: 2000,
            wake_on_success: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// Empty allows any model Aether lists.
    pub allow: Vec<ModelRef>,
    pub deny: Vec<ModelRef>,
    pub max_effort: Option<Effort>,
}

impl ModelsConfig {
    pub fn policy(&self) -> ModelPolicy {
        ModelPolicy::from_lists(&self.allow, &self.deny, self.max_effort)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// `false` turns every dollar budget off (§7.2); spend is still recorded.
    pub budgets_enabled: bool,
    pub max_parallel_agents: u32,
    pub max_per_repo: u32,
    pub max_active_runs: u32,
    pub budget_usd_per_item: f64,
    pub budget_usd_daily: f64,
    pub budget_usd_total: f64,
    /// Count separate API-equivalent estimates toward budgets. Raw reported
    /// cost always counts, including amounts with a list-price basis.
    pub count_estimated_cost: bool,
    /// Automatic re-spawns after infrastructure failures, per stage run.
    pub infra_retries: u32,
    /// Dispatch holds while the data dir or worktree root has less free
    /// space than this, in GiB; 0 disables the hold (ARCHITECTURE §7.3).
    pub min_free_gb: u64,
    pub per_pipeline: BTreeMap<String, u32>,
    /// Most delegations below a stage agent or the orchestrator (§7.4).
    pub max_delegation_depth: u32,
    /// Messages one agent may send the orchestrator, and one run node's
    /// inbox may relay, over its lifetime.
    pub max_agent_messages: u32,
    pub initiative_members: u32,
    pub initiative_contract_bytes: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        let caps = Caps::default();
        Self {
            budgets_enabled: caps.budgets_enabled,
            max_parallel_agents: caps.max_parallel_agents,
            max_per_repo: caps.max_per_repo,
            max_active_runs: caps.max_active_runs,
            budget_usd_per_item: caps.budget_usd_per_item,
            budget_usd_daily: caps.budget_usd_daily,
            budget_usd_total: caps.budget_usd_total,
            count_estimated_cost: true,
            infra_retries: 1,
            min_free_gb: 10,
            per_pipeline: BTreeMap::new(),
            max_delegation_depth: DEFAULT_DELEGATION_DEPTH,
            max_agent_messages: DEFAULT_AGENT_MESSAGES,
            initiative_members: InitiativeLimits::DEFAULT.members as u32,
            initiative_contract_bytes: InitiativeLimits::DEFAULT.contract_bytes as u32,
        }
    }
}

impl LimitsConfig {
    /// `min_free_gb` in bytes; 0 when the disk hold is off.
    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_gb.saturating_mul(1 << 30)
    }

    /// The size an initiative may have here.
    pub fn initiatives(&self) -> InitiativeLimits {
        let limits = InitiativeLimits {
            members: self.initiative_members as usize,
            contract_bytes: self.initiative_contract_bytes as usize,
        };
        assert!(
            limits.members >= 1 && limits.contract_bytes >= 1,
            "validated"
        );
        assert!(limits.within(&InitiativeLimits::CEILING), "validated");
        limits
    }

    fn validate(&self) -> Result<(), ArchonError> {
        let budgets = [
            self.budget_usd_per_item,
            self.budget_usd_daily,
            self.budget_usd_total,
        ];
        if budgets.iter().any(|b| !b.is_finite() || *b < 0.0) {
            return Err(ArchonError::Config(
                "limits budgets must be finite and >= 0 (0 = unlimited)".into(),
            ));
        }
        if self.min_free_gb > MAX_MIN_FREE_GB {
            return Err(ArchonError::Config(format!(
                "limits.min_free_gb must be at most {MAX_MIN_FREE_GB} (0 = no disk hold)"
            )));
        }
        let ceiling = InitiativeLimits::CEILING;
        let structural = [
            (
                "max_delegation_depth",
                self.max_delegation_depth,
                DELEGATION_DEPTH_CEILING,
            ),
            (
                "max_agent_messages",
                self.max_agent_messages,
                AGENT_MESSAGES_CEILING,
            ),
            (
                "initiative_members",
                self.initiative_members,
                ceiling.members as u32,
            ),
            (
                "initiative_contract_bytes",
                self.initiative_contract_bytes,
                ceiling.contract_bytes as u32,
            ),
        ];
        for (key, value, most) in structural {
            if !(1..=most).contains(&value) {
                return Err(ArchonError::Config(format!(
                    "limits.{key} must be between 1 and {most}, not {value}"
                )));
            }
        }
        Ok(())
    }

    pub fn caps(&self) -> Caps {
        Caps {
            budgets_enabled: self.budgets_enabled,
            max_parallel_agents: self.max_parallel_agents,
            max_per_repo: self.max_per_repo,
            max_per_pipeline: self.per_pipeline.clone(),
            max_active_runs: self.max_active_runs,
            budget_usd_total: self.budget_usd_total,
            budget_usd_daily: self.budget_usd_daily,
            budget_usd_per_item: self.budget_usd_per_item,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatcherConfig {
    pub tick_secs: u64,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self { tick_secs: 15 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    pub show_progress: bool,
    pub history: usize,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            show_progress: false,
            history: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// Unset: `<data>/worktrees`.
    pub root: Option<PathBuf>,
    pub branch_prefix: String,
    pub keep_on_success: bool,
    /// archond's periodic gc pass (ARCHITECTURE §13.2); `false` turns it off.
    pub auto_gc: bool,
    /// A finished run's worktree is removed this long after it finished.
    pub gc_after: Age,
    /// A finished run's build caches are deleted this long after it finished.
    pub cache_after: Age,
    /// The caches to delete, relative to a worktree; empty: every file git
    /// ignores.
    pub cache_dirs: Vec<PathBuf>,
    /// Stages build into one cache per repository under `<data>/cache`
    /// instead of a target directory per task; `false` turns it off for
    /// every repository (a repository's `[cache] shared = false` for one).
    pub shared_cache: bool,
    /// A repository's shared cache nobody used for this long, with no live
    /// run on the repository, is deleted.
    pub shared_cache_after: Age,
    /// While dispatch is held for disk space, finished runs' worktrees and
    /// idle shared caches go after this instead, if it is shorter.
    pub pressure_after: Age,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            root: None,
            branch_prefix: "archon/".into(),
            keep_on_success: false,
            auto_gc: true,
            gc_after: Age::from_secs(7 * 86_400),
            cache_after: Age::from_secs(3_600),
            cache_dirs: Vec::new(),
            shared_cache: true,
            shared_cache_after: Age::from_secs(14 * 86_400),
            pressure_after: Age::from_secs(86_400),
        }
    }
}

/// The durable per-item artifact store (ARCHITECTURE §13.5): files a stage
/// publishes outlive its worktree, read by item id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArtifactsConfig {
    /// The most one published file may take.
    pub max_file_mb: u64,
    /// The most one item's artifacts may take together.
    pub max_item_mb: u64,
    /// An item's artifacts are deleted this long after the item settled.
    pub keep_after: Age,
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            max_file_mb: 256,
            max_item_mb: 1024,
            keep_after: Age::from_secs(30 * 86_400),
        }
    }
}

impl ArtifactsConfig {
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb.saturating_mul(1 << 20)
    }

    pub fn max_item_bytes(&self) -> u64 {
        self.max_item_mb.saturating_mul(1 << 20)
    }

    fn validate(&self) -> Result<(), ArchonError> {
        let sane = |mb: u64| (1..=MAX_ARTIFACT_MB).contains(&mb);
        if !sane(self.max_file_mb) || !sane(self.max_item_mb) {
            return Err(ArchonError::Config(format!(
                "artifacts.max_file_mb and artifacts.max_item_mb must be 1..{MAX_ARTIFACT_MB}"
            )));
        }
        if self.max_file_mb > self.max_item_mb {
            return Err(ArchonError::Config(
                "artifacts.max_file_mb must not exceed artifacts.max_item_mb".into(),
            ));
        }
        Ok(())
    }
}

impl WorktreeConfig {
    /// Each cache directory names a place inside a worktree, and not git's own.
    fn validate(&self) -> Result<(), ArchonError> {
        for dir in &self.cache_dirs {
            let inside = dir.components().all(|c| match c {
                std::path::Component::Normal(name) => name != ".git",
                _ => false,
            });
            if dir.as_os_str().is_empty() || !inside {
                return Err(ArchonError::Config(format!(
                    "worktrees.cache_dirs: {} must be a relative path inside a worktree, \
                     without `..` or `.git`",
                    dir.display()
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Toggle {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LavishConfig {
    pub enabled: Toggle,
    pub binary: PathBuf,
}

impl Default for LavishConfig {
    fn default() -> Self {
        Self {
            enabled: Toggle::Auto,
            binary: "lavish-axi".into(),
        }
    }
}

/// How `github_reply` posts (docs/github-watcher.md). Aether's watcher
/// reads GitHub through its own `gh`; both use archond's login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    pub binary: PathBuf,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            binary: "gh".into(),
        }
    }
}

/// `<repo>/.archon/config.toml`. May only narrow global limits and models.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    pub repo: RepoDefaults,
    pub limits: RepoLimits,
    pub models: ModelsConfig,
    pub pipelines: RepoPipelines,
    /// May only lower the defaults and the ceiling (ARCHITECTURE §18).
    pub autonomy: AutonomyConfig,
    pub cache: RepoCache,
}

/// A repository's say over its build cache (ARCHITECTURE §13.5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoCache {
    /// `false` opts the repository out of the shared build cache.
    pub shared: Option<bool>,
}

impl RepoConfig {
    /// Whether this repository's stages build into the shared cache: the
    /// global switch, unless the repository opts out.
    pub fn shared_cache(&self, worktrees: &WorktreeConfig) -> bool {
        worktrees.shared_cache && self.cache.shared.unwrap_or(true)
    }
}

/// Defaults for registry fields; registry values set by the operator win.
/// There is deliberately no `auto_merge`: a repository can only lower its
/// autonomy, never opt into merging.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoDefaults {
    pub ship: Option<ShipMode>,
    pub base_branch: Option<String>,
    pub default_pipeline: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoLimits {
    pub max_per_repo: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoPipelines {
    pub disabled: Vec<String>,
}

impl Config {
    /// A missing file is the default configuration; a malformed one is an
    /// error naming the file.
    pub fn load(path: &Path) -> Result<Self, ArchonError> {
        let mut config: Self = read_toml(path)?.unwrap_or_default();
        let dir = std::path::absolute(path)
            .map_err(|e| ArchonError::io(format!("resolving {}", path.display()), e))?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| ArchonError::Config(format!("{} has no directory", path.display())))?;
        config.resolve_paths(&dir, dirs::home_dir().as_deref())?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_default() -> Result<Self, ArchonError> {
        Self::load(&crate::paths::config_dir().join(CONFIG_FILE_NAME))
    }

    /// Filesystem paths, so they mean the same in every process whatever its
    /// working directory: `~/` expands to `home`, relative paths join `dir`.
    fn resolve_paths(&mut self, dir: &Path, home: Option<&Path>) -> Result<(), ArchonError> {
        assert!(
            dir.is_absolute(),
            "paths resolve against an absolute directory"
        );
        for (key, path) in PATH_KEYS.into_iter().zip(self.paths_mut()) {
            if let Some(path) = path {
                *path = resolve_path(path, dir, home)
                    .map_err(|why| ArchonError::Config(format!("{key}: {why}")))?;
            }
        }
        debug_assert!(self.paths_absolute().is_ok());
        Ok(())
    }

    /// In `PATH_KEYS` order.
    fn paths(&self) -> [&Option<PathBuf>; 3] {
        [
            &self.aether.socket,
            &self.orchestrator.instructions_file,
            &self.worktrees.root,
        ]
    }

    fn paths_mut(&mut self) -> [&mut Option<PathBuf>; 3] {
        [
            &mut self.aether.socket,
            &mut self.orchestrator.instructions_file,
            &mut self.worktrees.root,
        ]
    }

    /// A relative path left here would resolve against whichever directory
    /// the reading process happens to run in.
    fn paths_absolute(&self) -> Result<(), ArchonError> {
        for (key, path) in PATH_KEYS.into_iter().zip(self.paths()) {
            if let Some(path) = path.as_ref().filter(|p| !p.is_absolute()) {
                return Err(ArchonError::Config(format!(
                    "{key} {} must be an absolute path",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ArchonError> {
        self.learning.validate()?;
        self.aether.validate()?;
        self.paths_absolute()?;
        if self.dispatcher.tick_secs == 0 {
            return Err(ArchonError::Config(
                "dispatcher.tick_secs must be > 0".into(),
            ));
        }
        self.limits.validate()?;
        self.autonomy.validate()?;
        self.worktrees.validate()?;
        self.artifacts.validate()?;
        let orchestrator = crate::model::ModelChoice {
            model: self.orchestrator.model.clone(),
            effort: self.orchestrator.effort,
        };
        self.models
            .policy()
            .check(&orchestrator)
            .map_err(|e| ArchonError::Config(format!("orchestrator.model violates [models]: {e}")))
    }
}

impl RepoConfig {
    pub fn load(repo_root: &Path) -> Result<Self, ArchonError> {
        let path = crate::paths::repo_archon_dir(repo_root).join(CONFIG_FILE_NAME);
        Ok(read_toml(&path)?.unwrap_or_default())
    }

    /// Global caps with this repository's narrowing applied.
    pub fn max_per_repo(&self, global: u32) -> u32 {
        match (self.limits.max_per_repo, global) {
            (None, g) => g,
            (Some(local), 0) => local,
            (Some(0), g) => g,
            (Some(local), g) => local.min(g),
        }
    }
}

/// `~` or `~/rest` becomes `home` or `home/rest`, another relative path
/// joins `dir`, and an absolute path stays as it is. `~user/` is refused.
fn resolve_path(path: &Path, dir: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path.as_os_str().is_empty() {
        return Err("the path is empty".into());
    }
    if let Ok(rest) = path.strip_prefix("~") {
        let home = home.ok_or_else(|| {
            format!(
                "{} starts with ~ but the home directory is unknown",
                path.display()
            )
        })?;
        assert!(home.is_absolute(), "the home directory is absolute");
        if rest.as_os_str().is_empty() {
            return Ok(home.to_path_buf());
        }
        return Ok(home.join(rest));
    }
    if path.as_os_str().as_encoded_bytes().starts_with(b"~") {
        return Err(format!(
            "{}: only ~/ is expanded, not ~user/; use an absolute path",
            path.display()
        ));
    }
    Ok(dir.join(path))
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, ArchonError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ArchonError::io(format!("reading {}", path.display()), e)),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| ArchonError::Config(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.limits.caps().max_parallel_agents, 8);
        assert!(config.orchestrator.wake_on_success);
    }

    #[test]
    fn budgets_are_on_unless_switched_off() {
        assert!(Config::default().limits.caps().budgets_enabled);
        let config: Config = toml::from_str("[limits]\nbudgets_enabled = false").unwrap();
        config.validate().unwrap();
        assert!(!config.limits.caps().budgets_enabled);
        let mut caps = serde_json::to_value(Caps::default()).unwrap();
        caps.as_object_mut().unwrap().remove("budgets_enabled");
        let caps: Caps = serde_json::from_value(caps).unwrap();
        assert!(caps.budgets_enabled, "a Caps without the switch enforces");
    }

    #[test]
    fn parses_documented_example_and_rejects_unknown_keys() {
        let text = r#"
            [orchestrator]
            model = "claude-code/claude-opus-5-5"
            effort = "x_high"
            [models]
            allow = ["claude-code/claude-opus-5-5"]
            [limits]
            max_parallel_agents = 20
            [limits.per_pipeline]
            adaptive = 3
            [lavish]
            enabled = "off"
            [github]
            binary = "/usr/bin/gh"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.orchestrator.effort, Effort::XHigh);
        assert_eq!(config.limits.per_pipeline["adaptive"], 3);
        assert_eq!(config.github.binary, PathBuf::from("/usr/bin/gh"));
        assert!(toml::from_str::<Config>("[limits]\nmax_paralel_agents = 3").is_err());
    }

    #[test]
    fn config_paths_resolve_against_home_and_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let text = "[aether]\nmode = \"shared\"\nsocket = \"run/a.sock\"\n\
                    [orchestrator]\ninstructions_file = \"~/orchestrator.md\"\n\
                    [worktrees]\nroot = \"/srv/trees\"\n";
        std::fs::write(&file, text).unwrap();
        let config = Config::load(&file).unwrap();
        assert_eq!(config.aether.socket, Some(dir.path().join("run/a.sock")));
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            config.orchestrator.instructions_file,
            Some(home.join("orchestrator.md"))
        );
        assert_eq!(config.worktrees.root, Some(PathBuf::from("/srv/trees")));

        std::fs::write(&file, "[worktrees]\nroot = \"~other/trees\"\n").unwrap();
        let refused = Config::load(&file).unwrap_err().to_string();
        assert!(refused.contains("[worktrees] root"), "{refused}");
    }

    #[test]
    fn a_config_path_is_absolute_home_relative_or_config_relative() {
        let (dir, home) = (Path::new("/etc/archon"), Path::new("/home/op"));
        let resolve = |path: &str| resolve_path(Path::new(path), dir, Some(home));
        assert_eq!(resolve("/srv/x").unwrap(), Path::new("/srv/x"));
        assert_eq!(resolve("~").unwrap(), Path::new("/home/op"));
        assert_eq!(resolve("~/a/b").unwrap(), Path::new("/home/op/a/b"));
        assert_eq!(resolve("trees").unwrap(), Path::new("/etc/archon/trees"));
        assert_eq!(resolve("./t").unwrap(), Path::new("/etc/archon/t"));
        assert!(resolve("~op/a").is_err(), "~user is not expanded");
        assert!(resolve("").is_err());
        assert!(resolve_path(Path::new("~/a"), dir, None).is_err());
    }

    #[test]
    fn a_relative_path_that_skipped_loading_is_refused() {
        let config: Config = toml::from_str("[worktrees]\nroot = \"trees\"").unwrap();
        assert!(matches!(config.validate(), Err(ArchonError::Config(_))));
        let absolute: Config = toml::from_str("[worktrees]\nroot = \"/srv/t\"").unwrap();
        absolute.validate().unwrap();
    }

    #[test]
    fn the_disk_hold_defaults_to_ten_gib_and_zero_turns_it_off() {
        let config = Config::default();
        assert_eq!(config.limits.min_free_gb, 10);
        assert_eq!(config.limits.min_free_bytes(), 10 << 30);
        let off: Config = toml::from_str("[limits]\nmin_free_gb = 0").unwrap();
        off.validate().unwrap();
        assert_eq!(off.limits.min_free_bytes(), 0);
        let huge = format!("[limits]\nmin_free_gb = {}", MAX_MIN_FREE_GB + 1);
        let refused: Config = toml::from_str(&huge).unwrap();
        assert!(matches!(refused.validate(), Err(ArchonError::Config(_))));
    }

    #[test]
    fn structural_ceilings_default_to_their_old_values_and_are_bounded() {
        let limits = Config::default().limits;
        assert_eq!(limits.max_delegation_depth, 2);
        assert_eq!(limits.max_agent_messages, 20);
        assert_eq!(
            limits.initiatives(),
            InitiativeLimits {
                members: 16,
                contract_bytes: 2048
            }
        );
        let raised: Config = toml::from_str(
            "[limits]\nmax_delegation_depth = 3\nmax_agent_messages = 50\n\
             initiative_members = 32\ninitiative_contract_bytes = 8192",
        )
        .unwrap();
        raised.validate().unwrap();
        assert_eq!(raised.limits.initiatives().contract_bytes, 8192);
        for wrong in [
            "max_delegation_depth = 0",
            "max_delegation_depth = 5",
            "max_agent_messages = 0",
            "initiative_members = 65",
            "initiative_contract_bytes = 16385",
        ] {
            let config: Config = toml::from_str(&format!("[limits]\n{wrong}")).unwrap();
            let Err(ArchonError::Config(error)) = config.validate() else {
                panic!("{wrong} was accepted");
            };
            let key = wrong.split(' ').next().unwrap();
            assert!(
                error.contains(&format!("limits.{key} must be between 1 and")),
                "{error}"
            );
        }
    }

    #[test]
    fn worktree_gc_settings_default_parse_and_refuse_escaping_cache_dirs() {
        let config = Config::default();
        assert!(config.worktrees.auto_gc);
        assert_eq!(config.worktrees.gc_after.secs(), 7 * 86_400);
        assert_eq!(config.worktrees.cache_after.secs(), 3_600);
        let text = "[worktrees]\nauto_gc = false\ngc_after = \"2w\"\ncache_after = \"30m\"\n\
                    cache_dirs = [\"target\", \"web/node_modules\"]";
        let set: Config = toml::from_str(text).unwrap();
        set.validate().unwrap();
        assert!(!set.worktrees.auto_gc);
        assert_eq!(set.worktrees.gc_after.secs(), 14 * 86_400);
        assert_eq!(set.worktrees.cache_dirs.len(), 2);
        assert!(toml::from_str::<Config>("[worktrees]\ngc_after = \"7\"").is_err());
        for dir in ["../x", "/abs", "a/.git", ""] {
            let text = format!("[worktrees]\ncache_dirs = [{dir:?}]");
            let refused: Config = toml::from_str(&text).unwrap();
            assert!(
                matches!(refused.validate(), Err(ArchonError::Config(_))),
                "{dir}"
            );
        }
    }

    #[test]
    fn orchestrator_outside_allowlist_is_refused() {
        let text = r#"
            [orchestrator]
            model = "codex/gpt-5"
            [models]
            allow = ["claude-code/claude-opus-5-5"]
        "#;
        let config: Config = toml::from_str(text).unwrap();
        assert!(matches!(config.validate(), Err(ArchonError::Config(_))));
    }

    #[test]
    fn host_context_defaults_to_full_and_rejects_unknown_values() {
        let defaults: Config = toml::from_str("").unwrap();
        assert_eq!(defaults.aether.host_context, HostContext::Full);
        for (text, expected) in [("full", HostContext::Full), ("lean", HostContext::Lean)] {
            let config: Config =
                toml::from_str(&format!("[aether]\nhost_context = \"{text}\"\n")).unwrap();
            assert_eq!(config.aether.host_context, expected);
            assert!(config.validate().is_ok());
        }
        for bad in ["minimal", "Lean", "", "auto"] {
            assert!(
                toml::from_str::<Config>(&format!("[aether]\nhost_context = \"{bad}\"\n")).is_err()
            );
        }
        assert!(toml::from_str::<Config>("[aether]\nhost_context = true").is_err());
    }

    #[test]
    fn aether_mode_defaults_to_dedicated_and_only_shared_takes_an_endpoint() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.aether.mode, AetherMode::Dedicated);
        let shared: Config =
            toml::from_str("[aether]\nmode = \"shared\"\nsocket = \"/run/a.sock\"").unwrap();
        shared.validate().unwrap();
        let dedicated: Config = toml::from_str("[aether]\nsocket = \"/run/a.sock\"").unwrap();
        assert!(matches!(dedicated.validate(), Err(ArchonError::Config(_))));
        assert!(toml::from_str::<Config>("[aether]\nmode = \"global\"").is_err());
    }

    #[test]
    fn repo_config_cannot_opt_into_auto_merge_and_only_lowers_caps() {
        assert!(toml::from_str::<RepoConfig>("[repo]\nauto_merge = true").is_err());
        let repo: RepoConfig = toml::from_str("[limits]\nmax_per_repo = 2").unwrap();
        assert_eq!(repo.max_per_repo(5), 2);
        assert_eq!(repo.max_per_repo(1), 1);
        assert_eq!(repo.max_per_repo(0), 2);
    }

    #[test]
    fn autonomy_tables_parse_in_both_files() {
        let text = "[autonomy.default]\nreview = \"strict\"\n[autonomy.max]\nmerge = \"auto\"";
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.autonomy.max.merge,
            Some(crate::autonomy::MergeMode::Auto)
        );
        let repo: RepoConfig = toml::from_str("[autonomy.max]\ntools = \"guarded\"").unwrap();
        assert_eq!(
            repo.autonomy.max.tools,
            Some(crate::autonomy::ToolsMode::Guarded)
        );
        assert!(toml::from_str::<Config>("[autonomy]\npreset = \"yolo\"").is_err());
        let over = "[autonomy.default]\nmerge = \"auto\"";
        let refused: Config = toml::from_str(over).unwrap();
        assert!(matches!(refused.validate(), Err(ArchonError::Config(_))));
    }
}
