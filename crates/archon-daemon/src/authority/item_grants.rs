//! Durable, exact-value ceiling exceptions for one item. They survive retry
//! and restart, but changing an authorized knob revokes its exception. An
//! item's repository and pipeline are immutable, so its ID bounds the grant.

use archon_core::autonomy::{Autonomy, AutonomyPatch, Knob};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::queue::QueueItem;
use archon_queue::Store;

fn key(id: &QueueItemId) -> String {
    format!("item_autonomy_grant:{id}")
}

fn read(store: &Store, id: &QueueItemId) -> Result<AutonomyPatch, ArchonError> {
    store
        .kv_get(&key(id))?
        .map(|raw| serde_json::from_str(&raw).map_err(ArchonError::from))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Only the explicitly approved knobs that exceed the current ceiling.
pub(crate) fn approved(patch: AutonomyPatch, after: Autonomy, ceiling: Autonomy) -> AutonomyPatch {
    fn above<K: Knob>(asked: Option<K>, after: K, ceiling: K) -> Option<K> {
        (asked.is_some() && after.rank() > ceiling.rank()).then_some(after)
    }
    AutonomyPatch {
        review: above(patch.review, after.review, ceiling.review),
        proposal: above(patch.proposal, after.proposal, ceiling.proposal),
        report: above(patch.report, after.report, ceiling.report),
        merge: above(patch.merge, after.merge, ceiling.merge),
        tools: above(patch.tools, after.tools, ceiling.tools),
    }
}

/// Keep grants only while their exact value stays unchanged. Persist the
/// item and grant together, so a crash cannot leave half an authorization.
pub(crate) fn save(
    store: &mut Store,
    item: &QueueItem,
    before: Autonomy,
    approved: AutonomyPatch,
) -> Result<(), ArchonError> {
    let old = read(store, &item.id)?;
    let after = item.autonomy;
    fn keep<K: Knob>(old: Option<K>, before: K, after: K, new: Option<K>) -> Option<K> {
        new.or_else(|| old.filter(|value| *value == before && before == after))
    }
    let grant = AutonomyPatch {
        review: keep(old.review, before.review, after.review, approved.review),
        proposal: keep(
            old.proposal,
            before.proposal,
            after.proposal,
            approved.proposal,
        ),
        report: keep(old.report, before.report, after.report, approved.report),
        merge: keep(old.merge, before.merge, after.merge, approved.merge),
        tools: keep(old.tools, before.tools, after.tools, approved.tools),
    };
    store.update_item_with_kv(item, &[(&key(&item.id), &serde_json::to_string(&grant)?)])
}

/// Check every unapproved knob against today's ceiling. An exact approved
/// value is masked only for this check; the item and repository stay intact.
pub(crate) fn for_ceiling_check(
    store: &Store,
    item: &QueueItem,
    ceiling: Autonomy,
) -> Result<Autonomy, ArchonError> {
    let grant = read(store, &item.id)?;
    let current = item.autonomy;
    fn checked<K: Knob>(grant: Option<K>, current: K, ceiling: K) -> K {
        if grant == Some(current) {
            current.lower(ceiling)
        } else {
            current
        }
    }
    Ok(Autonomy {
        review: checked(grant.review, current.review, ceiling.review),
        proposal: checked(grant.proposal, current.proposal, ceiling.proposal),
        report: checked(grant.report, current.report, ceiling.report),
        merge: checked(grant.merge, current.merge, ceiling.merge),
        tools: checked(grant.tools, current.tools, ceiling.tools),
    })
}
