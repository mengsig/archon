//! Idempotent enqueue, the dependency graph, and the dispatch snapshot.

mod common;

use archon_core::error::ArchonError;
use archon_core::github::GithubThread;
use archon_core::id::QueueItemId;
use archon_core::protocol::ItemQuery;
use archon_core::queue::{PauseState, Priority, QueueItem, QueueState};
use archon_core::run::StageState;
use archon_queue::Store;
use archon_queue::dispatch::DependencyState;
use archon_queue::store::SpendRow;
use common::*;

fn count(store: &Store) -> usize {
    let all = ItemQuery {
        limit: Some(usize::MAX),
        ..ItemQuery::default()
    };
    store.items(&all).unwrap().len()
}

#[test]
fn item_metadata_is_committed_only_with_a_valid_item_update() {
    let (mut store, repo, _) = seeded(0);
    let original = item(Some(&repo), "original");
    store.insert_item(&original, None).unwrap();
    let mut changed = original.clone();
    changed.title = "revised".into();
    store
        .update_item_with_kv(&changed, &[("item-grant-test", "approved")])
        .unwrap();
    assert_eq!(store.item(&changed.id).unwrap().title, "revised");
    assert_eq!(
        store.kv_get("item-grant-test").unwrap().as_deref(),
        Some("approved")
    );
    changed.title.clear();
    assert!(
        store
            .update_item_with_kv(&changed, &[("item-grant-test", "revoked")])
            .is_err()
    );
    assert_eq!(store.item(&changed.id).unwrap().title, "revised");
    assert_eq!(
        store.kv_get("item-grant-test").unwrap().as_deref(),
        Some("approved")
    );
}

#[test]
fn the_same_key_and_request_returns_the_first_item() {
    let (mut store, repo, _items) = seeded(0);
    let first = item(Some(&repo), "fix");
    let stored = store.insert_item(&first, Some("enq-1")).unwrap();
    assert_eq!(stored, first);
    // A retried enqueue builds a new id and timestamps but the same request.
    let retry = QueueItem {
        id: QueueItemId::new(),
        created_at: t(9),
        updated_at: t(9),
        ..first.clone()
    };
    assert_eq!(store.insert_item(&retry, Some("enq-1")).unwrap(), first);
    // Still the first item after it moved on.
    store
        .transition_item(&first.id, QueueState::Dispatching, t(10))
        .unwrap();
    let again = store.insert_item(&retry, Some("enq-1")).unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.state, QueueState::Dispatching);
    assert_eq!(count(&store), 1);
}

fn thread() -> GithubThread {
    serde_json::from_value(serde_json::json!({"repository": "o/r", "number": 7})).unwrap()
}

#[test]
fn an_item_keeps_the_github_thread_that_asked_for_it() {
    let (mut store, repo, _items) = seeded(0);
    let first = QueueItem {
        origin: Some(thread()),
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        ..item(Some(&repo), "fix")
    };
    store.insert_item(&first, Some("gh-1")).unwrap();
    assert_eq!(store.item(&first.id).unwrap().origin, Some(thread()));
    let retry = QueueItem {
        id: QueueItemId::new(),
        ..first.clone()
    };
    assert_eq!(
        store.insert_item(&retry, Some("gh-1")).unwrap().id,
        first.id
    );
    // Without an origin the key is a different request.
    let unasked = QueueItem {
        id: QueueItemId::new(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        ..first
    };
    let err = store.insert_item(&unasked, Some("gh-1")).unwrap_err();
    assert!(matches!(err, ArchonError::Invalid(_)), "{err}");
}

/// `first` with one request field changed at a time.
fn request_variants(first: &QueueItem) -> Vec<QueueItem> {
    let base = first.clone();
    vec![
        QueueItem {
            stage_timeouts: [("implement".into(), 120)].into(),
            ..base.clone()
        },
        QueueItem {
            title: "other".into(),
            ..base.clone()
        },
        QueueItem {
            brief: "other".into(),
            ..base.clone()
        },
        QueueItem {
            priority: Priority::High,
            ..base.clone()
        },
        QueueItem {
            tags: vec![],
            ..base.clone()
        },
        QueueItem {
            budget_usd: Some(3.0),
            ..base.clone()
        },
        QueueItem {
            repo: None,
            ..base.clone()
        },
        QueueItem {
            origin: Some(thread()),
            stage_timeouts: Default::default(),
            fresh_retries: 0,
            routed_provider: None,
            provider_switch: None,
            initiative: None,
            ..base.clone()
        },
        QueueItem {
            pipeline: "ask".into(),
            ..base
        },
    ]
}

#[test]
fn a_key_reused_for_another_request_is_invalid() {
    let (mut store, repo, _items) = seeded(0);
    let first = item(Some(&repo), "fix");
    store.insert_item(&first, Some("enq-1")).unwrap();
    for variant in request_variants(&first) {
        let fresh = QueueItem {
            id: QueueItemId::new(),
            ..variant
        };
        let err = store.insert_item(&fresh, Some("enq-1")).unwrap_err();
        assert!(matches!(err, ArchonError::Invalid(_)), "{err}");
    }
    // Without a key, or with another key, the same request is a new item.
    let fresh = QueueItem {
        id: QueueItemId::new(),
        ..first.clone()
    };
    store.insert_item(&fresh, None).unwrap();
    let other = QueueItem {
        id: QueueItemId::new(),
        ..first.clone()
    };
    store.insert_item(&other, Some("enq-2")).unwrap();
    assert_eq!(count(&store), 3);
    assert!(matches!(
        store.insert_item(&first, None),
        Err(ArchonError::Invalid(_))
    ));
    assert!(matches!(
        store.insert_item(&fresh, Some(" ")),
        Err(ArchonError::Invalid(_))
    ));
}

#[test]
fn new_items_must_be_queued_in_a_known_repository() {
    let (mut store, repo, _items) = seeded(0);
    let running = QueueItem {
        state: QueueState::Dispatching,
        ..item(Some(&repo), "x")
    };
    assert!(matches!(
        store.insert_item(&running, None),
        Err(ArchonError::Invalid(_))
    ));
    let unknown = item(Some(&common::repo("ghost")), "x");
    assert!(matches!(
        store.insert_item(&unknown, None),
        Err(ArchonError::NotFound(_))
    ));
    let untitled = item(Some(&repo), " ");
    assert!(matches!(
        store.insert_item(&untitled, None),
        Err(ArchonError::Invalid(_))
    ));
    // Repository-less items (the `ask` pipeline) are fine.
    store.insert_item(&item(None, "question"), None).unwrap();
    assert_eq!(count(&store), 1);
}

#[test]
fn dependencies_must_exist_be_distinct_and_not_self() {
    let (mut store, repo, items) = seeded(1);
    let mut missing = item(Some(&repo), "m");
    missing.dependencies = vec![QueueItemId::new()];
    assert!(matches!(
        store.insert_item(&missing, None),
        Err(ArchonError::NotFound(_))
    ));
    let mut twice = item(Some(&repo), "d");
    twice.dependencies = vec![items[0].id.clone(), items[0].id.clone()];
    assert!(matches!(
        store.insert_item(&twice, None),
        Err(ArchonError::Invalid(_))
    ));
    let mut own = item(Some(&repo), "s");
    own.dependencies = vec![own.id.clone()];
    assert!(matches!(
        store.insert_item(&own, None),
        Err(ArchonError::Invalid(_))
    ));
    assert_eq!(count(&store), 1);
    assert!(store.dependents(&items[0].id).unwrap().is_empty());
}

/// a <- b <- c (c depends on b, b on a). Closing the loop from either end
/// is refused and leaves the graph as it was.
#[test]
fn updates_that_close_a_cycle_are_refused() {
    let (mut store, repo, items) = seeded(1);
    let a = items[0].clone();
    let b = store
        .insert_item(
            &QueueItem {
                dependencies: vec![a.id.clone()],
                ..item(Some(&repo), "b")
            },
            None,
        )
        .unwrap();
    let c = store
        .insert_item(
            &QueueItem {
                dependencies: vec![b.id.clone()],
                ..item(Some(&repo), "c")
            },
            None,
        )
        .unwrap();

    for (from, to) in [(&a, &c), (&a, &b), (&b, &c)] {
        let mut looped = from.clone();
        looped.dependencies.push(to.id.clone());
        let err = store.update_item(&looped).unwrap_err();
        assert!(matches!(err, ArchonError::Invalid(_)), "{err}");
        assert_eq!(store.item(&from.id).unwrap(), *from);
    }
    assert!(store.dependents(&c.id).unwrap().is_empty());
    assert_eq!(store.dependents(&a.id).unwrap(), vec![b.id.clone()]);

    // A diamond is not a cycle: d depends on b and c, both reach a.
    let d = QueueItem {
        dependencies: vec![b.id.clone(), c.id.clone()],
        ..item(Some(&repo), "d")
    };
    store.insert_item(&d, None).unwrap();
    let mut rewired = c.clone();
    rewired.dependencies = vec![a.id.clone(), b.id.clone()];
    store.update_item(&rewired).unwrap();
    assert_eq!(
        store.dependents(&a.id).unwrap(),
        vec![b.id.clone(), c.id.clone()]
    );
    assert!(matches!(
        store.dependents(&QueueItemId::new()),
        Err(ArchonError::NotFound(_))
    ));
}

/// Random DAG growth: every refused edge would have closed a cycle
/// (checked against an in-memory reachability model), every accepted one
/// keeps the graph acyclic.
#[test]
fn random_dependency_edits_keep_the_graph_acyclic() {
    let mut rng = Rng::new(99);
    let (mut store, _repo, items) = seeded(12);
    let mut deps: Vec<Vec<usize>> = vec![vec![]; items.len()];
    let reaches = |deps: &Vec<Vec<usize>>, from: usize, to: usize| -> bool {
        let mut stack = vec![from];
        let mut seen = vec![false; deps.len()];
        while let Some(n) = stack.pop() {
            if n == to {
                return true;
            }
            if !std::mem::replace(&mut seen[n], true) {
                stack.extend(&deps[n]);
            }
        }
        false
    };
    for _ in 0..300 {
        let (x, y) = (rng.below(items.len()), rng.below(items.len()));
        if x == y || deps[x].contains(&y) {
            continue;
        }
        let mut edited = store.item(&items[x].id).unwrap();
        edited.dependencies.push(items[y].id.clone());
        let cycle = reaches(&deps, y, x);
        let result = store.update_item(&edited);
        assert_eq!(result.is_err(), cycle, "{x} -> {y}: {result:?}");
        if !cycle {
            deps[x].push(y);
        }
    }
    for (x, own) in deps.iter().enumerate() {
        let stored = store.item(&items[x].id).unwrap().dependencies;
        let expected: Vec<_> = own.iter().map(|&y| items[y].id.clone()).collect();
        assert_eq!(stored, expected);
    }
}

/// Moves the three items to `succeeded`, `failed` and `cancelled`.
fn settle(store: &mut Store, ok: &QueueItemId, failed: &QueueItemId, cancelled: &QueueItemId) {
    let run = start(store, ok);
    let done = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "s".into(),
    };
    settle_run(store, &run, done, t(20));
    store
        .transition_item(failed, QueueState::Dispatching, t(20))
        .unwrap();
    let error = QueueState::Failed {
        run: None,
        error: "e".into(),
    };
    store.transition_item(failed, error, t(21)).unwrap();
    let stop = QueueState::Cancelled { reason: None };
    store.transition_item(cancelled, stop, t(21)).unwrap();
}

#[test]
fn the_snapshot_resolves_dependency_states() {
    let (mut store, repo, items) = seeded(4);
    let [ok, waiting, failed, cancelled] = [0, 1, 2, 3].map(|i| items[i].id.clone());
    settle(&mut store, &ok, &failed, &cancelled);

    let with = |deps: Vec<QueueItemId>, title: &str| QueueItem {
        dependencies: deps,
        ..item(Some(&repo), title)
    };
    let ready = store
        .insert_item(&with(vec![ok.clone()], "ready"), None)
        .unwrap();
    let wait = store
        .insert_item(&with(vec![ok.clone(), waiting.clone()], "wait"), None)
        .unwrap();
    let broken = store
        .insert_item(&with(vec![waiting.clone(), failed.clone()], "broken"), None)
        .unwrap();
    let broken2 = store
        .insert_item(&with(vec![cancelled.clone()], "broken2"), None)
        .unwrap();

    let snapshot = store.dispatch_snapshot(t(30)).unwrap();
    let state_of = |id: &QueueItemId| {
        snapshot
            .queued
            .iter()
            .find(|c| &c.item == id)
            .map(|c| c.deps)
    };
    assert_eq!(state_of(&ready.id), Some(DependencyState::AllSucceeded));
    assert_eq!(state_of(&waiting), Some(DependencyState::AllSucceeded));
    assert_eq!(state_of(&wait.id), Some(DependencyState::Waiting));
    assert_eq!(state_of(&broken.id), Some(DependencyState::Broken));
    assert_eq!(state_of(&broken2.id), Some(DependencyState::Broken));
    assert_eq!(state_of(&ok), None, "only queued items are candidates");
    assert_eq!(snapshot.queued.len(), 5);
}

/// Two live agents on the first item, one live and one finished on the
/// second, one live on a repository-less `ask` item.
fn stage_fleet(store: &mut Store, items: &[QueueItem]) {
    let ask = QueueItem {
        pipeline: "ask".into(),
        ..item(None, "q")
    };
    let ask = store.insert_item(&ask, None).unwrap();
    let run_a = start(store, &items[0].id);
    let run_b = start(store, &items[1].id);
    let run_q = start(store, &ask.id);
    store
        .upsert_stage_run(&running_stage(&run_a, 0, 1))
        .unwrap();
    store.upsert_stage_run(&stage(&run_a, 1)).unwrap();
    store
        .upsert_stage_run(&running_stage(&run_b, 0, 2))
        .unwrap();
    store
        .upsert_stage_run(&running_stage(&run_q, 0, 3))
        .unwrap();
    let done = archon_core::run::StageRun {
        state: StageState::Completed,
        finished_at: Some(t(30)),
        ..running_stage(&run_b, 1, 4)
    };
    store.upsert_stage_run(&done).unwrap();
}

#[test]
fn the_snapshot_counts_live_agents_runs_pause_and_spend() {
    let (mut store, repo, items) = seeded(3);
    let other = common::repo("beta");
    store.insert_repo(&other).unwrap();
    stage_fleet(&mut store, &items);
    let pause = PauseState {
        global: false,
        repos: vec![other.id.clone()],
        pipelines: vec!["ask".into()],
    };
    store.set_pause_state(&pause).unwrap();
    let row = SpendRow {
        agent: agent(1),
        item: Some(items[0].id.clone()),
        cost_usd: 1.0,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: 2.0,
        input_tokens: 1,
        output_tokens: 1,
        at: t(40),
    };
    store.record_spend(&row).unwrap();

    let snapshot = store.dispatch_snapshot(t(40)).unwrap();
    assert_eq!(snapshot.running_total, 4);
    assert_eq!(snapshot.running_by_repo.get(&repo.id), Some(&3));
    assert_eq!(snapshot.running_by_repo.len(), 1);
    assert_eq!(snapshot.running_by_pipeline.get("direct"), Some(&3));
    assert_eq!(snapshot.running_by_pipeline.get("ask"), Some(&1));
    assert_eq!(snapshot.active_runs, 3);
    assert_eq!(snapshot.pause, pause);
    assert_eq!(
        (snapshot.spend_today_usd, snapshot.spend_total_usd),
        (3.0, 3.0)
    );
    assert_eq!(snapshot.queued.len(), 1);
    assert_eq!(snapshot.queued[0].item, items[2].id);
    assert!(snapshot.pending_spawns.is_empty());
}

/// An item counts toward `max_active_runs` from the moment it is picked,
/// not only once its run row exists, and never twice.
#[test]
fn dispatching_items_count_as_active_runs() {
    let (mut store, _repo, items) = seeded(3);
    assert_eq!(store.dispatch_snapshot(t(1)).unwrap().active_runs, 0);
    let picked = store
        .transition_item(&items[0].id, QueueState::Dispatching, t(2))
        .unwrap();
    assert_eq!(store.dispatch_snapshot(t(2)).unwrap().active_runs, 1);
    store.insert_run(&run_for(&picked)).unwrap();
    assert_eq!(store.dispatch_snapshot(t(3)).unwrap().active_runs, 1);
    start(&mut store, &items[1].id);
    let snapshot = store.dispatch_snapshot(t(4)).unwrap();
    assert_eq!(snapshot.active_runs, 2);
    assert_eq!(snapshot.queued.len(), 1);
    store.requeue_orphaned_dispatching(t(5)).unwrap();
    assert_eq!(store.dispatch_snapshot(t(5)).unwrap().active_runs, 2);
}

#[test]
fn snapshot_candidates_come_highest_priority_then_oldest_first() {
    let (mut store, repo, _items) = seeded(0);
    let mut expected = Vec::new();
    for (title, priority) in [
        ("n1", Priority::Normal),
        ("c1", Priority::Critical),
        ("l1", Priority::Low),
        ("n2", Priority::Normal),
        ("c2", Priority::Critical),
    ] {
        let it = store
            .insert_item(
                &QueueItem {
                    priority,
                    ..item(Some(&repo), title)
                },
                None,
            )
            .unwrap();
        expected.push((priority, it.id));
    }
    let snapshot = store.dispatch_snapshot(t(0)).unwrap();
    let got: Vec<_> = snapshot.queued.iter().map(|c| c.item.clone()).collect();
    let order = [1, 4, 0, 3, 2].map(|i| expected[i].1.clone());
    assert_eq!(got, order.to_vec());
    assert!(
        snapshot
            .queued
            .windows(2)
            .all(|w| w[0].created_seq != w[1].created_seq)
    );
}

#[test]
fn timeout_overrides_are_part_of_an_idempotent_request() {
    let (mut store, repo, _) = seeded(0);
    let mut first = item(Some(&repo), "fix");
    first.stage_timeouts.insert("implement".into(), 120);
    store.insert_item(&first, Some("timeout")).unwrap();
    let mut retry = QueueItem {
        id: QueueItemId::new(),
        ..first.clone()
    };
    assert_eq!(store.insert_item(&retry, Some("timeout")).unwrap(), first);
    for minutes in [Some(180), None] {
        retry.stage_timeouts = minutes
            .map(|m| [("implement".into(), m)].into())
            .unwrap_or_default();
        assert!(matches!(
            store.insert_item(&retry, Some("timeout")),
            Err(ArchonError::Invalid(_))
        ));
    }
    assert_eq!(count(&store), 1);
}
