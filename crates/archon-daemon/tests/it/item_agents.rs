//! Backend lifecycle reads stay scoped to the item's recorded stages.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{QueueItemId, StageRunId};
use archon_core::ports::DescendantState;
use archon_core::protocol::{ItemAgentState, ItemAgentStates, Reply, Request};

use crate::common::{Harness, MODEL};

async fn read(h: &Harness, item: &QueueItemId) -> Result<ItemAgentStates, ArchonError> {
    match h
        .cli(Request::ItemAgentStates { item: item.clone() })
        .await?
    {
        Reply::ItemAgentStates(states) => {
            assert!(states.agents.len() <= 2048);
            assert!(
                states
                    .agents
                    .windows(2)
                    .all(|pair| pair[0].agent < pair[1].agent)
            );
            Ok(states)
        }
        other => panic!("item_agent_states replied {other:?}"),
    }
}

#[tokio::test]
async fn reports_actual_lifecycle_including_finished_and_nested_delegates() {
    let h = Harness::new().await;
    let item = h.enqueue("lifecycle read").await;
    h.dispatch().await;
    let root = h.stages(&item.id).await[0].aether_agent.clone().unwrap();
    let live = h.backend.delegate(&root, MODEL);
    let completed = h.backend.delegate(&root, MODEL);
    let stopped = h.backend.delegate(&completed, MODEL);
    h.backend
        .end_delegated(&completed, DescendantState::Completed);
    h.backend.end_delegated(&stopped, DescendantState::Stopped);
    let before = h.ctx.store.lock().await.write_count();
    let states = read(&h, &item.id).await.unwrap();
    assert_eq!(states.agents.len(), 3);
    for (agent, expected) in [
        (live, ItemAgentState::Live),
        (completed, ItemAgentState::Completed),
        (stopped, ItemAgentState::Stopped),
    ] {
        assert_eq!(
            states
                .agents
                .iter()
                .find(|s| s.agent == agent)
                .unwrap()
                .state,
            expected
        );
    }
    assert_eq!(h.backend.descendant_checks(), [root]);
    assert_eq!(h.ctx.store.lock().await.write_count(), before, "read only");
}

#[tokio::test]
async fn never_queries_another_item_and_rejects_missing_items() {
    let h = Harness::new().await;
    let first = h.enqueue("first tree").await;
    let other = h.enqueue("other tree").await;
    h.dispatch().await;
    let root = h.stages(&first.id).await[0].aether_agent.clone().unwrap();
    let foreign = h.stages(&other.id).await[0].aether_agent.clone().unwrap();
    let own = h.backend.delegate(&root, MODEL);
    h.backend.delegate(&foreign, MODEL);
    let states = read(&h, &first.id).await.unwrap();
    assert_eq!(states.agents.len(), 1);
    assert_eq!(states.agents[0].agent, own);
    assert_eq!(h.backend.descendant_checks(), std::slice::from_ref(&root));
    assert!(matches!(
        read(&h, &QueueItemId::new()).await,
        Err(ArchonError::NotFound(_))
    ));
    assert_eq!(h.backend.descendant_checks(), [root]);
}

#[tokio::test]
async fn a_queued_item_has_no_backend_queries_and_backend_failure_is_visible() {
    let h = Harness::new().await;
    let item = h.enqueue("queued then unavailable").await;
    assert!(read(&h, &item.id).await.unwrap().agents.is_empty());
    assert!(h.backend.descendant_checks().is_empty());
    h.dispatch().await;
    h.backend
        .fail_next_descendants(ArchonError::Unavailable("backend offline".into()));
    let error = read(&h, &item.id).await.unwrap_err();
    assert!(error.to_string().contains("backend offline"));
    assert!(error.to_string().contains("item_agent_states"));
    assert_eq!(h.backend.descendant_checks().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_slow_backend_times_out_and_does_not_hold_the_store_lock() {
    let h = Harness::new().await;
    let item = h.enqueue("slow lifecycle read").await;
    h.dispatch().await;
    let _gate = h.backend.hold_descendants_after(0);
    let request = read(&h, &item.id);
    let lock_check = async {
        while h.backend.descendant_checks().is_empty() {
            tokio::task::yield_now().await;
        }
        let _store = h.ctx.store.lock().await;
    };
    let (result, ()) = tokio::join!(request, lock_check);
    assert!(matches!(result, Err(ArchonError::Unavailable(_))));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("exceeded 5 seconds")
    );
    assert_eq!(h.backend.descendant_checks().len(), 1, "no retry");
}

#[tokio::test]
async fn an_oversized_tree_is_refused_instead_of_truncated() {
    let h = Harness::new().await;
    let item = h.enqueue("oversized tree").await;
    h.dispatch().await;
    let root = h.stages(&item.id).await[0].aether_agent.clone().unwrap();
    for _ in 0..2049 {
        h.backend.delegate(&root, MODEL);
    }
    let error = read(&h, &item.id).await.unwrap_err();
    assert!(matches!(error, ArchonError::Unavailable(_)));
    assert!(error.to_string().contains("2048 delegated agents"));
    assert_eq!(h.backend.descendant_checks(), [root]);
}

#[tokio::test]
async fn too_many_stage_roots_are_refused_before_backend_io() {
    let h = Harness::new().await;
    let item = h.enqueue("many stage roots").await;
    h.dispatch().await;
    let mut stage = h.stages(&item.id).await.remove(0);
    {
        let mut store = h.ctx.store.lock().await;
        for slot in 1..65 {
            stage.id = StageRunId::new();
            stage.fan_index = slot;
            stage.aether_agent = Some(AetherAgentId::parse(&format!("agt_extra{slot}")).unwrap());
            store.upsert_stage_run(&stage).unwrap();
        }
    }
    let error = read(&h, &item.id).await.unwrap_err();
    assert!(error.to_string().contains("64 stage agents"));
    assert!(h.backend.descendant_checks().is_empty());
}

#[tokio::test]
async fn a_reused_stage_agent_is_queried_only_once() {
    let h = Harness::new().await;
    let item = h.enqueue("reused stage agent").await;
    h.dispatch().await;
    let mut stage = h.stages(&item.id).await.remove(0);
    let root = stage.aether_agent.clone().unwrap();
    let delegated = h.backend.delegate(&root, MODEL);
    {
        let mut store = h.ctx.store.lock().await;
        stage.state = archon_core::run::StageState::Completed;
        stage.finished_at = Some(archon_core::now());
        store.upsert_stage_run(&stage).unwrap();
        stage.id = StageRunId::new();
        stage.attempt += 1;
        stage.state = archon_core::run::StageState::Running;
        stage.finished_at = None;
        store.upsert_stage_run(&stage).unwrap();
    }
    let states = read(&h, &item.id).await.unwrap();
    assert_eq!(states.agents.len(), 1);
    assert_eq!(states.agents[0].agent, delegated);
    assert_eq!(h.backend.descendant_checks(), [root]);
}

#[tokio::test]
async fn includes_direct_ledger_cost_without_charging_a_parent_for_its_children() {
    use archon_core::cost::CostBasis;
    use archon_queue::store::SpendRow;
    let h = Harness::new().await;
    let item = h.enqueue("direct graph costs").await;
    h.dispatch().await;
    let root = h.stages(&item.id).await[0].aether_agent.clone().unwrap();
    let child = h.backend.delegate(&root, MODEL);
    for (agent, cost_usd) in [(&root, 1.25), (&child, 0.75)] {
        h.ctx
            .store
            .lock()
            .await
            .record_spend(&SpendRow {
                agent: agent.clone(),
                item: Some(item.id.clone()),
                cost_usd,
                cost_basis: CostBasis::ProviderReported,
                cost_complete: None,
                estimated_cost_usd: 0.0,
                input_tokens: 1,
                output_tokens: 1,
                at: archon_core::now(),
            })
            .unwrap();
    }
    let result = read(&h, &item.id).await.unwrap();
    assert_eq!(result.cost_usd[&root], 1.25);
    assert_eq!(result.cost_usd[&child], 0.75);
    assert_eq!(result.cost_usd.len(), 2);
}
