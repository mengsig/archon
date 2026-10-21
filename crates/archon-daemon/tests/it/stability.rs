//! Bad stored or external state fails the one record it belongs to, is set
//! aside with the commands that fix it, and never stops archond or the rest.

use crate::common::Harness;

use archon_core::queue::QueueState;
use archon_daemon::executor;

/// Corrupt variables of one run, including an empty object that used to trip
/// an assertion, fail that run's item and leave the other item running.
#[tokio::test]
async fn a_corrupt_run_variable_fails_its_item_and_recovery_serves_the_rest() {
    for corrupt in ["not json", "{}", "[1,2]"] {
        let mut h = Harness::new().await;
        let broken = h.enqueue("broken vars").await;
        let fine = h.enqueue("fine").await;
        h.dispatch().await;
        let run = h.run(&broken.id).await;
        h.ctx
            .store
            .lock()
            .await
            .kv_set(&format!("run_vars:{}", run.id), corrupt)
            .unwrap();

        h.restart();
        executor::recover(&h.ctx).await.expect("recovery succeeds");

        let QueueState::Failed { error, .. } = h.state(&broken.id).await else {
            panic!("the broken item fails for {corrupt:?}");
        };
        assert!(error.contains("archon queue retry"), "{error}");
        assert!(matches!(
            h.state(&fine.id).await,
            QueueState::Running { .. }
        ));
        let set_aside = archon_core::quarantine::list(&h.ctx.paths.data_dir).unwrap();
        assert_eq!(set_aside.len(), 1, "{set_aside:?}");
        assert_eq!(set_aside[0].id, run.id.as_str());
        assert!(set_aside[0].cancel.contains(broken.id.as_str()));
    }
}

const BROKEN_PIPELINE: &str = r#"[pipeline]
name = "broken"
description = "a pipeline edited into a bad state"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 30
"#;

fn implement_only() -> archon_core::model::ModelPlan {
    let choice = crate::common::choice(crate::common::MODEL, archon_core::model::Effort::Medium);
    archon_core::model::ModelPlan([("implement".to_owned(), vec![choice])].into())
}

/// A pipeline file edited into a bad stage after its item was queued fails
/// that item with the way out; the other pipeline's item still starts.
#[tokio::test]
async fn a_pipeline_file_with_a_bad_stage_fails_only_its_item() {
    let h = Harness::new().await;
    h.add_pipeline("broken", BROKEN_PIPELINE);
    let prompt = h.repo.join(".archon/pipelines/broken/implement.md");
    std::fs::write(prompt, "plan {{task.title}}\n\n{{task.brief}}\n").unwrap();
    let broken = h
        .enqueue_in("broken", "broken pipeline", implement_only())
        .await
        .expect("queued while the pipeline was fine");
    let fine = h.enqueue("fine").await;
    let bad = BROKEN_PIPELINE.replace("kind = \"work\"", "kind = \"no-such-kind\"");
    std::fs::write(h.repo.join(".archon/pipelines/broken/pipeline.toml"), bad).unwrap();

    h.dispatch().await;

    let QueueState::Failed { error, .. } = h.state(&broken.id).await else {
        panic!("the item with the bad pipeline fails");
    };
    assert!(error.contains("archon queue retry"), "{error}");
    assert!(matches!(
        h.state(&fine.id).await,
        QueueState::Running { .. }
    ));
}

/// A worktree removed under a running item must not stop the daemon or
/// the other item.
#[tokio::test]
async fn an_item_whose_worktree_is_gone_does_not_stop_the_others() {
    let h = Harness::new().await;
    let lost = h.enqueue("lost worktree").await;
    let fine = h.enqueue("fine").await;
    h.dispatch().await;
    let run = h.run(&lost.id).await;
    let tree = run.workspace.clone();
    std::fs::remove_dir_all(&tree).unwrap();

    h.settle_running(|stage| {
        if stage.run == run.id {
            crate::common::completed("done")
        } else {
            crate::common::completed("also done")
        }
    })
    .await;
    h.dispatch().await;

    assert!(
        matches!(
            h.state(&fine.id).await,
            QueueState::Running { .. } | QueueState::Succeeded { .. }
        ),
        "the other item goes on: {:?}",
        h.state(&fine.id).await
    );
}

/// An event type this archon has never heard of, aimed at a live stage's
/// agent, is skipped; the stage and the daemon carry on.
#[tokio::test]
async fn an_unknown_aether_event_is_ignored_and_the_stage_keeps_running() {
    let h = Harness::new().await;
    let item = h.enqueue("unknown event").await;
    h.dispatch().await;
    let stage = h.running().await.into_iter().next().expect("a live stage");
    let event = archon_aether::events::parse_event(serde_json::json!({
        "seq": 900, "at": "2026-10-09T10:00:00Z", "agent_id": stage.aether_agent,
        "kind": {"type": "lease.granted", "lease": {"id": 1}}
    }))
    .expect("unknown kinds parse");

    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .expect("an unknown event is not an error");

    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

/// A branch deleted on the remote does not fail its item: it forks from the
/// local branch, says so, and the daemon goes on serving.
#[tokio::test]
async fn a_branch_deleted_on_the_remote_degrades_to_the_local_branch() {
    let h = Harness::new().await;
    let origin = crate::common::delivery::add_origin(&h);
    crate::common::git(&origin, &["update-ref", "-d", "refs/heads/main"]);
    let lost = h.enqueue("remote branch gone").await;
    let fine = h.enqueue("also queued").await;

    h.dispatch().await;

    for id in [&lost.id, &fine.id] {
        assert!(
            matches!(h.state(id).await, QueueState::Running { .. }),
            "{id} runs: {:?}",
            h.state(id).await
        );
    }
    let notices = h.ctx.store.lock().await.feed_after(0, 1000).unwrap();
    let told = notices
        .iter()
        .any(|f| format!("{:?}", f.entry).contains("Could not fetch"));
    assert!(told, "the operator is told the base was not fetched");
}

/// A quarantine list that cannot be reset (a file where its directory
/// belongs) is logged, and every healthy run still recovers.
#[tokio::test]
async fn an_unusable_quarantine_directory_does_not_stop_recovery() {
    let mut h = Harness::new().await;
    let item = h.enqueue("recovers anyway").await;
    h.dispatch().await;
    let blocked = h.ctx.paths.data_dir.join("quarantine");
    let _ = std::fs::remove_dir_all(&blocked);
    std::fs::write(&blocked, "not a directory").unwrap();

    h.restart();
    executor::recover(&h.ctx).await.expect("recovery succeeds");

    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Running { .. }
    ));
}

/// A stage row that no longer decodes fails its run with the way out;
/// recovery still serves the other item.
#[tokio::test]
async fn an_undecodable_stage_row_fails_its_run_and_recovery_serves_the_rest() {
    let mut h = Harness::new().await;
    let broken = h.enqueue("broken stage row").await;
    let fine = h.enqueue("fine").await;
    h.dispatch().await;
    let run = h.run(&broken.id).await;
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    db.execute(
        "UPDATE stage_runs SET data = '{' WHERE run = ?1",
        [run.id.as_str()],
    )
    .unwrap();
    drop(db);

    h.restart();
    executor::recover(&h.ctx).await.expect("recovery succeeds");

    let QueueState::Failed { error, .. } = h.state(&broken.id).await else {
        panic!("the item with the unreadable stage row fails");
    };
    assert!(error.contains("archon queue retry"), "{error}");
    assert!(matches!(
        h.state(&fine.id).await,
        QueueState::Running { .. }
    ));
    let store = h.ctx.store.lock().await;
    assert!(store.stage_runs(&run.id).expect("readable now").is_empty());
    assert!(store.stage_runs_for_item(&broken.id).is_ok());
}

/// After an undecodable live stage row is set aside, the sweeps over live
/// stages still see the healthy ones.
#[tokio::test]
async fn an_undecodable_live_stage_row_does_not_hide_the_healthy_ones() {
    let h = Harness::new().await;
    let broken = h.enqueue("broken stage row").await;
    let fine = h.enqueue("fine").await;
    h.dispatch().await;
    let run = h.run(&broken.id).await;
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    db.execute(
        "UPDATE stage_runs SET data = '{' WHERE run = ?1",
        [run.id.as_str()],
    )
    .unwrap();
    drop(db);

    let live = h
        .ctx
        .store
        .lock()
        .await
        .live_stage_runs()
        .expect("sweep read");

    let healthy = h.run(&fine.id).await;
    assert!(live.iter().any(|stage| stage.run == healthy.id));
    assert!(live.iter().all(|stage| stage.run != run.id));
}

/// A stage row that operator feedback cites is kept: detaching it would
/// break the feedback table's CHECK constraints.
#[tokio::test]
async fn an_unreadable_stage_row_cited_by_feedback_is_kept() {
    let h = Harness::new().await;
    let broken = h.enqueue("cited stage row").await;
    h.dispatch().await;
    let run = h.run(&broken.id).await;
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    let stage: String = db
        .query_row(
            "SELECT id FROM stage_runs WHERE run = ?1",
            [run.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    db.execute(
        "INSERT INTO operator_feedback (id, request_key, item, run, stage, stage_run, model,
             effort, repo, pipeline, revision, review, task_class, cause, reason, observed_at, data)
         VALUES ('fb1', 'k', ?1, ?2, 's', ?3, 'm', 'medium', 'r', 'p', 'rev', 'rv', 'tc',
             'quality', 'why', '2026-01-01T00:00:00Z', '{}')",
        [broken.id.as_str(), run.id.as_str(), stage.as_str()],
    )
    .unwrap();
    db.execute("UPDATE stage_runs SET data = '{' WHERE id = ?1", [&stage])
        .unwrap();
    drop(db);

    let dropped = h
        .ctx
        .store
        .lock()
        .await
        .drop_unreadable_stages(&run.id, &h.ctx.paths.data_dir.join("quarantine-rows"))
        .expect("a cited row is skipped, not an error");

    assert!(dropped.is_empty(), "{dropped:?}");
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    let kept: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM stage_runs WHERE id = ?1",
            [&stage],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1);
    let store = h.ctx.store.lock().await;
    assert!(store.stage_runs(&run.id).expect("lenient read").is_empty());
    assert!(store.stage_runs_strict(&run.id).is_err());
}

/// An undecodable run record fails its item, is listed, and neither stops
/// recovery of the healthy run nor vanishes at the next restart.
#[tokio::test]
async fn an_undecodable_run_record_fails_its_item_and_stays_listed_across_restarts() {
    let mut h = Harness::new().await;
    let broken = h.enqueue("broken run").await;
    let fine = h.enqueue("fine").await;
    h.dispatch().await;
    let run = h.run(&broken.id).await;
    let db = rusqlite::Connection::open(&h.ctx.paths.store).unwrap();
    db.execute(
        "UPDATE runs SET data = '{' WHERE id = ?1",
        [run.id.as_str()],
    )
    .unwrap();
    drop(db);

    for _ in 0..2 {
        h.restart();
        executor::recover(&h.ctx).await.expect("recovery succeeds");

        let QueueState::Failed { error, .. } = h.state(&broken.id).await else {
            panic!("the item with the unreadable run fails");
        };
        assert!(error.contains("archon queue retry"), "{error}");
        assert!(matches!(
            h.state(&fine.id).await,
            QueueState::Running { .. }
        ));
        let set_aside = archon_core::quarantine::list(&h.ctx.paths.data_dir).unwrap();
        assert_eq!(set_aside.len(), 1, "{set_aside:?}");
        assert_eq!(set_aside[0].id, run.id.as_str());
    }
}

/// An owed auto-merge that cannot be decoded is removed and listed; a watch
/// of checks that cannot be decoded likewise, and the pass goes on.
#[tokio::test]
async fn unusable_owed_merge_and_watch_records_are_set_aside_by_a_pass() {
    let h = Harness::new().await;
    let run = "run_00000000000000000000000000";
    {
        let mut store = h.ctx.store.lock().await;
        store
            .kv_set(&format!("ship_automerge:{run}"), "not json")
            .unwrap();
        store.kv_set(&format!("ship_checks:{run}"), "{}").unwrap();
    }

    executor::coordination::reconcile(&h.ctx)
        .await
        .expect("the pass survives bad records");

    let store = h.ctx.store.lock().await;
    assert!(
        store
            .kv_get(&format!("ship_automerge:{run}"))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .kv_get(&format!("ship_checks:{run}"))
            .unwrap()
            .is_none()
    );
    let kinds: Vec<_> = archon_core::quarantine::list(&h.ctx.paths.data_dir)
        .unwrap()
        .into_iter()
        .map(|r| r.kind)
        .collect();
    assert!(kinds.contains(&"owed-merge".to_owned()), "{kinds:?}");
    assert!(kinds.contains(&"checks-watch".to_owned()), "{kinds:?}");
}
