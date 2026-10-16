//! The planner's input: the store's snapshot completed with what only the
//! daemon knows (caps, per-repository caps, pending spawns, fan-outs).

use std::collections::BTreeMap;

use archon_core::catalog::StageKind;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, RepoId};
use archon_core::model_tier::ModelCatalog;
use archon_core::protocol::ItemQuery;
use archon_core::queue::{Caps, QueueItem};
use archon_core::quota::QuotaView;
use archon_core::repo::Repo;
use archon_pipeline::Catalog;
use archon_queue::DispatchSnapshot;
use archon_queue::dispatch::{PendingSpawn, QueuedCandidate};

use crate::context::DaemonContext;
use crate::executor::RunTable;
use crate::resolve::{self, RepoEnv};

/// Called with the run table held, so pending spawns match the drivers.
/// `envs` was read before the table was taken (`Envs::prefetch`).
pub(super) async fn build(
    ctx: &DaemonContext,
    table: &RunTable,
    caps: Caps,
    mut envs: Envs,
) -> Result<DispatchSnapshot, ArchonError> {
    let now = archon_core::now();
    let mut snapshot = {
        let store = ctx.store.lock().await;
        let mut snapshot = store.dispatch_snapshot(now)?;
        if !ctx.config.limits.count_estimated_cost {
            (snapshot.spend_today_usd, snapshot.spend_total_usd) =
                store.spend_totals(now, false)?;
        }
        snapshot.pending_spawns = pending_spawns(&store, table)?;
        snapshot
    };
    snapshot.caps = caps;
    snapshot.low_disk = ctx.disk.holding();
    snapshot.draining = ctx.ops.draining();
    for candidate in &mut snapshot.queued {
        envs.read_late(ctx, candidate.repo.as_ref(), true).await?;
        (candidate.first_stage_agents, candidate.gang) = envs.first_stage(candidate);
    }
    for spawn in &snapshot.pending_spawns {
        envs.read_late(ctx, spawn.repo.as_ref(), false).await?;
    }
    quota_holds(ctx, table, &mut snapshot, &envs).await?;
    snapshot.repo_caps = envs.repo_caps();
    snapshot.repo_max_per_repo = crate::settings::mirror(ctx)
        .repos
        .iter()
        .filter_map(|(id, settings)| settings.max_per_repo.map(|limit| (id.clone(), limit)))
        .collect();
    snapshot.repo_budgets_enabled = crate::settings::mirror(ctx)
        .repos
        .iter()
        .filter_map(|(id, settings)| {
            settings
                .budgets_enabled
                .map(|enabled| (id.clone(), enabled))
        })
        .collect();
    assert!(snapshot.queued.iter().all(|q| q.first_stage_agents > 0));
    Ok(snapshot)
}

/// Each queued candidate's and pending spawn's quota hold, from the configs
/// and pipelines this pass read; none unless some provider is out of quota.
async fn quota_holds(
    ctx: &DaemonContext,
    table: &RunTable,
    snapshot: &mut DispatchSnapshot,
    envs: &Envs,
) -> Result<(), ArchonError> {
    let Some(view) = crate::quota::view(ctx).filter(QuotaView::any_exhausted) else {
        return Ok(());
    };
    let (catalog, items) = holdable(ctx, &snapshot.pending_spawns).await?;
    let global = RepoEnv::new(None)?;
    let env_of = |repo: Option<&RepoId>| match repo {
        Some(id) => envs.configs.get(id).and_then(Option::as_ref),
        None => Some(&global),
    };
    let read = (&catalog, &view);
    for candidate in &mut snapshot.queued {
        let pipelines = envs.catalogs.get(&candidate.repo).and_then(Option::as_ref);
        let item = items
            .get(&candidate.item)
            .filter(|i| i.state.kind() == "queued");
        // An item no longer queued, or whose files do not load, is not held.
        let (Some(item), Some(env), Some(pipelines)) =
            (item, env_of(candidate.repo.as_ref()), pipelines)
        else {
            continue;
        };
        candidate.quota_hold = crate::quota::item_hold(ctx, (env, pipelines), item, read);
    }
    for spawn in &mut snapshot.pending_spawns {
        let (Some(item), Some(env), Some(driver), Some(plans)) = (
            items.get(&spawn.item),
            env_of(spawn.repo.as_ref()),
            table.drivers.get(&spawn.run),
            table.pending.get(&spawn.run),
        ) else {
            continue;
        };
        let pending = (item, driver.spec().as_ref(), plans.as_slice());
        spawn.quota_hold = crate::quota::spawn_hold(ctx, env, pending, read);
    }
    debug_assert!(
        (snapshot.queued.iter())
            .filter(|c| c.quota_hold.is_some())
            .all(|c| items
                .get(&c.item)
                .is_some_and(|i| i.state.kind() == "queued")),
        "only queued items are held"
    );
    Ok(())
}

/// The model catalog, and every queued item and each of `spawns`' items by id.
async fn holdable(
    ctx: &DaemonContext,
    spawns: &[PendingSpawn],
) -> Result<(ModelCatalog, BTreeMap<QueueItemId, QueueItem>), ArchonError> {
    let store = ctx.store.lock().await;
    let query = ItemQuery {
        states: vec!["queued".into()],
        ..ItemQuery::default()
    };
    let mut items = store.items(&query)?;
    for spawn in spawns {
        items.push(store.item(&spawn.item)?);
    }
    let by_id: BTreeMap<_, _> = items.into_iter().map(|i| (i.id.clone(), i)).collect();
    debug_assert!(spawns.iter().all(|s| by_id.contains_key(&s.item)));
    Ok((store.model_catalog(), by_id))
}

/// Spawns drivers emitted that the dispatcher has not admitted yet.
fn pending_spawns(
    store: &archon_queue::Store,
    table: &RunTable,
) -> Result<Vec<PendingSpawn>, ArchonError> {
    let mut out = Vec::with_capacity(table.pending.len());
    for (run_id, plans) in &table.pending {
        if plans.is_empty() {
            continue;
        }
        let run = store.run(run_id)?;
        if run.state.is_terminal() {
            continue;
        }
        let item = store.item(&run.item)?;
        assert!(
            plans
                .iter()
                .all(|p| p.team.is_some() == plans[0].team.is_some()),
            "a team's spawns are pending alone"
        );
        let micros = item.created_at.timestamp_micros();
        assert!(micros >= 0, "items are created after 1970");
        out.push(PendingSpawn {
            run: run.id,
            item: item.id,
            repo: item.repo,
            pipeline: run.pipeline,
            priority: item.priority,
            item_created_seq: micros as u64,
            wanted: u32::try_from(plans.len()).expect("a fan-out fits in u32"),
            gang: plans.iter().any(|p| p.team.is_some()),
            quota_hold: None,
        });
    }
    Ok(out)
}

/// Reads a repository's pipeline catalog: `RepoEnv::catalog`, or a
/// counting stand-in in tests.
type Discover = fn(&RepoEnv) -> Result<Catalog, ArchonError>;

/// A repository whose files a pass reads: `id` is `None` for items without
/// one, `queued` when a queued item needs its pipeline catalog.
struct Wanted {
    id: Option<RepoId>,
    repo: Option<Repo>,
    queued: bool,
}

/// Repository configs and pipeline catalogs, read once per pass and at most
/// once per repository. Only the repositories of queued items need their
/// catalog; those of running items need their config.
#[derive(Default)]
pub(super) struct Envs {
    /// `None` records a repository whose config could not be read.
    configs: BTreeMap<RepoId, Option<RepoEnv>>,
    /// By repository (`None`: no repository); `None` when it did not load.
    catalogs: BTreeMap<Option<RepoId>, Option<Catalog>>,
}

impl Envs {
    /// Read the files of every repository with a queued or running item,
    /// before the run table is taken and off the async workers, so file
    /// I/O never holds up settles, spawns or `status`.
    pub(super) async fn prefetch(ctx: &DaemonContext) -> Result<Self, ArchonError> {
        let wanted = {
            let store = ctx.store.lock().await;
            let queued = store.item_repos("queued")?;
            let running = store.item_repos("running")?;
            let mut wanted = Vec::with_capacity(queued.len() + running.len());
            for (ids, queued) in [(queued, true), (running, false)] {
                for id in ids {
                    let repo = id.as_ref().map(|id| store.repo(&resolve::repo_ref(id)));
                    let repo = repo.transpose()?;
                    wanted.push(Wanted { id, repo, queued });
                }
            }
            wanted
        };
        tokio::task::spawn_blocking(move || {
            let mut envs = Self::default();
            for want in wanted {
                envs.read(want, RepoEnv::catalog);
            }
            envs
        })
        .await
        .map_err(|e| ArchonError::io("repository config task", std::io::Error::other(e)))
    }

    /// Read what `want` needs and is not read yet.
    fn read(&mut self, want: Wanted, discover: Discover) {
        assert_eq!(
            want.id.as_ref(),
            want.repo.as_ref().map(|r| &r.id),
            "a wanted repository is the one its id names"
        );
        let have_config = want
            .id
            .as_ref()
            .is_none_or(|id| self.configs.contains_key(id));
        let have_catalog = !want.queued || self.catalogs.contains_key(&want.id);
        if have_config && have_catalog {
            return;
        }
        let env = match RepoEnv::new(want.repo) {
            Ok(env) => Some(env),
            Err(error) => {
                tracing::warn!(repo = ?want.id, %error, "repository config unreadable; global caps apply");
                None
            }
        };
        if !have_catalog {
            let catalog = env.as_ref().and_then(|env| match discover(env) {
                Ok(catalog) => Some(catalog),
                Err(error) => {
                    tracing::debug!(repo = ?want.id, %error, "pipelines do not load; admitting one agent per item");
                    None
                }
            });
            self.catalogs.insert(want.id.clone(), catalog);
        }
        if let Some(id) = want.id {
            self.configs.entry(id).or_insert(env);
        }
    }

    /// A repository that gained a queued or running item since `prefetch`
    /// is read now, under the run table: rare, and still once per pass.
    async fn read_late(
        &mut self,
        ctx: &DaemonContext,
        id: Option<&RepoId>,
        queued: bool,
    ) -> Result<(), ArchonError> {
        let have_config = id.is_none_or(|id| self.configs.contains_key(id));
        let have_catalog = !queued || self.catalogs.contains_key(&id.cloned());
        if have_config && have_catalog {
            return Ok(());
        }
        let repo = match id {
            Some(id) => Some(ctx.store.lock().await.repo(&resolve::repo_ref(id))?),
            None => None,
        };
        let id = id.cloned();
        self.read(Wanted { id, repo, queued }, RepoEnv::catalog);
        Ok(())
    }

    /// The first stage's fan-out and whether it is a team, admitted whole;
    /// one agent when the pipeline does not load (the start then fails the
    /// item with the reason).
    fn first_stage(&self, item: &QueuedCandidate) -> (u32, bool) {
        let catalog = self.catalogs.get(&item.repo);
        assert!(catalog.is_some(), "a queued item's catalog is read first");
        let Some(Some(catalog)) = catalog else {
            return (1, false);
        };
        match catalog.get(&item.pipeline) {
            Ok(spec) => spec.file.stages.first().map_or((1, false), |stage| {
                (stage.fan_out.max(1), stage.kind == StageKind::Team)
            }),
            Err(error) => {
                tracing::debug!(item = %item.item, %error, "pipeline does not load; admitting one agent");
                (1, false)
            }
        }
    }

    /// Whether provider weights route any item this pass reads, so its
    /// routing wants a quota reading.
    pub(super) fn weighted(&self, config: &Config) -> bool {
        let none = Default::default();
        config.models.provider_weights(&none).is_some()
            || (self.configs.values().flatten()).any(|env| env.provider_weights(config).is_some())
    }

    /// `max_per_repo` from each repository's config (0 = no override).
    fn repo_caps(&self) -> BTreeMap<RepoId, u32> {
        self.configs
            .iter()
            .map(|(id, env)| {
                let cap = env.as_ref().and_then(|e| e.config.limits.max_per_repo);
                (id.clone(), cap.unwrap_or(0))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use archon_core::id::QueueItemId;
    use archon_core::queue::Priority;
    use archon_core::repo::ShipMode;
    use archon_queue::dispatch::DependencyState;

    use super::*;

    static DISCOVERED: AtomicUsize = AtomicUsize::new(0);

    fn counting_discover(_: &RepoEnv) -> Result<Catalog, ArchonError> {
        DISCOVERED.fetch_add(1, Ordering::SeqCst);
        Ok(Catalog::builtin())
    }

    fn repo(dir: &Path, name: &str, config: &str) -> Repo {
        std::fs::create_dir_all(dir.join(".archon")).unwrap();
        std::fs::write(dir.join(".archon/config.toml"), config).unwrap();
        Repo {
            id: RepoId::new(),
            name: name.to_owned(),
            path: dir.to_owned(),
            ship: ShipMode::Local,
            base_branch: None,
            default_pipeline: None,
            autonomy_max: Default::default(),
            grant_merge: false,
            attribution: Default::default(),
            gh_user: None,
            tiers: Default::default(),
            confirmed_fast_check: None,
            confirmed_full_check: None,
            confirmed_release_check: None,
            added_at: archon_core::now(),
        }
    }

    fn wanted(repo: Option<&Repo>, queued: bool) -> Wanted {
        let id = repo.map(|r| r.id.clone());
        Wanted {
            id,
            repo: repo.cloned(),
            queued,
        }
    }

    fn candidate(repo: Option<&Repo>, pipeline: &str, seq: u64) -> QueuedCandidate {
        QueuedCandidate {
            item: QueueItemId::new(),
            repo: repo.map(|r| r.id.clone()),
            pipeline: pipeline.to_owned(),
            priority: Priority::Normal,
            created_seq: seq,
            deps: DependencyState::AllSucceeded,
            first_stage_agents: 0,
            gang: false,
            quota_hold: None,
        }
    }

    #[test]
    fn a_pass_discovers_each_queued_repository_catalog_once() {
        let dir = tempfile::tempdir().unwrap();
        let queued = repo(&dir.path().join("a"), "a", "[limits]\nmax_per_repo = 2\n");
        let running = repo(&dir.path().join("b"), "b", "");
        let broken = repo(&dir.path().join("c"), "c", "not toml [");
        let mut envs = Envs::default();
        for want in [
            wanted(Some(&queued), true),
            wanted(None, true),
            wanted(Some(&running), false),
            wanted(Some(&broken), false),
            wanted(Some(&queued), true),
            wanted(Some(&running), false),
        ] {
            envs.read(want, counting_discover);
        }
        assert_eq!(
            DISCOVERED.load(Ordering::SeqCst),
            2,
            "repository a and no repository"
        );

        let spec = Catalog::builtin().get("refactor").unwrap().clone();
        let fan_out = spec.file.stages[0].fan_out.max(1);
        for seq in 0..20 {
            let repo = (seq % 2 == 0).then_some(&queued);
            assert_eq!(
                envs.first_stage(&candidate(repo, "refactor", seq)),
                (fan_out, false)
            );
            assert_eq!(
                envs.first_stage(&candidate(repo, "no-such-pipeline", seq)),
                (1, false)
            );
        }
        assert_eq!(
            DISCOVERED.load(Ordering::SeqCst),
            2,
            "candidates reuse the pass's catalogs"
        );
        let caps = envs.repo_caps();
        assert_eq!(caps.len(), 3);
        assert_eq!(caps[&queued.id], 2);
        assert_eq!(caps[&running.id], 0);
        assert_eq!(
            caps[&broken.id], 0,
            "an unreadable config leaves the global caps"
        );
    }
}
