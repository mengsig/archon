//! What applies to one repository: its `.archon/config.toml`, the model
//! policy narrowed by it, its autonomy policy, its pipeline catalog, and
//! the daemon's caps, each through the operator's settings overlays.

use archon_core::autonomy::AutonomyPolicy;
use archon_core::config::{Config, RepoConfig};
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::model_tier::Tier;
use archon_core::policy::ModelPolicy;
use archon_core::protocol::RepoRef;
use archon_core::provider_route::ProviderWeights;
use archon_core::queue::Caps;
use archon_core::repo::{Repo, RepoTiers};
use archon_core::settings::{EffectiveSettings, Layers, OperatorSettings, RepoLayers};
use archon_pipeline::{Catalog, CatalogSources};

use crate::context::DaemonContext;

/// The global caps in force: `[limits]` under the global settings overlay,
/// where `SetCaps` persists.
pub(crate) async fn caps(ctx: &DaemonContext) -> Caps {
    let global = crate::settings::mirror(ctx).global.clone();
    global.caps_over(ctx.config.limits.caps())
}

/// The caps for an item in `repo`: the global ones with the repository
/// overlay's budgets and per-repo cap.
pub(crate) fn item_caps(ctx: &DaemonContext, repo: Option<&RepoId>) -> Caps {
    let book = crate::settings::mirror(ctx);
    let global = book.global.caps_over(ctx.config.limits.caps());
    match repo.and_then(|id| book.repos.get(id)) {
        Some(overlay) => overlay.caps_over(global),
        None => global,
    }
}

pub(crate) fn repo_ref(id: &RepoId) -> RepoRef {
    RepoRef(id.as_str().to_owned())
}

/// A repository (or none, for repo-less items) with its local config.
#[derive(Debug, Clone)]
pub(crate) struct RepoEnv {
    pub repo: Option<Repo>,
    pub config: RepoConfig,
}

impl RepoEnv {
    /// Reads `<repo>/.archon/config.toml`; a broken file is an error so a
    /// narrowed model policy can never be skipped silently.
    pub fn new(repo: Option<Repo>) -> Result<Self, ArchonError> {
        let config = match &repo {
            Some(repo) => RepoConfig::load(&repo.path)?,
            None => RepoConfig::default(),
        };
        Ok(Self { repo, config })
    }

    /// Looks up `repo` (when given) in the store, then reads its config.
    /// The store lock is only held for the lookup.
    pub async fn load(ctx: &DaemonContext, repo: Option<&RepoId>) -> Result<Self, ArchonError> {
        let repo = match repo {
            Some(id) => Some(ctx.store.lock().await.repo(&repo_ref(id))?),
            None => None,
        };
        Self::new(repo)
    }

    /// The most agents an item of `pipeline` here may run at once; 0 when
    /// no cap applies.
    pub fn agent_cap(&self, ctx: &DaemonContext, pipeline: &str) -> u32 {
        let caps = item_caps(ctx, self.repo.as_ref().map(|r| &r.id));
        let repo_cap = self
            .repo
            .as_ref()
            .map(|_| self.config.limits.max_per_repo.unwrap_or(0));
        archon_queue::dispatch::effective_agent_cap(&caps, repo_cap, pipeline)
    }

    /// The global policy narrowed by the repository's `[models]`.
    pub fn policy(&self, config: &Config) -> ModelPolicy {
        config
            .models
            .policy()
            .narrowed_by(&self.config.models.policy())
    }

    /// The provider weights routing this repository's tier-only choices;
    /// `None` when neither `[models]` sets any.
    pub fn provider_weights(&self, config: &Config) -> Option<ProviderWeights> {
        config.models.provider_weights(&self.config.models)
    }

    /// The highest tier a default picks for this repository.
    pub fn max_default_tier(&self, config: &Config) -> Tier {
        config.models.max_default_tier(&self.config.models)
    }

    /// Global `[autonomy]` and overlay, the registry ceiling and repository
    /// overlay, then the repository's own file, which may only lower
    /// (ARCHITECTURE §18.2).
    pub fn autonomy(&self, ctx: &DaemonContext) -> AutonomyPolicy {
        self.with_layers(ctx, |layers| layers.autonomy())
    }

    /// The pipeline an enqueue naming none gets.
    pub fn default_pipeline(&self, ctx: &DaemonContext) -> Option<String> {
        self.with_layers(ctx, |layers| {
            layers.default_pipeline().map(|(name, _)| name)
        })
    }

    /// Every value in force for this scope, with its source.
    pub fn effective(&self, ctx: &DaemonContext) -> EffectiveSettings {
        self.with_layers(ctx, |layers| layers.effective())
    }

    /// What `effective` would be with `settings` stored at this env's scope.
    pub fn effective_with(
        &self,
        ctx: &DaemonContext,
        settings: &OperatorSettings,
    ) -> EffectiveSettings {
        self.layers_over(ctx, Some(settings), |layers| layers.effective())
    }

    fn with_layers<R>(&self, ctx: &DaemonContext, f: impl FnOnce(&Layers<'_>) -> R) -> R {
        self.layers_over(ctx, None, f)
    }

    /// The overlays copied out of the mirror, so none is held while `f` runs;
    /// `scoped` stands in for the one stored at this env's scope.
    fn layers_over<R>(
        &self,
        ctx: &DaemonContext,
        scoped: Option<&OperatorSettings>,
        f: impl FnOnce(&Layers<'_>) -> R,
    ) -> R {
        let (mut global, mut overlay) = {
            let book = crate::settings::mirror(ctx);
            let overlay = self.repo.as_ref().map(|r| book.get(Some(&r.id)));
            (book.global.clone(), overlay)
        };
        match (scoped, overlay.as_mut()) {
            (Some(settings), Some(overlay)) => *overlay = settings.clone(),
            (Some(settings), None) => global = settings.clone(),
            (None, _) => {}
        }
        let empty = OperatorSettings::default();
        let repo = self.repo.as_ref().map(|r| RepoLayers {
            registry_max: &r.autonomy_max,
            registry_pipeline: r.default_pipeline.as_deref(),
            overlay: overlay.as_ref().unwrap_or(&empty),
            file: &self.config,
        });
        f(&Layers {
            config: &ctx.config,
            global: &global,
            repo,
        })
    }

    /// How ceiling refusals name where the ceiling applies.
    pub fn scope(&self) -> String {
        match self.repo_name() {
            Some(name) => format!("repository {name:?}"),
            None => "items without a repository".to_owned(),
        }
    }

    /// Built-in, user and repository pipelines, minus the disabled ones.
    pub fn catalog(&self) -> Result<Catalog, ArchonError> {
        Catalog::discover(&CatalogSources {
            user_dir: Some(archon_core::paths::config_dir().join("pipelines")),
            repo_root: self.repo.as_ref().map(|r| r.path.clone()),
            disabled: self.config.pipelines.disabled.clone(),
        })
    }

    pub fn repo_name(&self) -> Option<&str> {
        self.repo.as_ref().map(|r| r.name.as_str())
    }

    /// The registry's verification tiers over the committed
    /// `.archon/repo.toml`; none for a repo-less item.
    pub fn tiers(&self) -> Result<RepoTiers, ArchonError> {
        let Some(repo) = &self.repo else {
            return Ok(RepoTiers::default());
        };
        let tiers = repo.tiers.clone().over(RepoTiers::load(&repo.path)?);
        tiers.validate()?;
        Ok(tiers)
    }

    /// The branch new work forks from and delivers to: the integration
    /// branch in `tiers`, else the registry's base branch, else the file's.
    pub fn fork_base(&self, tiers: &RepoTiers) -> Option<String> {
        assert!(
            self.repo.is_some() || tiers.is_empty(),
            "only a repository has tiers"
        );
        tiers
            .integration_branch
            .clone()
            .or_else(|| self.repo.as_ref().and_then(|r| r.base_branch.clone()))
            .or_else(|| self.config.repo.base_branch.clone())
    }
}
