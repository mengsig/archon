//! Which operator directives a trigger restates (ARCHITECTURE §18.5): all of
//! them once per orchestrator generation, and after that only what changed.
//! Whenever the orchestrator may have lost them (a new generation, a restart,
//! a compacted context) the next trigger states them in full again. An
//! orchestrator an earlier Archon started is told all of them every time.

use archon_core::Timestamp;
use archon_core::autonomy::Directive;
use archon_core::error::ArchonError;
use archon_queue::Store;
use serde::{Deserialize, Serialize};

/// `kv`: the directives the current orchestrator generation was last sent.
const TOLD_KEY: &str = "trigger_directives_told";
/// `kv`: the first orchestrator generation whose prompt reads
/// `directive_changes`; an older one expects every directive in each trigger.
const CHANGES_FROM_KEY: &str = "trigger_directive_changes_from";

/// The directives set and withdrawn since the previous trigger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectiveChanges {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<Directive>,
    /// Cleared or expired.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleared: Vec<Directive>,
}

/// What one batch says about the directives: every one in force, the
/// changes since the batch before it, or nothing when none changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DirectiveNews {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directives: Option<Vec<Directive>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directive_changes: Option<DirectiveChanges>,
    /// How many are in force, when the batch does not state them all.
    #[serde(default)]
    in_force: u32,
}

impl DirectiveNews {
    /// How many directives are in force as of this batch. Every trigger says
    /// it, so an orchestrator that holds fewer knows to read them again.
    pub fn in_force(&self) -> u32 {
        match &self.directives {
            Some(all) => count(all),
            None => self.in_force,
        }
    }
}

fn count(directives: &[Directive]) -> u32 {
    u32::try_from(directives.len()).expect("directives are few")
}

#[derive(Debug, Serialize, Deserialize)]
struct Told {
    generation: u64,
    directives: Vec<Directive>,
}

/// The directive news of the batch being keyed for orchestrator
/// `generation`, recorded as told so the next batch carries only changes.
pub(super) fn for_batch(
    store: &mut Store,
    generation: u64,
    now: Timestamp,
) -> Result<DirectiveNews, ArchonError> {
    let current = store.directives(now)?;
    let reads_changes = changes_from(store)?.is_some_and(|first| generation >= first);
    let news = match read_told(store)? {
        Some(told) if reads_changes && told.generation == generation => DirectiveNews {
            directives: None,
            directive_changes: changes(&told.directives, &current),
            in_force: count(&current),
        },
        _ => DirectiveNews {
            directives: Some(current.clone()),
            directive_changes: None,
            in_force: count(&current),
        },
    };
    assert!(
        news.directives.is_none() || news.directive_changes.is_none(),
        "a batch restates every directive or the changes, never both"
    );
    record_told(store, generation, current)?;
    Ok(news)
}

/// Orchestrator `generation` was started with this Archon's prompt, as is
/// every later one: from it on, a trigger may carry only the changes.
pub(super) fn teach_changes(store: &mut Store, generation: u64) -> Result<(), ArchonError> {
    if changes_from(store)?.is_some_and(|first| first <= generation) {
        return Ok(());
    }
    store.kv_set(CHANGES_FROM_KEY, &generation.to_string())
}

fn changes_from(store: &Store) -> Result<Option<u64>, ArchonError> {
    let Some(raw) = store.kv_get(CHANGES_FROM_KEY)? else {
        return Ok(None);
    };
    let first = raw
        .parse()
        .map_err(|e| ArchonError::Corrupt(format!("kv {CHANGES_FROM_KEY} = {raw:?}: {e}")))?;
    Ok(Some(first))
}

/// Record that orchestrator `generation` was sent `directives` in full.
pub(super) fn record_told(
    store: &mut Store,
    generation: u64,
    directives: Vec<Directive>,
) -> Result<(), ArchonError> {
    let told = Told {
        generation,
        directives,
    };
    store.kv_set(TOLD_KEY, &serde_json::to_string(&told)?)
}

/// Forget what the orchestrator was told, so the next batch restates every
/// directive. `true` when any is in force, so there is something to restate.
pub(super) fn forget_told(store: &mut Store, now: Timestamp) -> Result<bool, ArchonError> {
    store.kv_delete(TOLD_KEY)?;
    Ok(!store.directives(now)?.is_empty())
}

fn read_told(store: &Store) -> Result<Option<Told>, ArchonError> {
    let Some(raw) = store.kv_get(TOLD_KEY)? else {
        return Ok(None);
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| ArchonError::Corrupt(format!("kv {TOLD_KEY} = {raw:?}: {e}")))
}

/// What differs between the directives `told` and those in force `now`, a
/// changed text or expiry counting as set again; `None` when nothing does.
fn changes(told: &[Directive], now: &[Directive]) -> Option<DirectiveChanges> {
    let set: Vec<Directive> = now
        .iter()
        .filter(|directive| !told.contains(directive))
        .cloned()
        .collect();
    let cleared: Vec<Directive> = told
        .iter()
        .filter(|old| now.iter().all(|kept| kept.id != old.id))
        .cloned()
        .collect();
    assert!(
        set.len() <= now.len() && cleared.len() <= told.len(),
        "a change names a directive of one of the two lists"
    );
    if set.is_empty() && cleared.is_empty() {
        return None;
    }
    Some(DirectiveChanges { set, cleared })
}

#[cfg(test)]
mod tests {
    use archon_core::autonomy::DirectiveSource;

    use super::*;

    fn directive(text: &str) -> Directive {
        Directive::new(text, DirectiveSource::Operator, None, archon_core::now()).unwrap()
    }

    #[test]
    fn unchanged_directives_are_no_news() {
        let kept = vec![directive("no yolo today")];
        assert_eq!(changes(&kept, &kept), None);
        assert_eq!(changes(&[], &[]), None);
    }

    #[test]
    fn a_change_names_what_was_set_and_what_was_cleared() {
        let (kept, gone, new) = (directive("keep"), directive("gone"), directive("new"));
        let told = vec![kept.clone(), gone.clone()];
        let now = vec![kept, new.clone()];
        let found = changes(&told, &now).expect("two changes");
        assert_eq!(found.set, vec![new]);
        assert_eq!(found.cleared, vec![gone]);
    }

    #[test]
    fn a_directive_reworded_under_its_id_is_set_again_and_not_cleared() {
        let before = directive("merge docs fixes yourself");
        let mut after = before.clone();
        after.text = "merge docs and test fixes yourself".into();
        let found = changes(&[before], std::slice::from_ref(&after)).expect("a change");
        assert_eq!(found.set, vec![after]);
        assert!(found.cleared.is_empty(), "{found:?}");
    }

    #[test]
    fn a_generation_is_told_everything_once_and_then_only_changes() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let first = directive("no yolo today");
        store.insert_directive(&first).unwrap();
        teach_changes(&mut store, 1).unwrap();
        let full = for_batch(&mut store, 1, now).unwrap();
        assert_eq!(full.directives, Some(vec![first.clone()]));
        assert_eq!(full.directive_changes, None);

        let quiet = for_batch(&mut store, 1, now).unwrap();
        assert_eq!(
            (&quiet.directives, &quiet.directive_changes),
            (&None, &None),
            "nothing changed"
        );
        assert_eq!((full.in_force(), quiet.in_force()), (1, 1));

        let second = directive("merge docs fixes yourself");
        store.insert_directive(&second).unwrap();
        let delta = for_batch(&mut store, 1, now).unwrap();
        assert_eq!(delta.directives, None);
        assert_eq!(delta.in_force(), 2, "the count follows every change");
        let changed = delta.directive_changes.expect("one was set");
        assert_eq!(changed.set, vec![second.clone()]);
        assert!(changed.cleared.is_empty());

        let next = for_batch(&mut store, 2, now).unwrap();
        assert_eq!(
            next.directives,
            Some(vec![first, second]),
            "a new generation is told everything"
        );
    }

    #[test]
    fn forgetting_what_was_told_restates_everything() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        assert!(!forget_told(&mut store, now).unwrap(), "none in force");
        let only = directive("no yolo today");
        store.insert_directive(&only).unwrap();
        teach_changes(&mut store, 1).unwrap();
        for_batch(&mut store, 1, now).unwrap();
        assert!(forget_told(&mut store, now).unwrap());
        let again = for_batch(&mut store, 1, now).unwrap();
        assert_eq!(again.directives, Some(vec![only]));
    }

    /// Its prompt says every trigger carries the directives in force: one
    /// without them would read as none.
    #[test]
    fn an_orchestrator_an_earlier_archon_started_is_told_everything_every_time() {
        let mut store = Store::open_in_memory().unwrap();
        let now = archon_core::now();
        let only = directive("no yolo today");
        store.insert_directive(&only).unwrap();
        for _ in 0..2 {
            let news = for_batch(&mut store, 3, now).unwrap();
            assert_eq!(news.directives, Some(vec![only.clone()]));
        }

        teach_changes(&mut store, 4).unwrap();
        let old = for_batch(&mut store, 3, now).unwrap();
        assert!(old.directives.is_some(), "generation 3 predates the change");
        let first = for_batch(&mut store, 4, now).unwrap();
        let second = for_batch(&mut store, 4, now).unwrap();
        assert_eq!(first.directives, Some(vec![only]));
        assert_eq!(second.directives, None, "generation 4 reads changes");
        teach_changes(&mut store, 5).unwrap();
        let later = for_batch(&mut store, 4, now).unwrap();
        assert_eq!(
            later.directives, None,
            "a later generation moves nothing back"
        );
    }
}
