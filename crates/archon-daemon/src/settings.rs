//! The operator's persisted settings overlays (`archon_core::settings`):
//! stored in the store's key-value table, mirrored in memory so policy
//! reads need no store lock.

use std::collections::BTreeMap;

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::settings::{EffectiveSettings, OperatorSettings, SettingsScope};
use archon_queue::Store;

use crate::context::DaemonContext;
use crate::resolve::repo_ref;

const GLOBAL_KEY: &str = "operator_settings/global";
const REPO_PREFIX: &str = "operator_settings/repo/";

/// Every stored overlay. Owned by `DaemonContext::operator_settings`; the
/// store is the source of truth and this mirrors it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsBook {
    pub global: OperatorSettings,
    pub repos: BTreeMap<RepoId, OperatorSettings>,
}

impl SettingsBook {
    /// Every overlay in `store`; a stored value that no longer parses is an
    /// error, never an empty overlay that would silently loosen or tighten.
    pub fn load(store: &Store) -> Result<Self, ArchonError> {
        let global = match store.kv_get(GLOBAL_KEY)? {
            Some(json) => parse(GLOBAL_KEY, &json)?,
            None => OperatorSettings::default(),
        };
        let mut repos = BTreeMap::new();
        for (key, json) in store.kv_prefix(REPO_PREFIX)? {
            let id = RepoId::parse(&key[REPO_PREFIX.len()..])?;
            repos.insert(id, parse(&key, &json)?);
        }
        Ok(Self { global, repos })
    }

    /// Like [`SettingsBook::load`], but an overlay that no longer parses is
    /// left out (that scope runs on its defaults) and returned as its key
    /// with the error, so one bad record cannot stop the daemon.
    pub fn load_lenient(store: &Store) -> Result<(Self, Vec<(String, ArchonError)>), ArchonError> {
        let mut book = Self::default();
        let mut bad = Vec::new();
        if let Some(json) = store.kv_get(GLOBAL_KEY)? {
            match parse(GLOBAL_KEY, &json) {
                Ok(global) => book.global = global,
                Err(error) => bad.push((GLOBAL_KEY.to_owned(), error)),
            }
        }
        for (key, json) in store.kv_prefix(REPO_PREFIX)? {
            let id = RepoId::parse(&key[REPO_PREFIX.len()..]);
            match id.and_then(|id| Ok((id, parse(&key, &json)?))) {
                Ok((id, overlay)) => {
                    book.repos.insert(id, overlay);
                }
                Err(error) => bad.push((key, error)),
            }
        }
        Ok((book, bad))
    }

    /// Whether the overlay stored under `key` still fails to parse.
    pub fn is_corrupt(store: &Store, key: &str) -> Result<bool, ArchonError> {
        Ok(store
            .kv_get(key)?
            .is_some_and(|json| parse(key, &json).is_err()))
    }

    pub fn get(&self, repo: Option<&RepoId>) -> OperatorSettings {
        match repo {
            Some(id) => self.repos.get(id).cloned().unwrap_or_default(),
            None => self.global.clone(),
        }
    }

    fn put(&mut self, repo: Option<&RepoId>, settings: OperatorSettings) {
        match (repo, settings.is_empty()) {
            (Some(id), true) => {
                self.repos.remove(id);
            }
            (Some(id), false) => {
                self.repos.insert(id.clone(), settings);
            }
            (None, _) => self.global = settings,
        }
    }
}

fn parse(key: &str, json: &str) -> Result<OperatorSettings, ArchonError> {
    serde_json::from_str(json)
        .map_err(|e| ArchonError::Corrupt(format!("stored settings {key:?} do not parse: {e}")))
}

fn key(repo: Option<&RepoId>) -> String {
    match repo {
        Some(id) => format!("{REPO_PREFIX}{}", id.as_str()),
        None => GLOBAL_KEY.to_owned(),
    }
}

/// The overlay stored at one scope; empty when nothing is set.
pub async fn read(
    ctx: &DaemonContext,
    repo: Option<&RepoId>,
) -> Result<OperatorSettings, ArchonError> {
    if let Some(id) = repo {
        ctx.store.lock().await.repo(&repo_ref(id))?;
    }
    Ok(mirror(ctx).get(repo))
}

/// Replace the overlay at one scope whole, after validating it for that
/// scope. Callers serialize authority changes; this takes no authority lock.
pub async fn write(
    ctx: &DaemonContext,
    repo: Option<&RepoId>,
    settings: OperatorSettings,
) -> Result<(), ArchonError> {
    let scope = match repo {
        Some(_) => SettingsScope::Repo,
        None => SettingsScope::Global,
    };
    settings.validate(scope)?;
    let json = serde_json::to_string(&settings).expect("settings serialize");
    {
        let mut store = ctx.store.lock().await;
        if let Some(id) = repo {
            store.repo(&repo_ref(id))?;
        }
        store.kv_set(&key(repo), &json)?;
        mirror(ctx).put(repo, settings);
    }
    tracing::info!(repo = ?repo, settings = %json, "operator settings changed");
    ctx.dispatch.notify_one();
    Ok(())
}

/// Forget the overlay at one scope; what lies beneath applies again.
pub async fn reset(ctx: &DaemonContext, repo: Option<&RepoId>) -> Result<(), ArchonError> {
    {
        let mut store = ctx.store.lock().await;
        if let Some(id) = repo {
            store.repo(&repo_ref(id))?;
        }
        store.kv_delete(&key(repo))?;
        mirror(ctx).put(repo, OperatorSettings::default());
    }
    tracing::info!(repo = ?repo, "operator settings reset");
    ctx.dispatch.notify_one();
    Ok(())
}

/// The values in force were `settings` stored at this scope, through the
/// same precedence as the real ones; nothing is stored.
pub async fn preview(
    ctx: &DaemonContext,
    repo: Option<&RepoId>,
    settings: &OperatorSettings,
) -> Result<EffectiveSettings, ArchonError> {
    let scope = match repo {
        Some(_) => SettingsScope::Repo,
        None => SettingsScope::Global,
    };
    settings.validate(scope)?;
    let env = crate::resolve::RepoEnv::load(ctx, repo).await?;
    Ok(env.effective_with(ctx, settings))
}

/// The in-memory mirror. Held only for a copy or an update, never across
/// an await, so a poisoned lock means a panic mid-update: refuse to go on.
pub(crate) fn mirror(ctx: &DaemonContext) -> std::sync::MutexGuard<'_, SettingsBook> {
    ctx.operator_settings
        .lock()
        .expect("the settings mirror is never held across a panic")
}
