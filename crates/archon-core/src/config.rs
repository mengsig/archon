//! `~/.config/archon/config.toml` and `<repo>/.archon/config.toml`
//! (ARCHITECTURE §9). Every field is optional; unknown keys are refused so a
//! typo can never silently disable a limit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::age::Age;
use crate::autonomy::AutonomyConfig;
use crate::error::ArchonError;
use crate::model::{Effort, ModelRef};
use crate::policy::ModelPolicy;
use crate::queue::Caps;
use crate::repo::ShipMode;

pub const CONFIG_FILE_NAME: &str = "config.toml";

/// `limits.min_free_gb` above this (1 PiB) is surely a typo, and its bytes
/// must fit in `u64`.
pub const MAX_MIN_FREE_GB: u64 = 1 << 20;

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
    pub explore_rate: f64,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            distill_every: 20,
            explore_rate: 0.1,
        }
    }
}

impl LearningConfig {
    pub fn validate(&self) -> Result<(), ArchonError> {
        if self.distill_every == 0 {
            return Err(ArchonError::Config(
                "learning.distill_every must be positive".into(),
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
}

impl Default for AetherConfig {
    fn default() -> Self {
        Self {
            mode: AetherMode::default(),
            socket: None,
            address: None,
            token_env: "AETHER_TOKEN".into(),
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
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            model: ModelRef::new("claude-code", "claude-opus-5-5"),
            effort: Effort::High,
            name: "Archon".into(),
            instructions_file: None,
            trigger_coalesce_ms: 2000,
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
}

impl Default for LimitsConfig {
    fn default() -> Self {
        let caps = Caps::default();
        Self {
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
        }
    }
}

impl LimitsConfig {
    /// `min_free_gb` in bytes; 0 when the disk hold is off.
    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_gb.saturating_mul(1 << 30)
    }

    pub fn caps(&self) -> Caps {
        Caps {
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
        }
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
        let budgets = [
            self.limits.budget_usd_per_item,
            self.limits.budget_usd_daily,
            self.limits.budget_usd_total,
        ];
        if budgets.iter().any(|b| !b.is_finite() || *b < 0.0) {
            return Err(ArchonError::Config(
                "limits budgets must be finite and >= 0 (0 = unlimited)".into(),
            ));
        }
        if self.limits.min_free_gb > MAX_MIN_FREE_GB {
            return Err(ArchonError::Config(format!(
                "limits.min_free_gb must be at most {MAX_MIN_FREE_GB} (0 = no disk hold)"
            )));
        }
        self.autonomy.validate()?;
        self.worktrees.validate()?;
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
        assert_eq!(config.limits.caps().max_parallel_agents, 3);
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
            strict-pr-review = 3
            [lavish]
            enabled = "off"
            [github]
            binary = "/usr/bin/gh"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.orchestrator.effort, Effort::XHigh);
        assert_eq!(config.limits.per_pipeline["strict-pr-review"], 3);
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
