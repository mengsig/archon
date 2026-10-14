//! State machine enforcement: illegal changes are refused and write nothing.

mod common;

use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{RunState, StageState};
use archon_queue::Store;
use common::*;

fn all_states(run: &PipelineRunId) -> Vec<QueueState> {
    vec![
        QueueState::Queued,
        QueueState::Dispatching,
        QueueState::Running { run: run.clone() },
        QueueState::Blocked {
            reason: "dep failed".into(),
        },
        QueueState::Succeeded {
            run: run.clone(),
            summary: "ok".into(),
        },
        QueueState::Failed {
            run: Some(run.clone()),
            error: "boom".into(),
        },
        QueueState::Failed {
            run: None,
            error: "worktree".into(),
        },
        QueueState::Cancelled {
            reason: Some("stop".into()),
        },
    ]
}

/// Drives one item into `target` along legal edges, with a finished run so
/// that re-queueing stays legal afterwards.
fn item_in_state(
    store: &mut Store,
    repo: &archon_core::repo::Repo,
    target: &str,
) -> (QueueItem, PipelineRunId) {
    let item = store.insert_item(&item(Some(repo), target), None).unwrap();
    let run = start(store, &item.id);
    let failed = QueueState::Failed {
        run: Some(run.id.clone()),
        error: "x".into(),
    };
    settle_run(store, &run, failed, t(8));
    store
        .transition_item(&item.id, QueueState::Queued, t(9))
        .unwrap();
    let path: &[QueueState] = match target {
        "queued" => &[],
        "dispatching" => &[QueueState::Dispatching],
        "blocked" => &[QueueState::Blocked { reason: "r".into() }],
        "failed" => &[
            QueueState::Dispatching,
            QueueState::Failed {
                run: None,
                error: "e".into(),
            },
        ],
        "cancelled" => &[QueueState::Cancelled { reason: None }],
        other => panic!("no path to {other}"),
    };
    for next in path {
        store
            .transition_item(&item.id, next.clone(), t(10))
            .unwrap();
    }
    (store.item(&item.id).unwrap(), run.id)
}

#[test]
fn every_illegal_edge_is_refused_with_nothing_written() {
    let mut store = store();
    let repo = repo("alpha");
    store.insert_repo(&repo).unwrap();
    for from in ["queued", "dispatching", "blocked", "failed", "cancelled"] {
        let (before, run) = item_in_state(&mut store, &repo, from);
        for next in all_states(&run) {
            if before.state.can_transition_to(&next) {
                continue;
            }
            let err = store
                .transition_item(&before.id, next.clone(), t(99))
                .unwrap_err();
            assert!(
                matches!(err, ArchonError::InvalidTransition(_)),
                "{from} -> {}: {err}",
                next.kind()
            );
            assert_eq!(
                store.item(&before.id).unwrap(),
                before,
                "{from} -> {}",
                next.kind()
            );
        }
    }
}

#[test]
fn succeeded_can_only_be_requeued_explicitly() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let done = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "ok".into(),
    };
    let after = settle_run(&mut store, &run, done, t(20));
    for next in all_states(&run.id)
        .into_iter()
        .filter(|state| *state != QueueState::Queued)
    {
        let err = store
            .transition_item(&items[0].id, next, t(21))
            .unwrap_err();
        assert!(matches!(err, ArchonError::InvalidTransition(_)), "{err}");
    }
    assert_eq!(store.item(&items[0].id).unwrap(), after);
    let retried = store
        .transition_item(&items[0].id, QueueState::Queued, t(22))
        .unwrap();
    assert_eq!(retried.state, QueueState::Queued);
    assert!(store.run(&run.id).unwrap().state.is_terminal());
}

#[test]
fn states_naming_a_run_must_name_one_of_the_items_runs() {
    let (mut store, _repo, items) = seeded(2);
    let other_run = start(&mut store, &items[1].id);
    store
        .transition_item(&items[0].id, QueueState::Dispatching, t(2))
        .unwrap();
    let before = store.item(&items[0].id).unwrap();
    for next in [
        QueueState::Running {
            run: other_run.id.clone(),
        },
        QueueState::Running {
            run: PipelineRunId::new(),
        },
        QueueState::Failed {
            run: Some(PipelineRunId::new()),
            error: "e".into(),
        },
    ] {
        let err = store.transition_item(&items[0].id, next, t(3)).unwrap_err();
        assert!(matches!(err, ArchonError::Invalid(_)), "{err}");
        assert_eq!(store.item(&items[0].id).unwrap(), before);
    }
}

#[test]
fn an_item_with_a_live_run_settles_only_together_with_it() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let before = store.item(&items[0].id).unwrap();
    let cancelled = QueueState::Cancelled {
        reason: Some("operator".into()),
    };
    for next in all_states(&run.id)
        .into_iter()
        .filter(QueueState::is_terminal)
    {
        let err = store
            .transition_item(&items[0].id, next, t(20))
            .unwrap_err();
        assert!(matches!(err, ArchonError::InvalidTransition(_)), "{err}");
    }
    assert_eq!(store.item(&before.id).unwrap(), before);
    assert_eq!(store.active_runs().unwrap(), vec![run.clone()]);

    let settled = settle_run(&mut store, &run, cancelled, t(22));
    assert_eq!(settled.state.kind(), "cancelled");
    assert!(store.active_runs().unwrap().is_empty());
    let requeued = store
        .transition_item(&items[0].id, QueueState::Queued, t(23))
        .unwrap();
    assert_eq!(requeued.state, QueueState::Queued);
    assert_eq!(requeued.updated_at, t(23));
}

/// A live run never finishes on its own: otherwise a crash before the item
/// settles would leave the item `running` with nothing left to drive it.
#[test]
fn a_live_run_finishes_only_through_finish_run() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let finished = archon_core::run::PipelineRun {
        state: RunState::Succeeded {
            summary: "s".into(),
        },
        finished_at: Some(t(30)),
        ..run.clone()
    };
    let err = store.update_run(&finished).unwrap_err();
    assert!(matches!(err, ArchonError::InvalidTransition(_)), "{err}");
    assert_eq!(store.active_runs().unwrap(), vec![run.clone()]);

    let done = QueueState::Succeeded {
        run: run.id.clone(),
        summary: "s".into(),
    };
    let item = store.finish_run(&finished, done, t(31)).unwrap().item;
    assert_eq!(item.state.kind(), "succeeded");
    assert_eq!(item.updated_at, t(31));
    assert_eq!(store.run(&run.id).unwrap(), finished);
    assert!(store.active_runs().unwrap().is_empty());
    let again = QueueState::Failed {
        run: Some(run.id.clone()),
        error: "e".into(),
    };
    let err = store.finish_run(&finished, again, t(32)).unwrap_err();
    assert!(matches!(err, ArchonError::InvalidTransition(_)), "{err}");
    assert_eq!(store.item(&items[0].id).unwrap(), item);
}

/// A refused finish writes neither the run nor the item.
#[test]
fn a_refused_finish_writes_nothing() {
    let (mut store, _repo, items) = seeded(2);
    let other = start(&mut store, &items[1].id);
    let dispatching = store
        .transition_item(&items[0].id, QueueState::Dispatching, t(2))
        .unwrap();
    let run = run_for(&dispatching);
    store.insert_run(&run).unwrap();
    let finished = archon_core::run::PipelineRun {
        state: RunState::Succeeded {
            summary: "s".into(),
        },
        finished_at: Some(t(3)),
        ..run.clone()
    };
    let refused = [
        // dispatching -> succeeded is not an edge.
        QueueState::Succeeded {
            run: run.id.clone(),
            summary: "s".into(),
        },
        QueueState::Failed {
            run: Some(other.id.clone()),
            error: "e".into(),
        },
        QueueState::Queued,
    ];
    for next in refused {
        assert!(store.finish_run(&finished, next, t(4)).is_err());
        assert_eq!(store.item(&items[0].id).unwrap(), dispatching);
        assert_eq!(store.run(&run.id).unwrap(), run);
    }
    let live = QueueState::Failed {
        run: Some(run.id.clone()),
        error: "e".into(),
    };
    assert!(matches!(
        store.finish_run(&run, live.clone(), t(4)),
        Err(ArchonError::Invalid(_))
    ));
    let failed = store.finish_run(&finished, live, t(4)).unwrap().item;
    assert_eq!(failed.state.kind(), "failed");
    assert_eq!(store.active_runs().unwrap(), vec![other]);
}

#[test]
fn a_finished_run_stays_finished() {
    let (mut store, _repo, items) = seeded(1);
    let mut run = start(&mut store, &items[0].id);
    run.state = RunState::Running {
        stage_id: "review".into(),
        attempt: 2,
    };
    store.update_run(&run).unwrap();
    let mut no_end = run.clone();
    no_end.state = RunState::Succeeded {
        summary: "s".into(),
    };
    assert!(matches!(
        store.update_run(&no_end),
        Err(ArchonError::Invalid(_))
    ));

    run.state = RunState::Failed {
        stage_id: "review".into(),
        error: "e".into(),
    };
    run.finished_at = Some(t(30));
    let failed = QueueState::Failed {
        run: Some(run.id.clone()),
        error: "e".into(),
    };
    store.finish_run(&run, failed, t(30)).unwrap();
    let mut revived = run.clone();
    revived.state = RunState::Running {
        stage_id: "review".into(),
        attempt: 3,
    };
    revived.finished_at = None;
    assert!(matches!(
        store.update_run(&revived),
        Err(ArchonError::InvalidTransition(_))
    ));
    let mut flipped = run.clone();
    flipped.state = RunState::Succeeded {
        summary: "s".into(),
    };
    assert!(matches!(
        store.update_run(&flipped),
        Err(ArchonError::InvalidTransition(_))
    ));
    let mut moved = run.clone();
    moved.item = items[0].id.clone();
    moved.pipeline = "other".into();
    assert!(matches!(
        store.update_run(&moved),
        Err(ArchonError::Invalid(_))
    ));
    // Same terminal state, new cost: allowed.
    run.cost_usd = 1.5;
    store.update_run(&run).unwrap();
    assert_eq!(store.run(&run.id).unwrap(), run);
}

#[test]
fn stage_runs_only_move_forward() {
    let (mut store, _repo, items) = seeded(1);
    let run = start(&mut store, &items[0].id);
    let spawning = stage(&run, 0);
    store.upsert_stage_run(&spawning).unwrap();
    let no_agent = archon_core::run::StageRun {
        state: StageState::Running,
        ..spawning.clone()
    };
    assert!(matches!(
        store.upsert_stage_run(&no_agent),
        Err(ArchonError::Invalid(_))
    ));

    let mut running = running_stage(&run, 0, 1);
    running.id = spawning.id.clone();
    store.upsert_stage_run(&running).unwrap();
    let back = archon_core::run::StageRun {
        state: StageState::Spawning,
        ..running.clone()
    };
    assert!(matches!(
        store.upsert_stage_run(&back),
        Err(ArchonError::InvalidTransition(_))
    ));
    let swapped = archon_core::run::StageRun {
        aether_agent: Some(agent(2)),
        ..running.clone()
    };
    assert!(matches!(
        store.upsert_stage_run(&swapped),
        Err(ArchonError::Invalid(_))
    ));
    let reslot = archon_core::run::StageRun {
        fan_index: 3,
        ..running.clone()
    };
    assert!(matches!(
        store.upsert_stage_run(&reslot),
        Err(ArchonError::Invalid(_))
    ));

    let failed = archon_core::run::StageRun {
        state: StageState::Failed { error: "e".into() },
        finished_at: Some(t(30)),
        ..running.clone()
    };
    store.upsert_stage_run(&failed).unwrap();
    let completed = archon_core::run::StageRun {
        state: StageState::Completed,
        ..failed.clone()
    };
    assert!(matches!(
        store.upsert_stage_run(&completed),
        Err(ArchonError::InvalidTransition(_))
    ));
    assert_eq!(store.stage_run(&spawning.id).unwrap(), failed);
    // Replaying the terminal write is idempotent.
    store.upsert_stage_run(&failed).unwrap();
}

#[test]
fn an_agent_belongs_to_one_stage_run_and_finished_runs_take_no_new_stages() {
    let (mut store, _repo, items) = seeded(1);
    let mut run = start(&mut store, &items[0].id);
    store.upsert_stage_run(&running_stage(&run, 0, 1)).unwrap();
    let dup = running_stage(&run, 1, 1);
    assert!(matches!(
        store.upsert_stage_run(&dup),
        Err(ArchonError::Invalid(_))
    ));
    let zero = archon_core::run::StageRun {
        attempt: 0,
        ..stage(&run, 2)
    };
    assert!(matches!(
        store.upsert_stage_run(&zero),
        Err(ArchonError::Invalid(_))
    ));

    run.state = RunState::Cancelled { reason: None };
    run.finished_at = Some(t(30));
    let cancelled = QueueState::Cancelled { reason: None };
    store.finish_run(&run, cancelled, t(30)).unwrap();
    let late = stage(&run, 1);
    assert!(matches!(
        store.upsert_stage_run(&late),
        Err(ArchonError::InvalidTransition(_))
    ));
    assert_eq!(store.stage_runs(&run.id).unwrap().len(), 1);
    let orphan = stage(&run_for(&items[0]), 0);
    assert!(matches!(
        store.upsert_stage_run(&orphan),
        Err(ArchonError::NotFound(_))
    ));
}

#[test]
fn recovery_requeues_only_dispatching_items_without_a_live_run() {
    let (mut store, _repo, items) = seeded(4);
    store
        .transition_item(&items[0].id, QueueState::Dispatching, t(2))
        .unwrap();
    let with_run = store
        .transition_item(&items[1].id, QueueState::Dispatching, t(2))
        .unwrap();
    store.insert_run(&run_for(&with_run)).unwrap();
    start(&mut store, &items[2].id);

    let requeued = store.requeue_orphaned_dispatching(t(50)).unwrap();
    assert_eq!(requeued, vec![items[0].id.clone()]);
    let recovered = store.item(&items[0].id).unwrap();
    assert_eq!(recovered.state, QueueState::Queued);
    assert_eq!(recovered.updated_at, t(50));
    assert_eq!(
        store.item(&items[1].id).unwrap().state,
        QueueState::Dispatching
    );
    assert_eq!(store.item(&items[2].id).unwrap().state.kind(), "running");
    assert_eq!(store.item(&items[3].id).unwrap().state, QueueState::Queued);
    assert!(
        store
            .requeue_orphaned_dispatching(t(51))
            .unwrap()
            .is_empty()
    );
}

/// What the store must do with a transition the domain model allows: run
/// references must resolve, and re-queueing or settling waits for the live
/// run (which `finish_run` settles together with the item).
fn store_accepts(next: &QueueState, live: Option<&PipelineRunId>, known: bool) -> bool {
    match next {
        QueueState::Running { run } => live == Some(run),
        _ if next.is_terminal() && live.is_some() => false,
        QueueState::Succeeded { .. } | QueueState::Failed { run: Some(_), .. } => known,
        QueueState::Queued => live.is_none(),
        _ => true,
    }
}

/// Finishes the item's live run with a random terminal item state, as the
/// executor does; the store accepts exactly the legal edges.
fn finish_step(
    store: &mut Store,
    rng: &mut Rng,
    model: &mut [QueueItem],
    index: usize,
    live: &PipelineRunId,
    at: archon_core::Timestamp,
) -> Option<&'static str> {
    let endings: Vec<QueueState> = all_states(live)
        .into_iter()
        .filter(QueueState::is_terminal)
        .collect();
    let next = endings[rng.below(endings.len())].clone();
    let mut expected = model[index].clone();
    let legal = expected.transition(next.clone(), at).is_ok();
    let mut run = store.run(live).unwrap();
    run.state = archon_core::run::RunState::Cancelled { reason: None };
    run.finished_at = Some(at);
    let result = store.finish_run(&run, next, at).map(|f| f.item);
    assert_eq!(result.is_ok(), legal, "finish disagrees: {result:?}");
    if let Ok(item) = result {
        assert_eq!(item, expected);
        assert!(store.active_runs().unwrap().iter().all(|r| &r.id != live));
        model[index] = item;
    }
    legal.then(|| model[index].state.kind())
}

/// One random step for `model[index]`. Returns the state kind the store
/// accepted, if it accepted one.
fn walk_step(
    store: &mut Store,
    rng: &mut Rng,
    model: &mut [QueueItem],
    index: usize,
    at: archon_core::Timestamp,
) -> Option<&'static str> {
    let id = model[index].id.clone();
    let runs = store.runs_for_item(&id).unwrap();
    let live = runs
        .iter()
        .find(|r| !r.state.is_terminal())
        .map(|r| r.id.clone());
    if let Some(live) = &live
        && rng.below(3) == 0
    {
        return finish_step(store, rng, model, index, live, at);
    }
    let named = live.clone().or_else(|| runs.last().map(|r| r.id.clone()));
    let mut candidates = all_states(&named.clone().unwrap_or_default());
    let legal_only: Vec<QueueState> = candidates
        .iter()
        .filter(|c| model[index].state.can_transition_to(c))
        .cloned()
        .collect();
    if !legal_only.is_empty() && rng.below(2) == 0 {
        // Half the steps take a legal edge so the walk reaches deep states.
        candidates = legal_only;
    }
    let next = candidates[rng.below(candidates.len())].clone();
    let mut expected = model[index].clone();
    let legal = expected.transition(next.clone(), at).is_ok();
    if legal && matches!(next, QueueState::Running { .. }) && live.is_none() {
        // Running needs a live run: create it first, as the executor does.
        store.insert_run(&run_for(&model[index])).unwrap();
        return None;
    }
    let accept = legal && store_accepts(&next, live.as_ref(), named.is_some());
    let result = store.transition_item(&id, next, at);
    assert_eq!(
        result.is_ok(),
        accept,
        "store and model disagree: {result:?}"
    );
    if let Ok(item) = result {
        assert_eq!(item, expected);
        model[index] = item;
    }
    assert_eq!(store.item(&id).unwrap(), model[index]);
    accept.then(|| model[index].state.kind())
}

/// A live run exists exactly while the item is `running` on it, or still
/// `dispatching` after the run was created.
fn assert_live_run_matches_state(store: &Store, item: &QueueItem) {
    let live: Vec<PipelineRunId> = store
        .runs_for_item(&item.id)
        .unwrap()
        .into_iter()
        .filter(|r| !r.state.is_terminal())
        .map(|r| r.id)
        .collect();
    match &item.state {
        QueueState::Running { run } => assert_eq!(live, vec![run.clone()]),
        QueueState::Dispatching => assert!(live.len() <= 1),
        other => assert!(live.is_empty(), "{} with live run {live:?}", other.kind()),
    }
}

/// Random walks over the queue state machine, mirrored in memory with
/// `QueueItem::transition`: the store accepts exactly the legal edges whose
/// run references hold, and always agrees with the model.
#[test]
fn random_transition_walks_match_the_domain_model() {
    for seed in [0x5eed, 7, 1234567] {
        let mut rng = Rng::new(seed);
        let (mut store, _repo, items) = seeded(5);
        let mut model: Vec<QueueItem> = items.clone();
        let mut reached = std::collections::BTreeSet::new();
        for step in 0..1500 {
            let index = rng.below(model.len());
            if let Some(kind) = walk_step(&mut store, &mut rng, &mut model, index, t(100 + step)) {
                reached.insert(kind);
            }
            assert_live_run_matches_state(&store, &model[index]);
        }
        assert_eq!(reached.len(), 7, "seed {seed} only reached {reached:?}");
    }
}
