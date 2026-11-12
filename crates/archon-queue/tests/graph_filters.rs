//! The `list_items` history filters of protocol 24 (feature `item_filters`):
//! a window on `updated_at` and a text search of the title and brief.

mod common;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::ItemQuery;
use archon_core::queue::QueueItem;
use archon_queue::Store;
use common::*;

fn listed(store: &Store, query: ItemQuery) -> Vec<QueueItemId> {
    let query = ItemQuery {
        limit: Some(100),
        ..query
    };
    let mut ids: Vec<_> = store
        .items(&query)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    ids.sort();
    ids
}

fn sorted(items: &[&QueueItem]) -> Vec<QueueItemId> {
    let mut ids: Vec<_> = items.iter().map(|i| i.id.clone()).collect();
    ids.sort();
    ids
}

fn insert(
    store: &mut Store,
    repo: &archon_core::repo::Repo,
    title: &str,
    brief: &str,
    at: i64,
) -> QueueItem {
    let mut new = item(Some(repo), title);
    new.brief = brief.into();
    new.created_at = t(at);
    new.updated_at = t(at);
    store.insert_item(&new, None).unwrap()
}

#[test]
fn updated_since_and_before_bound_the_window() {
    let (mut store, repo, _) = seeded(0);
    let old = insert(&mut store, &repo, "old", "b", 10);
    let middle = insert(&mut store, &repo, "middle", "b", 20);
    let new = insert(&mut store, &repo, "new", "b", 30);
    let since = ItemQuery {
        updated_since: Some(t(20)),
        ..ItemQuery::default()
    };
    assert_eq!(
        listed(&store, since),
        sorted(&[&middle, &new]),
        "at or after"
    );
    let before = ItemQuery {
        before: Some(t(20)),
        ..ItemQuery::default()
    };
    assert_eq!(listed(&store, before), sorted(&[&old]), "strictly before");
    let window = ItemQuery {
        updated_since: Some(t(15)),
        before: Some(t(30)),
        ..ItemQuery::default()
    };
    assert_eq!(listed(&store, window), sorted(&[&middle]));
    let empty = ItemQuery {
        updated_since: Some(t(20)),
        before: Some(t(20)),
        ..ItemQuery::default()
    };
    assert!(matches!(store.items(&empty), Err(ArchonError::Invalid(_))));
}

#[test]
fn text_matches_a_literal_substring_of_the_title_or_brief_in_any_ascii_case() {
    let (mut store, repo, _) = seeded(0);
    let titled = insert(&mut store, &repo, "Fix the Parser", "tidy", 10);
    let briefed = insert(
        &mut store,
        &repo,
        "other",
        "the parser crashes on 100%_ input",
        11,
    );
    let neither = insert(&mut store, &repo, "unrelated", "nothing here", 12);
    let text = |text: &str| ItemQuery {
        text: Some(text.into()),
        ..ItemQuery::default()
    };
    assert_eq!(listed(&store, text("PARSER")), sorted(&[&titled, &briefed]));
    assert_eq!(listed(&store, text("100%_")), sorted(&[&briefed]));
    assert!(
        listed(&store, text("100_%")).is_empty(),
        "% and _ are literal"
    );
    assert!(listed(&store, text("x%")).is_empty());
    assert_eq!(listed(&store, text("nothing")), sorted(&[&neither]));
    let both = ItemQuery {
        text: Some("parser".into()),
        updated_since: Some(t(11)),
        ..ItemQuery::default()
    };
    assert_eq!(listed(&store, both), sorted(&[&briefed]), "filters combine");
}

#[test]
fn text_is_one_to_two_hundred_characters() {
    let (mut store, repo, _) = seeded(0);
    let long_title = "é".repeat(200);
    let unicode = insert(&mut store, &repo, &long_title, "b", 10);
    let query = |text: String| ItemQuery {
        text: Some(text),
        ..ItemQuery::default()
    };
    assert_eq!(
        listed(&store, query(long_title.clone())),
        std::slice::from_ref(&unicode.id),
        "200 characters, not bytes"
    );
    for bad in [String::new(), "a".repeat(201)] {
        let refused = store.items(&query(bad)).unwrap_err();
        assert!(matches!(&refused, ArchonError::Invalid(m) if m.contains("1 to 200")));
    }
}
