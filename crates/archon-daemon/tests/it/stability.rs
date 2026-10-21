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
