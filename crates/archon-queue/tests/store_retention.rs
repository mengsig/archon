//! The history retention pass, the bounded WAL and the store's file modes
//! (ARCHITECTURE §16).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::{NotificationId, QuestionId};
use archon_core::notify::{FeedEntry, Notification, NotificationKind, Question};
use archon_core::queue::{QueueItem, QueueState};
use archon_queue::store::stacks::StackParent;
use archon_queue::store::{SpendRow, WAL_SIZE_LIMIT};
use archon_queue::{Checkpointer, DataLock, Store};
use common::*;

const DAY: i64 = 86_400;

/// Every store row is older than this but what a test makes newer.
fn cutoff() -> archon_core::Timestamp {
    t(30 * DAY)
}

/// An item whose one run was charged `usd` by agent `n` at `charged`, and
/// that settled at `settled`, with its outcome recorded.
fn settled_item(store: &mut Store, n: u32, usd: f64, charged: i64, settled: i64) -> QueueItem {
    let new = item(None, &format!("item {n}"));
    settle(store, &new, n, usd, charged, settled)
}

fn settle(
    store: &mut Store,
    new: &QueueItem,
    n: u32,
    usd: f64,
    charged: i64,
    settled: i64,
) -> QueueItem {
    let item = store.insert_item(new, None).unwrap();
    let run = start(store, &item.id);
    store.upsert_stage_run(&running_stage(&run, 0, n)).unwrap();
    let row = SpendRow {
        agent: agent(n),
        item: Some(item.id.clone()),
        cost_usd: usd,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: usd / 2.0,
        input_tokens: 1,
        output_tokens: 1,
        at: t(charged),
    };
    store.record_spend(&row).unwrap();
    let done = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "done".into(),
    };
    let item = settle_run(store, &run, done, t(settled));
    let finished = store.run(&run.id).unwrap();
    store.capture_outcome(&finished, None, true).unwrap();
    item
}

fn notification(kind: NotificationKind) -> FeedEntry {
    FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind,
        text: "note".into(),
        item: None,
        run: None,
        at: t(10),
    })
}

fn gone(found: Result<impl std::fmt::Debug, ArchonError>) -> bool {
    matches!(found, Err(ArchonError::NotFound(_)))
}

#[test]
fn old_settled_items_go_with_their_history_and_learning_and_budgets_stay() {
    let mut store = store();
    let old = settled_item(&mut store, 1, 2.0, 30, 50);
    let recent = settled_item(&mut store, 2, 1.0, 31 * DAY, 31 * DAY);
    let needed = settled_item(&mut store, 3, 0.5, 30, 60);
    let mut waiting = item(None, "waiting");
    waiting.dependencies = vec![needed.id.clone()];
    store.insert_item(&waiting, None).unwrap();
    let old_run = store.runs_for_item(&old.id).unwrap().remove(0);
    store.kv_set(&format!("steers:{}", old.id), "[]").unwrap();
    store.kv_set(&format!("shape:{}", old.id), "[]").unwrap();
    store
        .kv_set(&format!("steers:{}", recent.id), "[]")
        .unwrap();
    store
        .append_feed(notification(NotificationKind::Progress), t(10))
        .unwrap();
    store
        .append_feed(notification(NotificationKind::Outcome), t(10))
        .unwrap();
    let totals = store.spend_totals(t(40 * DAY), true).unwrap();
    let learned = store.outcome(&old_run.id).unwrap();
    assert!(learned.is_some(), "the old run's outcome is recorded");

    let pruned = store.prune_history(cutoff(), 100, |_, _| false).unwrap();
    assert_eq!((pruned.items, pruned.runs, pruned.stage_runs), (1, 1, 1));
    assert_eq!((pruned.spend, pruned.feed, pruned.kv), (1, 1, 2));
    assert!(gone(store.item(&old.id)), "the old settled item goes");
    assert!(gone(store.run(&old_run.id)), "with its run");
    assert!(
        store
            .kv_get(&format!("steers:{}", old.id))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .kv_get(&format!("shape:{}", old.id))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.outcome(&old_run.id).unwrap(),
        learned,
        "learning stays"
    );
    assert_eq!(store.outcomes(None).unwrap().len(), 3);
    store.item(&recent.id).expect("a recent item stays");
    store
        .kv_get(&format!("steers:{}", recent.id))
        .unwrap()
        .expect("with its kv");
    store
        .item(&needed.id)
        .expect("an item a live one depends on stays");
    assert_eq!(
        store.feed_after(0, 10).unwrap().len(),
        1,
        "the shown entry stays"
    );
    assert_eq!(
        store.spend_totals(t(40 * DAY), true).unwrap(),
        totals,
        "the total budget still counts pruned spend"
    );
    assert_eq!(
        store
            .prune_history(cutoff(), 100, |_, _| false)
            .unwrap()
            .total(),
        0
    );
}

#[test]
fn spend_of_a_day_budgets_still_count_keeps_its_item() {
    let mut store = store();
    let late = settled_item(&mut store, 1, 2.0, 30 * DAY - 60, 50);
    let asked = settled_item(&mut store, 2, 1.0, 30, 50);
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text: "keep it?".into(),
        options: vec![],
        item: Some(asked.id.clone()),
        aether: None,
        asked_by: None,
        asked_at: t(70),
        answer: None,
        answered_at: None,
    };
    store.insert_question(&question).unwrap();
    let today = store.spend_totals(cutoff(), false).unwrap().0;
    assert!(today > 0.0, "the late charge falls on the cutoff's day");

    assert_eq!(
        store
            .prune_history(cutoff(), 100, |_, _| false)
            .unwrap()
            .items,
        0
    );
    store
        .item(&late.id)
        .expect("a charge on the cutoff's day keeps its item");
    store
        .item(&asked.id)
        .expect("an open question keeps its item");
    assert_eq!(store.spend_totals(cutoff(), false).unwrap().0, today);
}

#[test]
fn a_pass_prunes_a_bounded_batch_newest_first() {
    let mut store = store();
    let items: Vec<QueueItem> = (1..=3)
        .map(|n| settled_item(&mut store, n, 1.0, 30, 40 + i64::from(n)))
        .collect();
    assert_eq!(
        store
            .prune_history(cutoff(), 2, |_, _| false)
            .unwrap()
            .items,
        2
    );
    assert!(gone(store.item(&items[1].id)) && gone(store.item(&items[2].id)));
    store
        .item(&items[0].id)
        .expect("the oldest waits for the next batch");
    assert_eq!(
        store
            .prune_history(cutoff(), 2, |_, _| false)
            .unwrap()
            .items,
        1
    );
    assert!(gone(store.item(&items[0].id)));
}

#[test]
fn a_dependency_chain_longer_than_a_batch_goes_over_passes() {
    let mut store = store();
    let mut chain: Vec<QueueItem> = Vec::new();
    for n in 1..=7 {
        let mut new = item(None, &format!("link {n}"));
        new.dependencies = chain
            .last()
            .map(|prev| prev.id.clone())
            .into_iter()
            .collect();
        chain.push(settle(&mut store, &new, n, 1.0, 30, 40 + i64::from(n)));
    }

    for (pass, expected) in [3, 3, 1].into_iter().enumerate() {
        let pruned = store.prune_history(cutoff(), 3, |_, _| false).unwrap();
        assert_eq!(pruned.items, expected, "pass {pass} makes progress");
        let (left, went) = chain.split_at(7 - [3, 6, 7][pass]);
        assert!(left.iter().all(|i| store.item(&i.id).is_ok()));
        assert!(went.iter().all(|i| gone(store.item(&i.id))), "newest first");
    }
    assert!(chain.iter().all(|i| gone(store.item(&i.id))));
}

#[test]
fn a_cycle_through_a_stack_and_an_edited_dependency_goes_whole() {
    let (mut store, repo, _) = seeded(0);
    let a = store.insert_item(&item(Some(&repo), "A"), None).unwrap();
    let mut new_b = item(Some(&repo), "B");
    new_b.dependencies = vec![a.id.clone()];
    let mut b = store.insert_item(&new_b, None).unwrap();
    store
        .save_stack_parent(&StackParent {
            item: b.id.clone(),
            parent_item: a.id.clone(),
            parent_branch: "archon/a".into(),
            parent_sha: "a".repeat(40),
            position: 0,
            repository_base: None,
            merged: true,
            closed: false,
            conflict: None,
            needs_attention: None,
        })
        .unwrap();
    b.dependencies.clear();
    store.update_item(&b).unwrap();
    let mut a = store.item(&a.id).unwrap();
    a.dependencies = vec![b.id.clone()];
    store.update_item(&a).unwrap();
    for id in [&a.id, &b.id] {
        let cancelled = QueueState::Cancelled {
            reason: Some("done".into()),
        };
        store.transition_item(id, cancelled, t(20)).unwrap();
    }

    let pruned = store.prune_history(cutoff(), 1, |_, _| false).unwrap();
    assert_eq!(pruned.items, 0, "a cycle goes whole or waits for room");
    assert!(store.item(&a.id).is_ok() && store.item(&b.id).is_ok());
    let pruned = store.prune_history(cutoff(), 2, |_, _| false).unwrap();
    assert_eq!(pruned.items, 2);
    assert!(gone(store.item(&a.id)) && gone(store.item(&b.id)));
}

#[test]
fn an_item_gc_still_needs_stays_until_gc_is_done_with_it() {
    let mut store = store();
    let worktree = settled_item(&mut store, 1, 1.0, 30, 40);
    let reported = settled_item(&mut store, 2, 1.0, 30, 50);
    let report = format!(r#"{{"html":"/r/a.html","item":"{}"}}"#, reported.id);
    store.kv_set("report:rpt_1", &report).unwrap();
    let on_disk = |_: &str, paths: &[PathBuf]| paths.iter().any(|p| p == Path::new("/wt/alpha/x"));

    assert_eq!(store.prune_history(cutoff(), 10, on_disk).unwrap().items, 0);
    store
        .item(&worktree.id)
        .expect("gc finds a worktree through its item");
    assert_eq!(
        store
            .prune_history(cutoff(), 10, |_, _| false)
            .unwrap()
            .items,
        1
    );
    assert!(
        gone(store.item(&worktree.id)),
        "once gc removed its worktree"
    );
    store
        .item(&reported.id)
        .expect("report expiry finds a snapshot through its item");

    store.kv_delete("report:rpt_1").unwrap();
    store
        .kv_set("report:rpt_2", r#"{"html":"/r/b.html","item":null}"#)
        .unwrap();
    let pruned = store.prune_history(cutoff(), 10, |_, _| false).unwrap();
    assert_eq!(pruned.items, 1, "a report of no item keeps none");
    assert!(gone(store.item(&reported.id)));
}

#[test]
fn a_pruned_outcome_keeps_the_cost_settled_after_it_was_captured() {
    let mut store = store();
    let settled = settled_item(&mut store, 1, 2.0, 30, 50);
    let run = store.runs_for_item(&settled.id).unwrap().remove(0);
    let captured = store.outcome(&run.id).unwrap().unwrap().cost_usd;
    let late = SpendRow {
        agent: agent(1),
        item: Some(settled.id.clone()),
        cost_usd: 3.0,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: Some(true),
        estimated_cost_usd: 1.0,
        input_tokens: 2,
        output_tokens: 2,
        at: t(60),
    };
    store.record_spend(&late).unwrap();
    let learned = store.outcome(&run.id).unwrap().unwrap();
    assert!(learned.cost_usd > captured, "the late settlement counts");
    let stats = store.learning_stats(None).unwrap();

    assert_eq!(
        store
            .prune_history(cutoff(), 10, |_, _| false)
            .unwrap()
            .runs,
        1
    );
    let kept = store.outcome(&run.id).unwrap().unwrap();
    assert_eq!(kept.cost_usd, learned.cost_usd);
    assert_eq!(kept.stages, learned.stages);
    assert_eq!(kept.cost_provisional, learned.cost_provisional);
    assert_eq!(store.learning_stats(None).unwrap(), stats);
}

#[test]
fn a_distillation_stays_until_its_lessons_are_applied() {
    let mut store = store();
    let mut new = item(None, "distill");
    new.tags.push("archon:distillation".into());
    let distilled = settle(&mut store, &new, 1, 1.0, 30, 40);
    let run = store.runs_for_item(&distilled.id).unwrap().remove(0);

    assert_eq!(
        store
            .prune_history(cutoff(), 10, |_, _| false)
            .unwrap()
            .items,
        0
    );
    let applied = format!("learning/applied/{}", run.id);
    store.kv_set(&applied, "1").unwrap();
    assert_eq!(
        store
            .prune_history(cutoff(), 10, |_, _| false)
            .unwrap()
            .items,
        1
    );
    assert!(
        store.kv_get(&applied).unwrap().is_none(),
        "its mark goes with it"
    );
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

#[test]
fn a_fresh_store_is_private_whatever_the_umask() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let path = data.join("archon.db");
    let mut store = Store::open(&path).unwrap();
    store.kv_set("written", "1").unwrap();
    assert_eq!(mode(&data), 0o700);
    for name in ["archon.db", "archon.db-wal", "archon.db-shm"] {
        assert_eq!(mode(&data.join(name)), 0o600, "{name}");
    }
    assert!(
        store.vacuum_freed().unwrap(),
        "a new store vacuums incrementally"
    );
}

#[test]
fn the_wal_is_cut_back_after_many_writes_and_at_restart() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().canonicalize().unwrap();
    let path = data.join("archon.db");
    let wal = data.join("archon.db-wal");
    let socket = Path::new("/run/a/archond.sock");
    let lock = DataLock::acquire(&data, socket).unwrap();
    let mut store = Store::open_owned(&path, lock).unwrap();
    for n in 0..650 {
        let queued = store
            .insert_item(&item(None, &format!("item {n}")), None)
            .unwrap();
        let cancelled = QueueState::Cancelled {
            reason: Some("test".into()),
        };
        store.transition_item(&queued.id, cancelled, t(2)).unwrap();
    }
    assert!(
        file_size(&wal) > WAL_SIZE_LIMIT as u64,
        "the writer defers checkpoints"
    );
    let checkpointer = Checkpointer::open(&path).unwrap();
    let (log, done) = checkpointer.checkpoint().unwrap();
    assert_eq!(log, done, "no reader holds the WAL");
    store.kv_set("after", "1").unwrap();
    assert!(
        file_size(&wal) <= WAL_SIZE_LIMIT as u64,
        "a reset WAL is cut back"
    );

    store.kv_set("more", "1").unwrap();
    assert!(checkpointer.truncate().unwrap());
    assert_eq!(file_size(&wal), 0, "a truncating checkpoint empties it");

    // An idle connection left open keeps an oversized WAL across the restart.
    let blob = "x".repeat(2 * WAL_SIZE_LIMIT as usize);
    store.kv_set("blob", &blob).unwrap();
    let idle = rusqlite::Connection::open(&path).unwrap();
    let keys: i64 = idle
        .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
        .unwrap();
    assert!(keys > 0, "the idle connection has read the store");
    drop(checkpointer);
    drop(store);
    assert!(
        file_size(&wal) > WAL_SIZE_LIMIT as u64,
        "the WAL outlives archond"
    );
    let lock = DataLock::acquire(&data, socket).unwrap();
    let store = Store::open_owned(&path, lock).unwrap();
    assert_eq!(file_size(&wal), 0, "opening the store truncates it");
    assert_eq!(
        store.kv_get("blob").unwrap().map(|v| v.len()),
        Some(blob.len())
    );
    drop(idle);
}
