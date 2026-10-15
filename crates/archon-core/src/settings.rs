//! Operator settings archond persists: one global overlay over
//! `config.toml` and one overlay per repository (ARCHITECTURE §18.2).
//!
//! Precedence, weakest first: `config.toml`, the global overlay, the
//! registry and the repository overlay, then the repository's own
//! `.archon/config.toml`, which may only lower.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::autonomy::{
    Autonomy, AutonomyConfig, AutonomyPatch, AutonomyPolicy, MergeMode, ProposalMode, ReportMode,
    ReviewLevel, ToolsMode,
};
use crate::error::ArchonError;
use crate::queue::Caps;

/// What the operator set at one scope; unset fields come from the layer
/// beneath. An empty value is "nothing set".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OperatorSettings {
    #[serde(skip_serializing_if = "AutonomyPatch::is_empty")]
    pub autonomy_default: AutonomyPatch,
    #[serde(skip_serializing_if = "AutonomyPatch::is_empty")]
    pub autonomy_max: AutonomyPatch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budgets_enabled: Option<bool>,
    /// Dollars; 0 is unlimited, as in `[limits]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_usd_per_item: Option<f64>,
    /// Global only: spend is not yet attributed per repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_usd_daily: Option<f64>,
    /// Global only, like `budget_usd_daily`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_usd_total: Option<f64>,
    /// Global only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel_agents: Option<u32>,
    /// Global only; a repository lowers it in its `.archon/config.toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_repo: Option<u32>,
    /// Global only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_active_runs: Option<u32>,
    /// Global only; replaces `[limits] per_pipeline` whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_per_pipeline: Option<BTreeMap<String, u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_pipeline: Option<String>,
    /// Reserved: enabling is refused until full-auto exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_auto: Option<bool>,
}

/// Where an overlay applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Repo,
}

impl OperatorSettings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Refuse what this scope cannot enforce, so no setting is a silent no-op.
    pub fn validate(&self, scope: SettingsScope) -> Result<(), ArchonError> {
        if self.full_auto == Some(true) {
            return Err(ArchonError::Invalid(
                "full_auto is not supported yet; leave it unset or false".into(),
            ));
        }
        let budgets = [
            ("budget_usd_per_item", self.budget_usd_per_item),
            ("budget_usd_daily", self.budget_usd_daily),
            ("budget_usd_total", self.budget_usd_total),
        ];
        for (name, usd) in budgets {
            if let Some(usd) = usd
                && (!usd.is_finite() || usd < 0.0)
            {
                return Err(ArchonError::Invalid(format!(
                    "{name} must be a finite amount of at least 0 (0 = none), not {usd}"
                )));
            }
        }
        if scope == SettingsScope::Repo {
            self.refuse_global_only()?;
        }
        if let Some(name) = &self.default_pipeline
            && name.trim().is_empty()
        {
            return Err(ArchonError::Invalid("default_pipeline is empty".into()));
        }
        if let Some(per) = &self.max_per_pipeline
            && let Some(name) = per.keys().find(|n| n.trim().is_empty())
        {
            return Err(ArchonError::Invalid(format!(
                "max_per_pipeline has an empty name {name:?}"
            )));
        }
        self.check_default_within_max()
    }

    fn refuse_global_only(&self) -> Result<(), ArchonError> {
        let global_only = [
            ("budget_usd_daily", self.budget_usd_daily.is_some()),
            ("budget_usd_total", self.budget_usd_total.is_some()),
            ("max_parallel_agents", self.max_parallel_agents.is_some()),
            ("max_per_repo", self.max_per_repo.is_some()),
            ("max_active_runs", self.max_active_runs.is_some()),
            ("max_per_pipeline", self.max_per_pipeline.is_some()),
        ];
        let set: Vec<&str> = global_only
            .iter()
            .filter(|(_, set)| *set)
            .map(|(name, _)| *name)
            .collect();
        if set.is_empty() {
            return Ok(());
        }
        Err(ArchonError::Invalid(format!(
            "{} can only be set globally, not per repository",
            set.join(", ")
        )))
    }

    /// A default above the ceiling set beside it is a mistake, not a clamp.
    fn check_default_within_max(&self) -> Result<(), ArchonError> {
        let ceiling = self.autonomy_max.over(most_autonomous());
        let excess = self.autonomy_default.excess(ceiling);
        if excess.is_empty() {
            return Ok(());
        }
        let listed: Vec<String> = excess.iter().map(ToString::to_string).collect();
        Err(ArchonError::Invalid(format!(
            "autonomy_default exceeds autonomy_max: {}",
            listed.join("; ")
        )))
    }

    /// `base` with the caps this overlay sets; per-repository overlays only
    /// set the fields `validate` lets them.
    pub fn caps_over(&self, mut base: Caps) -> Caps {
        let fields = [
            (&mut base.budget_usd_per_item, self.budget_usd_per_item),
            (&mut base.budget_usd_daily, self.budget_usd_daily),
            (&mut base.budget_usd_total, self.budget_usd_total),
        ];
        for (field, set) in fields {
            if let Some(value) = set {
                *field = value;
            }
        }
        let counts = [
            (&mut base.max_parallel_agents, self.max_parallel_agents),
            (&mut base.max_per_repo, self.max_per_repo),
            (&mut base.max_active_runs, self.max_active_runs),
        ];
        for (field, set) in counts {
            if let Some(value) = set {
                *field = value;
            }
        }
        if let Some(enabled) = self.budgets_enabled {
            base.budgets_enabled = enabled;
        }
        if let Some(per) = &self.max_per_pipeline {
            base.max_per_pipeline = per.clone();
        }
        base
    }

    /// The overlay that makes `caps_over` yield `caps` whatever lies beneath:
    /// how `SetCaps` persists.
    pub fn set_caps(&mut self, caps: &Caps) {
        self.budgets_enabled = Some(caps.budgets_enabled);
        self.budget_usd_per_item = Some(caps.budget_usd_per_item);
        self.budget_usd_daily = Some(caps.budget_usd_daily);
        self.budget_usd_total = Some(caps.budget_usd_total);
        self.max_parallel_agents = Some(caps.max_parallel_agents);
        self.max_per_repo = Some(caps.max_per_repo);
        self.max_active_runs = Some(caps.max_active_runs);
        self.max_per_pipeline = Some(caps.max_per_pipeline.clone());
        debug_assert_eq!(&self.caps_over(Caps::default()), caps);
    }
}

fn most_autonomous() -> Autonomy {
    Autonomy {
        review: ReviewLevel::None,
        proposal: ProposalMode::Off,
        report: ReportMode::Off,
        merge: MergeMode::Auto,
        tools: ToolsMode::Yolo,
    }
}

/// Which layer decided an effective value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// Archon's built-in default.
    Builtin,
    /// `config.toml`.
    Config,
    /// The global operator overlay.
    Global,
    /// The repository's registry entry (`archon repo set`).
    Registry,
    /// The repository's operator overlay.
    Repo,
    /// The repository's `.archon/config.toml`, which only lowers.
    RepoFile,
}

/// A repository's layers above the global ones.
#[derive(Debug, Clone, Copy)]
pub struct RepoLayers<'a> {
    pub registry_max: &'a AutonomyPatch,
    pub registry_pipeline: Option<&'a str>,
    pub overlay: &'a OperatorSettings,
    pub file: &'a crate::config::RepoConfig,
}

/// Every layer for one scope: the repository's, or none for the global view
/// and for items without one.
#[derive(Debug, Clone, Copy)]
pub struct Layers<'a> {
    pub config: &'a crate::config::Config,
    pub global: &'a OperatorSettings,
    pub repo: Option<RepoLayers<'a>>,
}

/// The values in force for one scope, and where each came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveSettings {
    pub autonomy: AutonomyPolicy,
    pub caps: Caps,
    pub default_pipeline: Option<String>,
    /// Field (`caps.budget_usd_daily`, `autonomy.max.merge`, …) to the
    /// layer that decided it.
    pub sources: BTreeMap<String, SettingSource>,
}

impl Layers<'_> {
    pub fn global_caps(&self) -> Caps {
        self.global.caps_over(self.config.limits.caps())
    }

    /// Global caps, then the repository overlay's budget switch and per-item
    /// budget. A repository file's `max_per_repo` lowers it in the dispatcher.
    pub fn caps(&self) -> Caps {
        let global = self.global_caps();
        match &self.repo {
            Some(repo) => repo.overlay.caps_over(global),
            None => global,
        }
    }

    pub fn autonomy(&self) -> AutonomyPolicy {
        self.autonomy_traced().0
    }

    /// Repository overlay, registry, repository file, then the global overlay.
    pub fn default_pipeline(&self) -> Option<(String, SettingSource)> {
        let repo = self.repo.as_ref();
        let candidates = [
            (
                repo.and_then(|r| r.overlay.default_pipeline.clone()),
                SettingSource::Repo,
            ),
            (
                repo.and_then(|r| r.registry_pipeline.map(str::to_owned)),
                SettingSource::Registry,
            ),
            (
                repo.and_then(|r| r.file.repo.default_pipeline.clone()),
                SettingSource::RepoFile,
            ),
            (self.global.default_pipeline.clone(), SettingSource::Global),
        ];
        candidates
            .into_iter()
            .find_map(|(name, source)| name.map(|n| (n, source)))
    }

    pub fn effective(&self) -> EffectiveSettings {
        let (autonomy, mut sources) = self.autonomy_traced();
        let caps = self.caps();
        self.trace_caps(&mut sources);
        let default_pipeline = self.default_pipeline().map(|(name, source)| {
            sources.insert("default_pipeline".into(), source);
            name
        });
        EffectiveSettings {
            autonomy,
            caps,
            default_pipeline,
            sources,
        }
    }

    fn trace_caps(&self, sources: &mut BTreeMap<String, SettingSource>) {
        let repo = self.repo.as_ref().map(|r| r.overlay);
        for (name, set) in cap_fields(self.global) {
            let repo_set =
                repo.is_some_and(|r| cap_fields(r).iter().any(|(n, s)| *n == name && *s));
            let source = if repo_set {
                SettingSource::Repo
            } else if set {
                SettingSource::Global
            } else {
                SettingSource::Config
            };
            sources.insert(format!("caps.{name}"), source);
        }
    }

    /// The policy, with the layer that last changed or set each knob.
    fn autonomy_traced(&self) -> (AutonomyPolicy, BTreeMap<String, SettingSource>) {
        let mut trace = Trace::default();
        let config = &self.config.autonomy;
        let mut ceiling = trace.replace(
            "max",
            Autonomy::default_ceiling(),
            &[
                (&config.max, SettingSource::Config),
                (&self.global.autonomy_max, SettingSource::Global),
            ],
        );
        let mut default = trace.replace(
            "default",
            Autonomy::default(),
            &[
                (&config.default, SettingSource::Config),
                (&self.global.autonomy_default, SettingSource::Global),
            ],
        );
        if let Some(repo) = &self.repo {
            ceiling = trace.replace(
                "max",
                ceiling,
                &[
                    (repo.registry_max, SettingSource::Registry),
                    (&repo.overlay.autonomy_max, SettingSource::Repo),
                ],
            );
            default = trace.replace(
                "default",
                default,
                &[(&repo.overlay.autonomy_default, SettingSource::Repo)],
            );
            ceiling = trace.lower("max", ceiling, &repo.file.autonomy);
            default = trace.lower("default", default, &repo.file.autonomy);
        }
        let narrowed = default.narrowed(ceiling);
        trace.mark_clamped(default, narrowed);
        let policy = AutonomyPolicy {
            default: narrowed,
            ceiling,
        };
        assert!(policy.default.excess(policy.ceiling).is_empty());
        (policy, trace.sources)
    }
}

fn cap_fields(s: &OperatorSettings) -> [(&'static str, bool); 8] {
    [
        ("budgets_enabled", s.budgets_enabled.is_some()),
        ("budget_usd_per_item", s.budget_usd_per_item.is_some()),
        ("budget_usd_daily", s.budget_usd_daily.is_some()),
        ("budget_usd_total", s.budget_usd_total.is_some()),
        ("max_parallel_agents", s.max_parallel_agents.is_some()),
        ("max_per_repo", s.max_per_repo.is_some()),
        ("max_active_runs", s.max_active_runs.is_some()),
        ("max_per_pipeline", s.max_per_pipeline.is_some()),
    ]
}

/// The set knobs of a patch, in knob order.
fn set_knobs(patch: &AutonomyPatch) -> [(&'static str, bool); 5] {
    [
        ("review", patch.review.is_some()),
        ("proposal", patch.proposal.is_some()),
        ("report", patch.report.is_some()),
        ("merge", patch.merge.is_some()),
        ("tools", patch.tools.is_some()),
    ]
}

fn knob_values(a: Autonomy) -> [(&'static str, String); 5] {
    [
        ("review", a.review.to_string()),
        ("proposal", a.proposal.to_string()),
        ("report", a.report.to_string()),
        ("merge", a.merge.to_string()),
        ("tools", a.tools.to_string()),
    ]
}

#[derive(Default)]
struct Trace {
    sources: BTreeMap<String, SettingSource>,
}

impl Trace {
    /// Apply replacing layers in order, crediting each knob to the last
    /// layer that set it.
    fn replace(
        &mut self,
        which: &str,
        mut base: Autonomy,
        layers: &[(&AutonomyPatch, SettingSource)],
    ) -> Autonomy {
        for (knob, _) in set_knobs(&AutonomyPatch::default()) {
            self.sources
                .entry(format!("autonomy.{which}.{knob}"))
                .or_insert(SettingSource::Builtin);
        }
        for (patch, source) in layers {
            for (knob, set) in set_knobs(patch) {
                if set {
                    self.sources
                        .insert(format!("autonomy.{which}.{knob}"), *source);
                }
            }
            base = patch.over(base);
        }
        base
    }

    /// The repository file only lowers; credit it where it did.
    fn lower(&mut self, which: &str, base: Autonomy, file: &AutonomyConfig) -> Autonomy {
        let patch = if which == "max" {
            &file.max
        } else {
            &file.default
        };
        let lowered = patch.lower(base);
        self.credit_changes(which, base, lowered, SettingSource::RepoFile);
        lowered
    }

    /// A default above the ceiling is held at it; the ceiling's source decides.
    fn mark_clamped(&mut self, default: Autonomy, narrowed: Autonomy) {
        for ((knob, before), (_, after)) in
            knob_values(default).into_iter().zip(knob_values(narrowed))
        {
            if before != after {
                let source = self.sources[&format!("autonomy.max.{knob}")];
                self.sources
                    .insert(format!("autonomy.default.{knob}"), source);
            }
        }
    }

    fn credit_changes(
        &mut self,
        which: &str,
        before: Autonomy,
        after: Autonomy,
        source: SettingSource,
    ) {
        for ((knob, old), (_, new)) in knob_values(before).into_iter().zip(knob_values(after)) {
            if old != new {
                self.sources
                    .insert(format!("autonomy.{which}.{knob}"), source);
            }
        }
    }
}

/// `Reply::Settings`: what one scope stores, what lies beneath, and the
/// values in force with their sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    /// The repository asked about; `None` is the global scope.
    pub repo: Option<String>,
    /// The overlay stored at this scope.
    pub scoped: OperatorSettings,
    /// The global overlay, for a repository's view; the same as `scoped`
    /// for the global one.
    pub global: OperatorSettings,
    pub effective: EffectiveSettings,
}

impl SettingsView {
    /// Plain text for the CLI and MCP: what is stored, then what is in
    /// force, each value with its source.
    pub fn describe(&self) -> String {
        let scope = match &self.repo {
            Some(name) => format!("repository {name}"),
            None => "global".to_owned(),
        };
        let stored = match self.scoped.is_empty() {
            true => "nothing set".to_owned(),
            false => serde_json::to_string(&self.scoped).expect("settings serialize"),
        };
        let mut lines = vec![format!("{scope} settings: {stored}")];
        if self.repo.is_some() && !self.global.is_empty() {
            let global = serde_json::to_string(&self.global).expect("settings serialize");
            lines.push(format!("global settings: {global}"));
        }
        let effective = &self.effective;
        let source = |key: &str| {
            effective
                .sources
                .get(key)
                .map_or("builtin".to_owned(), |s| format!("{s:?}").to_lowercase())
        };
        let caps = &effective.caps;
        lines.push(format!(
            "in force: budgets {} ({}), per item ${} ({}), daily ${} ({}), total ${} ({}), max per repo {} ({})",
            if caps.budgets_enabled { "on" } else { "off" },
            source("caps.budgets_enabled"),
            caps.budget_usd_per_item,
            source("caps.budget_usd_per_item"),
            caps.budget_usd_daily,
            source("caps.budget_usd_daily"),
            caps.budget_usd_total,
            source("caps.budget_usd_total"),
            caps.max_per_repo,
            source("caps.max_per_repo"),
        ));
        lines.push(format!(
            "autonomy default: {}\nautonomy ceiling: {}",
            traced_knobs(effective, "default", effective.autonomy.default),
            traced_knobs(effective, "max", effective.autonomy.ceiling),
        ));
        if let Some(pipeline) = &effective.default_pipeline {
            lines.push(format!(
                "default pipeline: {pipeline} ({})",
                source("default_pipeline")
            ));
        }
        lines.join("\n")
    }
}

fn traced_knobs(effective: &EffectiveSettings, which: &str, values: Autonomy) -> String {
    let pairs: Vec<String> = knob_values(values)
        .into_iter()
        .map(|(knob, value)| {
            let source = effective
                .sources
                .get(&format!("autonomy.{which}.{knob}"))
                .copied()
                .unwrap_or(SettingSource::Builtin);
            format!("{knob}={value} ({})", format!("{source:?}").to_lowercase())
        })
        .collect();
    pairs.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RepoConfig};

    fn patch(text: &str) -> AutonomyPatch {
        AutonomyPatch::parse_list(text).unwrap()
    }

    fn repo<'a>(
        registry_max: &'a AutonomyPatch,
        overlay: &'a OperatorSettings,
        file: &'a RepoConfig,
    ) -> RepoLayers<'a> {
        RepoLayers {
            registry_max,
            registry_pipeline: None,
            overlay,
            file,
        }
    }

    #[test]
    fn empty_overlays_resolve_exactly_like_the_configuration() {
        let mut config = Config::default();
        config.autonomy.max = patch("merge=auto");
        config.autonomy.default = patch("review=strict");
        let file = RepoConfig::default();
        let registry = patch("tools=guarded");
        let none = OperatorSettings::default();
        let layers = Layers {
            config: &config,
            global: &none,
            repo: Some(repo(&registry, &none, &file)),
        };
        let expected =
            AutonomyPolicy::resolve(&config.autonomy, Some(&registry), Some(&file.autonomy));
        assert_eq!(layers.autonomy(), expected);
        assert_eq!(layers.caps(), config.limits.caps());
        let sources = layers.effective().sources;
        assert_eq!(sources["autonomy.max.merge"], SettingSource::Config);
        assert_eq!(sources["autonomy.max.tools"], SettingSource::Registry);
        assert_eq!(sources["autonomy.max.review"], SettingSource::Builtin);
        assert_eq!(sources["caps.budget_usd_daily"], SettingSource::Config);
    }

    #[test]
    fn each_layer_overrides_the_one_beneath_and_the_repo_file_only_lowers() {
        let config = Config::default();
        let global = OperatorSettings {
            autonomy_max: patch("merge=auto"),
            autonomy_default: patch("merge=auto"),
            budget_usd_per_item: Some(5.0),
            ..OperatorSettings::default()
        };
        let overlay = OperatorSettings {
            autonomy_max: patch("tools=guarded"),
            budget_usd_per_item: Some(2.0),
            ..OperatorSettings::default()
        };
        let mut file = RepoConfig::default();
        file.autonomy.max = patch("merge=manual,review=strict");
        file.autonomy.default = patch("merge=auto,tools=yolo");
        let registry = AutonomyPatch::default();
        let layers = Layers {
            config: &config,
            global: &global,
            repo: Some(repo(&registry, &overlay, &file)),
        };
        let policy = layers.autonomy();
        assert_eq!(policy.ceiling.merge, MergeMode::Manual, "the file lowers");
        assert_eq!(policy.ceiling.review, ReviewLevel::Strict);
        assert_eq!(policy.ceiling.tools, ToolsMode::Guarded, "repo overlay");
        assert_eq!(
            policy.default.merge,
            MergeMode::Manual,
            "held at the ceiling"
        );
        assert_eq!(
            policy.default.tools,
            ToolsMode::Guarded,
            "the file cannot raise"
        );
        assert_eq!(layers.caps().budget_usd_per_item, 2.0);
        let sources = layers.effective().sources;
        assert_eq!(sources["autonomy.max.merge"], SettingSource::RepoFile);
        assert_eq!(sources["autonomy.max.tools"], SettingSource::Repo);
        assert_eq!(sources["caps.budget_usd_per_item"], SettingSource::Repo);
        let global_only = Layers {
            repo: None,
            ..layers
        };
        assert_eq!(global_only.autonomy().ceiling.merge, MergeMode::Auto);
        assert_eq!(global_only.caps().budget_usd_per_item, 5.0);
    }

    #[test]
    fn a_repo_overlay_can_raise_or_lower_both_default_and_ceiling() {
        let mut config = Config::default();
        config.autonomy.max = patch("tools=guarded");
        let global = OperatorSettings::default();
        let overlay = OperatorSettings {
            autonomy_max: patch("tools=yolo,review=standard"),
            autonomy_default: patch("tools=yolo"),
            ..OperatorSettings::default()
        };
        let file = RepoConfig::default();
        let registry = AutonomyPatch::default();
        let layers = Layers {
            config: &config,
            global: &global,
            repo: Some(repo(&registry, &overlay, &file)),
        };
        let policy = layers.autonomy();
        assert_eq!(policy.ceiling.tools, ToolsMode::Yolo, "raised");
        assert_eq!(policy.ceiling.review, ReviewLevel::Standard, "lowered");
        assert_eq!(policy.default.tools, ToolsMode::Yolo);
    }

    #[test]
    fn budgets_switch_per_repository() {
        let config = Config::default();
        let global = OperatorSettings {
            budgets_enabled: Some(false),
            ..OperatorSettings::default()
        };
        let on = OperatorSettings {
            budgets_enabled: Some(true),
            budget_usd_per_item: Some(1.5),
            ..OperatorSettings::default()
        };
        let file = RepoConfig::default();
        let registry = AutonomyPatch::default();
        let layers = Layers {
            config: &config,
            global: &global,
            repo: None,
        };
        assert!(!layers.caps().budgets_enabled);
        let repo_on = Layers {
            repo: Some(repo(&registry, &on, &file)),
            ..layers
        };
        assert!(repo_on.caps().budgets_enabled);
        assert_eq!(repo_on.caps().budget_usd_per_item, 1.5);
    }

    #[test]
    fn default_pipeline_prefers_the_most_specific_layer() {
        let config = Config::default();
        let global = OperatorSettings {
            default_pipeline: Some("direct".into()),
            ..OperatorSettings::default()
        };
        let none = OperatorSettings::default();
        let bare_file = RepoConfig::default();
        let mut file = RepoConfig::default();
        file.repo.default_pipeline = Some("reviewed".into());
        let registry = AutonomyPatch::default();
        let bare = Layers {
            config: &config,
            global: &global,
            repo: Some(repo(&registry, &none, &bare_file)),
        };
        assert_eq!(
            bare.default_pipeline(),
            Some(("direct".into(), SettingSource::Global))
        );
        let overlay = OperatorSettings {
            default_pipeline: Some("team".into()),
            ..OperatorSettings::default()
        };
        let mut with_file = Layers {
            repo: Some(repo(&registry, &none, &file)),
            ..bare
        };
        assert_eq!(with_file.default_pipeline().unwrap().0, "reviewed");
        with_file.repo = Some(repo(&registry, &overlay, &file));
        assert_eq!(
            with_file.default_pipeline(),
            Some(("team".into(), SettingSource::Repo))
        );
    }

    #[test]
    fn validation_refuses_what_a_scope_cannot_enforce() {
        let full_auto = OperatorSettings {
            full_auto: Some(true),
            ..OperatorSettings::default()
        };
        let err = full_auto.validate(SettingsScope::Global).unwrap_err();
        assert!(
            err.to_string().contains("full_auto is not supported"),
            "{err}"
        );
        let off = OperatorSettings {
            full_auto: Some(false),
            ..OperatorSettings::default()
        };
        off.validate(SettingsScope::Repo).unwrap();

        let daily = OperatorSettings {
            budget_usd_daily: Some(3.0),
            max_active_runs: Some(2),
            ..OperatorSettings::default()
        };
        daily.validate(SettingsScope::Global).unwrap();
        let err = daily.validate(SettingsScope::Repo).unwrap_err().to_string();
        assert!(err.contains("budget_usd_daily, max_active_runs"), "{err}");

        let negative = OperatorSettings {
            budget_usd_per_item: Some(-1.0),
            ..OperatorSettings::default()
        };
        assert!(negative.validate(SettingsScope::Repo).is_err());
        let nan = OperatorSettings {
            budget_usd_total: Some(f64::NAN),
            ..OperatorSettings::default()
        };
        assert!(nan.validate(SettingsScope::Global).is_err());

        let above = OperatorSettings {
            autonomy_default: patch("merge=auto"),
            autonomy_max: patch("merge=manual"),
            ..OperatorSettings::default()
        };
        let err = above
            .validate(SettingsScope::Global)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("autonomy_default exceeds autonomy_max"),
            "{err}"
        );
    }

    #[test]
    fn unknown_fields_are_refused_and_empty_settings_serialize_empty() {
        let typo = serde_json::from_str::<OperatorSettings>(r#"{"budget_usd":1}"#);
        assert!(typo.is_err());
        let empty = serde_json::to_string(&OperatorSettings::default()).unwrap();
        assert_eq!(empty, "{}");
    }

    #[test]
    fn set_caps_round_trips_any_caps() {
        let caps = Caps {
            budgets_enabled: false,
            max_parallel_agents: 3,
            budget_usd_daily: 9.0,
            ..Caps::default()
        };
        let mut overlay = OperatorSettings::default();
        overlay.set_caps(&caps);
        let mut other_base = Caps::default();
        other_base.max_per_repo = 7;
        assert_eq!(overlay.caps_over(other_base), caps);
    }
}
