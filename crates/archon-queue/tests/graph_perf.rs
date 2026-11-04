//! A 2,000-item task graph is read in under 100 ms (WORKSTREAMS.md AR-G).
//! Timed on a file store, as archond reads it, with the best of a few reads
//! kept; the bound is generous so a loaded CI host or a debug build passes.

mod common;

use std::time::{Duration, Instant};

use archon_core::protocol::TaskFilter;
use archon_core::queue::QueueItem;
use archon_queue::Store;
use common::*;

const ITEMS: usize = 2000;
/// The acceptance bound, for an optimized build.
const RELEASE_BOUND: Duration = Duration::from_millis(100);
/// Unoptimized builds and busy CI hosts run several times slower.
const DEBUG_BOUND: Duration = Duration::from_millis(1000);

/// `ITEMS` active items: chains of dependencies, a share of them running
/// with a live stage agent and spend, all in one transaction.
fn fill(store: &mut Store, repo: &archon_core::repo::Repo) {
    let mut previous: Option<QueueItem> = None;
    for n in 0..ITEMS {
        let mut new = item(Some(repo), &format!("item {n}"));
        new.brief = format!("brief {n} ").repeat(40);
        new.created_at = t(n as i64);
        new.updated_at = new.created_at;
        if n % 4 != 0
            && let Some(previous) = &previous
        {
            new.dependencies = vec![previous.id.clone()];
        }
        let new = store.insert_item(&new, None).unwrap();
        if n % 10 == 0 {
            let run = start(store, &new.id);
            store
                .upsert_stage_run(&running_stage(&run, 0, n as u32))
                .unwrap();
        }
        previous = Some(new);
    }
}

#[test]
fn a_two_thousand_item_graph_is_read_well_within_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let mut writer = Store::open(&path).unwrap();
    let repo = repo("alpha");
    writer.insert_repo(&repo).unwrap();
    fill(&mut writer, &repo);
    let reader = writer.open_reader(&path).unwrap();

    let mut best = Duration::MAX;
    for _ in 0..5 {
        let began = Instant::now();
        let graph = reader
            .task_graph(&TaskFilter::default(), Some(2000), true)
            .unwrap();
        best = best.min(began.elapsed());
        assert_eq!(graph.nodes.len(), ITEMS);
        assert!(graph.edges.len() >= ITEMS / 2, "chains of dependencies");
        assert!(!graph.truncated);
    }
    let bound = if cfg!(debug_assertions) {
        DEBUG_BOUND
    } else {
        RELEASE_BOUND
    };
    eprintln!("a {ITEMS}-item task graph read in {best:?} (bound {bound:?})");
    assert!(best < bound, "{best:?} is over {bound:?}");
}
