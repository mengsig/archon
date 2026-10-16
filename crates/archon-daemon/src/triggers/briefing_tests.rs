use super::*;
use crate::triggers::compact::{self, Facts, Held};

#[test]
fn store_snapshot_is_deterministic_and_frozen_until_the_next_generation() {
    let mut store = Store::open_in_memory().unwrap();
    let initial = build(&store).unwrap();
    assert_eq!(initial, build(&store).unwrap());
    let first = frozen(&mut store, 1).unwrap();
    store
        .append_feed(
            FeedEntry::Operator {
                text: "new context".into(),
            },
            archon_core::now(),
        )
        .unwrap();
    assert_eq!(first, frozen(&mut store, 1).unwrap());
    let next = frozen(&mut store, 2).unwrap();
    assert_ne!(first.events, next.events);
    assert_eq!(next.events, vec![build(&store).unwrap()]);
}

#[test]
fn an_old_briefing_is_pinned_when_a_backlog_is_compacted() {
    let store = Store::open_in_memory().unwrap();
    let briefing = build(&store).unwrap();
    let now = archon_core::now();
    let mut held = vec![Held::new(
        briefing.clone(),
        now - chrono::TimeDelta::days(1),
        false,
    )];
    for _ in 0..50 {
        held.push(Held::new(
            TriggerEvent::Budget {
                scope: "day".into(),
                spent_usd: 2.0,
                limit_usd: 1.0,
            },
            now,
            false,
        ));
    }
    let compacted = compact::compact(held, &Facts::default(), now);
    assert!(compacted.iter().any(|news| news.event == briefing));
    assert!(compacted.len() <= compact::MAX_BATCH_EVENTS + 1);
}

#[test]
fn corrupt_frozen_state_is_visible_instead_of_replaced() {
    let mut store = Store::open_in_memory().unwrap();
    store.kv_set(FROZEN_KEY, "not json").unwrap();
    assert!(matches!(frozen(&mut store, 1), Err(ArchonError::Store(_))));
    assert_eq!(
        store.kv_get(FROZEN_KEY).unwrap().as_deref(),
        Some("not json")
    );
}
