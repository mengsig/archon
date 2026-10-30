//! `~/.config/archon/config.toml` and `<repo>/.archon/config.toml`
//! (ARCHITECTURE §9). Every field is optional; unknown keys are refused so a
//! typo can never silently disable a limit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::age::Age;
use crate::agent_message::{AGENT_MESSAGES_CEILING, DEFAULT_AGENT_MESSAGES};
use crate::autonomy::{AutonomyConfig, MAX_DIRECTIVE_CHARS, MAX_DIRECTIVES};
use crate::error::ArchonError;
use crate::initiative::InitiativeLimits;
use crate::model::{Effort, ModelRef};
use crate::model_tier::Tier;
use crate::policy::ModelPolicy;
use crate::provider_route::{DEFAULT_PROVIDER_WEIGHT, ProviderWeights};
use crate::queue::Caps;
use crate::quota::Reserves;
use crate::repo::ShipMode;

pub const CONFIG_FILE_NAME: &str = "config.toml";

/// `limits.min_free_gb` above this (1 PiB) is surely a typo, and its bytes
/// must fit in `u64`.
pub const MAX_MIN_FREE_GB: u64 = 1 << 20;

/// `limits.max_delegation_depth` by default, and the most it may be: every
/// level multiplies the agents running outside Archon's caps.
pub const DEFAULT_DELEGATION_DEPTH: u32 = 2;
pub const DELEGATION_DEPTH_CEILING: u32 = 4;

/// `limits.stall_minutes` by default.
pub const DEFAULT_STALL_MINUTES: u32 = 20;
/// `limits.stall_fallback_minutes` by default.
pub const DEFAULT_STALL_FALLBACK_MINUTES: u32 = 45;

/// `limits.max_stage_timeout_minutes` by default, and the most it may be (30
/// days, a pipeline stage's own bound).
pub const DEFAULT_STAGE_TIMEOUT_MINUTES: u32 = 24 * 60;
pub const STAGE_TIMEOUT_CEILING_MINUTES: u32 = 30 * 24 * 60;
/// `artifacts.max_item_mb` above this (1 PiB) is surely a typo.
pub const MAX_ARTIFACT_MB: u64 = 1 << 30;

/// The keys holding filesystem paths, which `Config::load` resolves.
const PATH_KEYS: [&str; 4] = [
    "[aether] socket",
    "[orchestrator] instructions_file",
    "[worktrees] root",
    "[integration] slot_lock",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub authority: crate::authority::AuthorityConfig,
    pub aether: AetherConfig,
    pub orchestrator: OrchestratorConfig,
    pub models: ModelsConfig,
    pub review: ReviewConfig,
    pub limits: LimitsConfig,
    pub dispatcher: DispatcherConfig,
    pub chat: ChatConfig,
    pub worktrees: WorktreeConfig,
    pub artifacts: ArtifactsConfig,
    pub storage: StorageConfig,
    pub lavish: LavishConfig,
    pub github: GithubConfig,
    pub autonomy: AutonomyConfig,
    pub learning: LearningConfig,
    pub integration: IntegrationConfig,
    pub notify: crate::push::NotifyConfig,
    pub update: UpdateConfig,
    pub remote: crate::remote::RemoteConfig,
}

/// How archond verifies each repository's integration branch off the items'
/// path (issue #82): it batches merges, then runs the full check once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntegrationConfig {
    /// `false`: no integration branch is watched.
    pub enabled: bool,
    /// How often each integration branch's head is read.
    pub poll_every: Age,
    /// A head this many merges past the last verified one is verified at once.
    pub batch_merges: u32,
    /// A head that stood still this long is verified however few merges it adds.
    pub idle_after: Age,
    /// A full check running longer is ended and counts as failed.
    pub full_timeout: Age,
    /// The lock file full checks take, one at a time; unset:
    /// `/tmp/archon-full-check-<uid>.lock`, shared by every archond of the user.
    pub slot_lock: Option<PathBuf>,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_every: Age::from_secs(60),
            batch_merges: 3,
            idle_after: Age::from_secs(10 * 60),
            full_timeout: Age::from_secs(2 * 3_600),
            slot_lock: None,
        }
    }
}

impl IntegrationConfig {
    fn validate(&self) -> Result<(), ArchonError> {
        if self.poll_every.secs() == 0 || self.full_timeout.secs() == 0 {
            return Err(ArchonError::Config(
                "integration.poll_every and integration.full_timeout must be > 0s".into(),
            ));
        }
        if self.batch_merges == 0 {
            return Err(ArchonError::Config(
                "integration.batch_merges must be at least 1".into(),
            ));
        }
        Ok(())
    }
}

/// Defaults for reviewers an item's model plan leaves unnamed. The tier is a
/// preference: when the model policy permits none at it, the best permitted
/// model is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewConfig {
    pub default_tier: Tier,
    pub default_effort: Effort,
    pub prefer_other_provider: bool,
    /// Queue one retry of an item whose run failed only because the review
    /// or ship record it owes was refused.
    pub auto_repair: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            default_tier: Tier::Balanced,
            default_effort: Effort::Low,
            prefer_other_provider: true,
            auto_repair: true,
        }
    }
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
    /// The highest tier a default picks on its own ([`DEFAULT_MAX_TIER`]
    /// when unset); a plan may name any. A repository may only lower it.
    pub max_default_tier: Option<Tier>,
    /// Each provider's share of tier-only items; empty keeps them on the
    /// orchestrator's provider. A repository's entries override the global ones.
    pub provider_weights: BTreeMap<String, u32>,
    /// The weight of a provider `provider_weights` leaves out
    /// ([`DEFAULT_PROVIDER_WEIGHT`] when unset).
    pub default_provider_weight: Option<u32>,
    /// Quota stage routing leaves to the orchestrator; global only.
    pub reserve_percent: Option<ReservePercent>,
    /// Providers the operator also uses by hand, which keep the
    /// `interactive` reserve; global only.
    pub interactive_providers: Vec<String>,
}

/// Percent of each quota window held back from tier-only stage routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReservePercent {
    /// On the orchestrator's provider.
    pub orchestrator: u8,
    /// On each of `interactive_providers`.
    pub interactive: u8,
}

impl Default for ReservePercent {
    fn default() -> Self {
        Self {
            orchestrator: 15,
            interactive: 10,
        }
    }
}

/// Defaults stop below the frontier tier unless the operator raises them.
pub const DEFAULT_MAX_TIER: Tier = Tier::Strong;

impl ModelsConfig {
    pub fn policy(&self) -> ModelPolicy {
        ModelPolicy::from_lists(&self.allow, &self.deny, self.max_effort)
    }

    /// The weights in force for `repo`: its entries and default over the
    /// global ones. `None` when neither names a provider, so routing is off.
    pub fn provider_weights(&self, repo: &ModelsConfig) -> Option<ProviderWeights> {
        if self.provider_weights.is_empty() && repo.provider_weights.is_empty() {
            return None;
        }
        let mut table = self.provider_weights.clone();
        table.extend(repo.provider_weights.clone());
        let default = repo
            .default_provider_weight
            .or(self.default_provider_weight)
            .unwrap_or(DEFAULT_PROVIDER_WEIGHT);
        debug_assert!(!table.is_empty(), "routing is on only with a table");
        Some(ProviderWeights::new(table, default))
    }

    /// `provider_weights` and `interactive_providers` name providers, not
    /// models, and reserves are percentages.
    pub fn check_weights(&self) -> Result<(), ArchonError> {
        let named = [
            (
                "provider_weights key",
                self.provider_weights.keys().collect::<Vec<_>>(),
            ),
            (
                "interactive_providers entry",
                self.interactive_providers.iter().collect(),
            ),
        ];
        for (what, providers) in named {
            if let Some(bad) = providers
                .iter()
                .find(|p| p.trim().is_empty() || p.contains('/'))
            {
                return Err(ArchonError::Config(format!(
                    "models.{what} {bad:?} must name a provider, such as \"codex\", not a model"
                )));
            }
        }
        let reserve = self.reserve_percent.unwrap_or_default();
        if reserve.orchestrator > 100 || reserve.interactive > 100 {
            return Err(ArchonError::Config(
                "models.reserve_percent values are percentages, 0 to 100".into(),
            ));
        }
        Ok(())
    }

    /// The quota held back from stage routing, as the global file sets it.
    pub fn reserves(&self, orchestrator_provider: &str) -> Reserves {
        assert!(
            !orchestrator_provider.is_empty(),
            "the orchestrator has a provider"
        );
        let percent = self.reserve_percent.unwrap_or_default();
        let reserves = Reserves {
            orchestrator_provider: orchestrator_provider.to_owned(),
            orchestrator: percent.orchestrator,
            interactive_providers: self.interactive_providers.iter().cloned().collect(),
            interactive: percent.interactive,
        };
        debug_assert!(reserves.orchestrator <= 100 && reserves.interactive <= 100);
        reserves
    }

    /// The global cap on a default's tier, lowered by `repo`'s.
    pub fn max_default_tier(&self, repo: &ModelsConfig) -> Tier {
        let global = self.max_default_tier.unwrap_or(DEFAULT_MAX_TIER);
        repo.max_default_tier
            .map_or(global, |local| local.min(global))
    }
}

/// Where a structural limit's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// Not set in `[limits]`, or set to its default.
    Default,
    /// Set in config.toml's `[limits]`.
    Config,
    /// Built in; no configuration changes it.
    Fixed,
}

/// One structural limit as in force, for `archon autonomy` and the
/// `autonomy_policy` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralLimit {
    pub key: String,
    pub value: u64,
    pub default: u64,
    /// The most `[limits]` may raise it to.
    pub ceiling: u64,
    pub source: LimitSource,
}

impl StructuralLimit {
    fn fixed(key: &str, value: u64) -> Self {
        Self {
            key: key.to_owned(),
            value,
            default: value,
            ceiling: value,
            source: LimitSource::Fixed,
        }
    }

    /// `max_agent_messages = 20 (default 20, ceiling 200; default)`.
    pub fn describe(&self) -> String {
        let source = match self.source {
            LimitSource::Default => "default",
            LimitSource::Config => "set in config.toml [limits]",
            LimitSource::Fixed => return format!("{} = {} (fixed)", self.key, self.value),
        };
        format!(
            "{} = {} (default {}, ceiling {}; {source})",
            self.key, self.value, self.default, self.ceiling
        )
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
    /// Count separate API-equivalent estimates, such as Codex usage, toward
    /// budgets. Raw reported cost always counts; learning always counts both.
    pub count_estimated_cost: bool,
    /// Automatic re-spawns after infrastructure failures, per stage run.
    pub infra_retries: u32,
    /// Minutes of Aether silence, with an agent `running` and no engine
    /// alive, after which a running stage is treated as stalled; 0 turns the
    /// stage watchdog off.
    pub stall_minutes: u32,
    /// Minutes of silence that stall a stage when Aether does not report
    /// whether the engine is alive (an Aether older than the watchdog).
    pub stall_fallback_minutes: u32,
    /// Dispatch holds while the data dir or worktree root has less free
    /// space than this, in GiB; 0 disables the hold (ARCHITECTURE §7.3).
    pub min_free_gb: u64,
    /// Dispatch holds while the 1-minute load average per CPU is above
    /// this; unset leaves the host-load hold off.
    pub max_load_per_cpu: Option<f64>,
    /// Dispatch holds while `MemAvailable` is below this, in GiB; unset
    /// leaves the memory hold off.
    pub min_available_memory_gb: Option<f64>,
    pub per_pipeline: BTreeMap<String, u32>,
    /// Most delegations below a stage agent or the orchestrator (§7.4).
    pub max_delegation_depth: u32,
    /// Longest per-stage timeout an item may ask for (`stage_timeouts`).
    pub max_stage_timeout_minutes: u32,
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
            stall_minutes: DEFAULT_STALL_MINUTES,
            stall_fallback_minutes: DEFAULT_STALL_FALLBACK_MINUTES,
            min_free_gb: 10,
            max_load_per_cpu: None,
            min_available_memory_gb: None,
            per_pipeline: BTreeMap::new(),
            max_delegation_depth: DEFAULT_DELEGATION_DEPTH,
            max_stage_timeout_minutes: DEFAULT_STAGE_TIMEOUT_MINUTES,
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

    /// The host-load hold's limits.
    pub fn host_limits(&self) -> crate::host_load::HostLimits {
        crate::host_load::HostLimits {
            max_load_per_cpu: self.max_load_per_cpu,
            min_available_memory_gb: self.min_available_memory_gb,
        }
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
        for (key, value) in [
            ("max_load_per_cpu", self.max_load_per_cpu),
            ("min_available_memory_gb", self.min_available_memory_gb),
        ] {
            if value.is_some_and(|v| !v.is_finite() || v <= 0.0) {
                return Err(ArchonError::Config(format!(
                    "limits.{key} must be a positive number, not {}; remove the key to turn the check off",
                    value.unwrap_or_default()
                )));
            }
        }
        for (key, value) in [
            ("stall_minutes", self.stall_minutes),
            ("stall_fallback_minutes", self.stall_fallback_minutes),
        ] {
            if value > STAGE_TIMEOUT_CEILING_MINUTES {
                return Err(ArchonError::Config(format!(
                    "limits.{key} must be at most {STAGE_TIMEOUT_CEILING_MINUTES} (0 = no stage watchdog), not {value}"
                )));
            }
        }
        for (key, value, _, most) in self.configurable() {
            if !(1..=most).contains(&value) {
                return Err(ArchonError::Config(format!(
                    "limits.{key} must be between 1 and {most}, not {value}"
                )));
            }
        }
        Ok(())
    }

    /// The structural limits `[limits]` may set: key, value, default, ceiling.
    fn configurable(&self) -> [(&'static str, u32, u32, u32); 5] {
        let default = Self::default();
        let ceiling = InitiativeLimits::CEILING;
        [
            (
                "max_delegation_depth",
                self.max_delegation_depth,
                default.max_delegation_depth,
                DELEGATION_DEPTH_CEILING,
            ),
            (
                "max_stage_timeout_minutes",
                self.max_stage_timeout_minutes,
                default.max_stage_timeout_minutes,
                STAGE_TIMEOUT_CEILING_MINUTES,
            ),
            (
                "max_agent_messages",
                self.max_agent_messages,
                default.max_agent_messages,
                AGENT_MESSAGES_CEILING,
            ),
            (
                "initiative_members",
                self.initiative_members,
                default.initiative_members,
                ceiling.members as u32,
            ),
            (
                "initiative_contract_bytes",
                self.initiative_contract_bytes,
                default.initiative_contract_bytes,
                ceiling.contract_bytes as u32,
            ),
        ]
    }

    /// Every structural limit in force, configurable ones first, then the
    /// fixed directive bounds.
    pub fn structural(&self) -> Vec<StructuralLimit> {
        let mut limits: Vec<StructuralLimit> = self
            .configurable()
            .into_iter()
            .map(|(key, value, default, ceiling)| {
                assert!(
                    default <= ceiling,
                    "{key}: the default sits within the ceiling"
                );
                StructuralLimit {
                    key: key.to_owned(),
                    value: u64::from(value),
                    default: u64::from(default),
                    ceiling: u64::from(ceiling),
                    source: if value == default {
                        LimitSource::Default
                    } else {
                        LimitSource::Config
                    },
                }
            })
            .collect();
        for (key, value) in [
            ("max_directives", MAX_DIRECTIVES),
            ("max_directive_chars", MAX_DIRECTIVE_CHARS),
        ] {
            limits.push(StructuralLimit::fixed(key, value as u64));
        }
        debug_assert!(limits.iter().all(|l| l.value <= l.ceiling));
        limits
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
    /// An inbox item a running stage's agent leaves unread this long is a
    /// stage health warning in `get_item` and `status`.
    pub inbox_unread_after: Age,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            tick_secs: 15,
            inbox_unread_after: Age::from_secs(15 * 60),
        }
    }
}

/// The release check a launch of `archon` makes (docs/configuration.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// `false`: a launch never asks GitHub for releases and never offers an update.
    pub check_on_launch: bool,
    /// A release answer younger than this is reused instead of asked again.
    pub cache_for: Age,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_on_launch: true,
            cache_for: Age::from_secs(60 * 60),
        }
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
    /// Stages of a repository build into one cargo target directory under
    /// `<data>/cache`, serialising on cargo's build lock. Off by default:
    /// each item gets its own (`item_cache`). A repository's
    /// `[cache] shared = false` opts it out.
    pub shared_cache: bool,
    /// Each item builds into its own cargo target directory under
    /// `<data>/cache/<repo>/items`, deleted when the item ends; `false`
    /// leaves agents to build into their worktrees. Ignored where
    /// `shared_cache` applies.
    pub item_cache: bool,
    /// archond's cargo config turns off incremental builds and trims
    /// debuginfo to line tables; a repository's `[cache] lean = false`
    /// opts out.
    pub lean_builds: bool,
    /// A repository's shared cache nobody used for this long, with no live
    /// run on the repository, is deleted.
    pub shared_cache_after: Age,
    /// While dispatch is held for disk space, finished runs' worktrees and
    /// idle shared caches go after this instead, if it is shorter.
    pub pressure_after: Age,
    /// The `docker` CLI that tears down the Compose stacks agents leave in
    /// worktrees; without it, or its daemon, stacks are left alone.
    pub docker: PathBuf,
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
            shared_cache: false,
            item_cache: true,
            lean_builds: true,
            shared_cache_after: Age::from_secs(14 * 86_400),
            pressure_after: Age::from_secs(86_400),
            docker: "docker".into(),
        }
    }
}

/// Filesystem retention; SQLite history and evidence windows are independent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Settled item reports and rotated logs; 0 disables age expiry.
    pub retention_days: u32,
    /// Rotate daemon output after this many MiB; 0 disables size rotation.
    pub log_max_mb: u64,
    /// Retained log archives; 0 disables the count limit.
    pub log_max_files: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            retention_days: 14,
            log_max_mb: 10,
            log_max_files: 3,
        }
    }
}

impl StorageConfig {
    fn validate(&self) -> Result<(), ArchonError> {
        if self.retention_days > 36_500 || self.log_max_mb > (1 << 20) || self.log_max_files > 1000
        {
            return Err(ArchonError::Config(
                "storage: retention_days must be 0..36500, log_max_mb 0..1048576, and log_max_files 0..1000".into(),
            ));
        }
        Ok(())
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
            keep_after: Age::from_secs(14 * 86_400),
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
    /// `false` opts the repository out of per-item target directories.
    pub item: Option<bool>,
    /// `false` opts the repository out of the lean build profile.
    pub lean: Option<bool>,
}

/// Where a repository's stages build with cargo (ARCHITECTURE §13.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildCache {
    /// One target directory per repository.
    Shared,
    /// One target directory per item, deleted when the item ends.
    Item,
    /// The agents' own worktrees.
    Worktree,
}

impl RepoConfig {
    /// Where this repository's stages build: the global switches, unless
    /// the repository opts out. The shared cache wins where both apply.
    pub fn build_cache(&self, worktrees: &WorktreeConfig) -> BuildCache {
        if worktrees.shared_cache && self.cache.shared.unwrap_or(true) {
            BuildCache::Shared
        } else if worktrees.item_cache && self.cache.item.unwrap_or(true) {
            BuildCache::Item
        } else {
            BuildCache::Worktree
        }
    }

    /// Whether archond's cargo config trims builds for this repository.
    pub fn lean_builds(&self, worktrees: &WorktreeConfig) -> bool {
        worktrees.lean_builds && self.cache.lean.unwrap_or(true)
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
    fn paths(&self) -> [&Option<PathBuf>; 4] {
        [
            &self.aether.socket,
            &self.orchestrator.instructions_file,
            &self.worktrees.root,
            &self.integration.slot_lock,
        ]
    }

    fn paths_mut(&mut self) -> [&mut Option<PathBuf>; 4] {
        [
            &mut self.aether.socket,
            &mut self.orchestrator.instructions_file,
            &mut self.worktrees.root,
            &mut self.integration.slot_lock,
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
        if self.dispatcher.inbox_unread_after.secs() == 0 {
            return Err(ArchonError::Config(
                "dispatcher.inbox_unread_after must be > 0s".into(),
            ));
        }
        self.limits.validate()?;
        self.autonomy.validate()?;
        self.worktrees.validate()?;
        self.artifacts.validate()?;
        self.storage.validate()?;
        self.integration.validate()?;
        self.notify.validate()?;
        self.remote.validate()?;
        self.models.check_weights()?;
        let orchestrator = crate::model::ModelChoice {
            model: self.orchestrator.model.clone(),
            effort: self.orchestrator.effort,
            tier: None,
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
        let config: Self = read_toml(&path)?.unwrap_or_default();
        config.models.check_weights()?;
        if config.models.reserve_percent.is_some()
            || !config.models.interactive_providers.is_empty()
        {
            return Err(ArchonError::Config(format!(
                "{}: models.reserve_percent and models.interactive_providers are set in \
                 Archon's global config only",
                path.display()
            )));
        }
        Ok(config)
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
    fn review_defaults_and_overrides_are_validated() {
        let default: Config = toml::from_str("").unwrap();
        assert_eq!(default.review.default_tier, Tier::Balanced);
        assert_eq!(default.review.default_effort, Effort::Low);
        assert!(default.review.prefer_other_provider);
        assert!(default.review.auto_repair);
        let configured: Config = toml::from_str(
            "[review]\ndefault_tier = 'strong'\ndefault_effort = 'medium'\nprefer_other_provider = false\nauto_repair = false",
        ).unwrap();
        assert_eq!(configured.review.default_tier, Tier::Strong);
        assert_eq!(configured.review.default_effort, Effort::Medium);
        assert!(!configured.review.prefer_other_provider);
        assert!(!configured.review.auto_repair);
        for invalid in [
            "default_tier = 'typo'",
            "default_effort = 'typo'",
            "unknown = true",
        ] {
            assert!(toml::from_str::<Config>(&format!("[review]\n{invalid}")).is_err());
        }
    }

    #[test]
    fn the_stage_watchdog_settings_default_and_are_bounded() {
        let default = LimitsConfig::default();
        assert_eq!(
            (default.stall_minutes, default.stall_fallback_minutes),
            (20, 45)
        );
        let off: Config = toml::from_str("[limits]\nstall_minutes = 0").unwrap();
        assert!(off.limits.validate().is_ok(), "0 turns the watchdog off");
        let typo = LimitsConfig {
            stall_fallback_minutes: STAGE_TIMEOUT_CEILING_MINUTES + 1,
            ..LimitsConfig::default()
        };
        let error = typo.validate().unwrap_err().to_string();
        assert!(error.contains("limits.stall_fallback_minutes"), "{error}");
    }

    #[test]
    fn missing_file_is_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.limits.caps().max_parallel_agents, 8);
        assert!(config.orchestrator.wake_on_success);
    }

    #[test]
    fn an_unread_inbox_warns_after_fifteen_minutes_unless_configured() {
        let default = Config::default().dispatcher.inbox_unread_after;
        assert_eq!(default, Age::from_secs(15 * 60));
        let config: Config = toml::from_str("[dispatcher]\ninbox_unread_after = '40m'").unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.dispatcher.inbox_unread_after,
            Age::from_secs(40 * 60)
        );
        let zero: Config = toml::from_str("[dispatcher]\ninbox_unread_after = '0s'").unwrap();
        let refused = zero.validate().unwrap_err();
        assert!(
            refused.to_string().contains("inbox_unread_after"),
            "{refused}"
        );
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
    fn integration_batches_are_read_and_checked() {
        let text = "[integration]\nbatch_merges = 5\nidle_after = \"30m\"\n";
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.integration.batch_merges, 5);
        assert_eq!(config.integration.idle_after.secs(), 1_800);
        assert_eq!(config.integration.poll_every.secs(), 60, "the default");
        for bad in [
            "batch_merges = 0",
            "poll_every = \"0s\"",
            "full_timeout = \"0s\"",
        ] {
            let config: Config = toml::from_str(&format!("[integration]\n{bad}\n")).unwrap();
            assert!(config.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn provider_weights_overlay_per_repository_and_name_providers_only() {
        let global: Config =
            toml::from_str("[models]\nprovider_weights = { \"claude-code\" = 7, \"codex\" = 3 }")
                .unwrap();
        global.validate().unwrap();
        let none = ModelsConfig::default();
        assert_eq!(
            none.provider_weights(&none),
            None,
            "unset keeps routing off"
        );
        let weights = global.models.provider_weights(&none).unwrap();
        assert_eq!(
            (weights.weight("claude-code"), weights.weight("codex")),
            (7, 3)
        );
        assert_eq!(weights.weight("gemini"), DEFAULT_PROVIDER_WEIGHT);
        let repo: RepoConfig = toml::from_str(
            "[models]\nprovider_weights = { codex = 5 }\ndefault_provider_weight = 0",
        )
        .unwrap();
        let local = global.models.provider_weights(&repo.models).unwrap();
        assert_eq!((local.weight("claude-code"), local.weight("codex")), (7, 5));
        assert_eq!(local.weight("gemini"), 0);
        let model: Config =
            toml::from_str("[models]\nprovider_weights = { \"codex/sol\" = 1 }").unwrap();
        let refused = model.validate().unwrap_err().to_string();
        assert!(refused.contains("must name a provider"), "{refused}");
    }

    #[test]
    fn quota_reserves_default_to_fifteen_and_ten_and_stay_global() {
        let defaults = ModelsConfig::default().reserves("claude-code");
        assert_eq!((defaults.orchestrator, defaults.interactive), (15, 10));
        assert_eq!(defaults.percent("codex"), 0);
        let set: Config = toml::from_str(
            "[models]\nreserve_percent = { orchestrator = 20 }\ninteractive_providers = [\"codex\"]",
        )
        .unwrap();
        set.validate().unwrap();
        let reserves = set.models.reserves("claude-code");
        assert_eq!(
            (reserves.percent("claude-code"), reserves.percent("codex")),
            (20, 10)
        );
        let over: Config =
            toml::from_str("[models]\nreserve_percent = { interactive = 101 }").unwrap();
        assert!(over.validate().is_err(), "a reserve is a percentage");
        let dir = tempfile::tempdir().unwrap();
        let file = crate::paths::repo_archon_dir(dir.path()).join(CONFIG_FILE_NAME);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[models]\nreserve_percent = { orchestrator = 0 }\n").unwrap();
        let refused = RepoConfig::load(dir.path()).unwrap_err().to_string();
        assert!(refused.contains("global config only"), "{refused}");
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
    fn storage_defaults_disable_switches_and_validation() {
        let config = Config::default();
        assert_eq!(config.storage.retention_days, 14);
        assert_eq!(config.storage.log_max_mb, 10);
        assert_eq!(config.storage.log_max_files, 3);
        assert_eq!(config.artifacts.keep_after.secs(), 14 * 86_400);
        let off: Config =
            toml::from_str("[storage]\nretention_days = 0\nlog_max_mb = 0\nlog_max_files = 0")
                .unwrap();
        off.validate().unwrap();
        for bad in [
            "retention_days = 36501",
            "log_max_mb = 1048577",
            "log_max_files = 1001",
        ] {
            let config: Config = toml::from_str(&format!("[storage]\n{bad}")).unwrap();
            assert!(config.validate().is_err(), "{bad}");
        }
        for bad in [
            "retention_days = -1",
            "retention_days = 1.5",
            "retention_days = \"14d\"",
            "retenton_days = 14",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[storage]\n{bad}")).is_err(),
                "{bad}"
            );
        }
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
    fn the_host_hold_is_off_by_default_and_its_limits_must_be_positive() {
        let config = Config::default();
        assert_eq!(config.limits.max_load_per_cpu, None);
        assert_eq!(config.limits.min_available_memory_gb, None);
        let on: Config =
            toml::from_str("[limits]\nmax_load_per_cpu = 1.5\nmin_available_memory_gb = 2")
                .unwrap();
        on.validate().unwrap();
        assert_eq!(on.limits.host_limits().max_load_per_cpu, Some(1.5));
        for bad in [
            "max_load_per_cpu = 0",
            "max_load_per_cpu = -1.0",
            "min_available_memory_gb = 0.0",
            "min_available_memory_gb = nan",
            "min_available_memory_gb = inf",
        ] {
            let config: Config = toml::from_str(&format!("[limits]\n{bad}")).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains("must be a positive number"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn structural_ceilings_default_to_their_old_values_and_are_bounded() {
        let limits = Config::default().limits;
        assert_eq!(limits.max_delegation_depth, 2);
        assert_eq!(limits.max_agent_messages, 20);
        assert_eq!(limits.max_stage_timeout_minutes, 1440);
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
            "max_stage_timeout_minutes = 0",
            "max_stage_timeout_minutes = 43201",
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
    fn structural_limits_name_value_default_ceiling_and_source() {
        let raised: Config =
            toml::from_str("[limits]\ninitiative_contract_bytes = 8192\nmax_agent_messages = 20")
                .unwrap();
        let limits = raised.limits.structural();
        let keys: Vec<&str> = limits.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "max_delegation_depth",
                "max_stage_timeout_minutes",
                "max_agent_messages",
                "initiative_members",
                "initiative_contract_bytes",
                "max_directives",
                "max_directive_chars",
            ]
        );
        let describe = |key: &str| limits.iter().find(|l| l.key == key).unwrap().describe();
        assert_eq!(
            describe("initiative_contract_bytes"),
            "initiative_contract_bytes = 8192 (default 2048, ceiling 16384; set in config.toml [limits])"
        );
        assert_eq!(
            describe("max_agent_messages"),
            "max_agent_messages = 20 (default 20, ceiling 200; default)"
        );
        assert_eq!(describe("max_directives"), "max_directives = 20 (fixed)");
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
