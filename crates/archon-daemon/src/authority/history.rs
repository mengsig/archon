//! Settings history stores a concrete inverse, guarded by the state it undoes.

use archon_core::authority::OperatorMessage;
use archon_core::error::ArchonError;
use archon_core::notify::FeedEntry;
use archon_core::protocol::{Clearable, RepoPatch, Request, TiersPatch};
use archon_core::repo::Repo;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{context::DaemonContext, feed};

#[derive(Serialize, Deserialize)]
struct Change {
    restore: Request,
    after: Value,
}

fn key(seq: u64) -> String {
    format!("authority_change:{seq}")
}

pub(super) async fn resolve(ctx: &DaemonContext, action: Request) -> Result<Request, ArchonError> {
    let Request::UndoAuthority { change } = action else {
        return Ok(action);
    };
    let raw = ctx
        .store
        .lock()
        .await
        .kv_get(&key(change))?
        .ok_or_else(|| ArchonError::NotFound(format!("undoable authority change {change}")))?;
    let change: Change = serde_json::from_str(&raw)?;
    if super::actions::snapshot(ctx, &change.restore).await? != change.after {
        return Err(ArchonError::InvalidTransition(
            "settings changed after this entry; undo would overwrite a newer change".into(),
        ));
    }
    Ok(change.restore)
}

pub(super) async fn record(
    ctx: &DaemonContext,
    evidence: &OperatorMessage,
    action: &Request,
    before: Value,
    after: Value,
) -> Result<(), ArchonError> {
    let restore = match action {
        Request::SetSettings { repo, .. } | Request::ResetSettings { repo } => {
            let old: archon_core::settings::SettingsView = serde_json::from_value(before.clone())?;
            Some(Request::SetSettings {
                repo: repo.clone(),
                settings: old.scoped,
            })
        }
        Request::UpdateItem { item, .. } => Some(Request::UpdateItem {
            item: item.clone(),
            patch: archon_core::protocol::ItemPatch {
                autonomy: Some(serde_json::from_value(before["autonomy"].clone())?),
                ..Default::default()
            },
        }),
        Request::RepoUpdate { repo, .. } => {
            let old: Repo = serde_json::from_value(before.clone())?;
            Some(Request::RepoUpdate {
                repo: repo.clone(),
                patch: RepoPatch {
                    ship: Some(old.ship),
                    base_branch: optional(old.base_branch),
                    default_pipeline: optional(old.default_pipeline),
                    autonomy_max: Some(old.autonomy_max),
                    grant_merge: Some(old.grant_merge),
                    attribution: Some(old.attribution),
                    tiers: TiersPatch {
                        integration_branch: optional(old.tiers.integration_branch),
                        release_branch: optional(old.tiers.release_branch),
                        fast_check: optional(old.tiers.checks.fast),
                        full_check: optional(old.tiers.checks.full),
                        release_hook: optional(old.tiers.checks.release),
                        confirmed_fast_check: optional(old.confirmed_fast_check),
                        confirmed_full_check: optional(old.confirmed_full_check),
                        confirmed_release_check: optional(old.confirmed_release_check),
                    },
                },
            })
        }
        Request::SetCaps { .. } => {
            let old: archon_core::settings::SettingsView = serde_json::from_value(before.clone())?;
            Some(Request::SetSettings {
                repo: None,
                settings: old.scoped,
            })
        }
        _ => None,
    };
    let mut store = ctx.store.lock().await;
    let seq = store.last_feed_seq()? + 1;
    let result = if restore.is_some() {
        format!("applied; undo with /archon undo {seq}")
    } else {
        "applied".into()
    };
    let entry = FeedEntry::Authority {
        evidence: evidence.clone(),
        action: crate::handlers::command_name(action),
        result,
        before,
        after: after.clone(),
    };
    let item = match restore {
        Some(restore) => {
            let record = serde_json::to_string(&Change { restore, after })?;
            store.append_feed_with_kv(entry, archon_core::now(), &[(&key(seq), &record)])?
        }
        None => store.append_feed(entry, archon_core::now())?,
    };
    drop(store);
    feed::publish(ctx, vec![item]);
    Ok(())
}

fn optional(value: Option<String>) -> Clearable<String> {
    match value {
        Some(value) => Clearable::Set(value),
        None => Clearable::Clear,
    }
}
