//! Coordinated admission, delivery and recovery across repositories.
mod common;

use archon_core::initiative::{
    EnqueueInitiative, InitiativeDetail, InitiativeMember, InitiativeState,
};
use archon_core::protocol::{Reply, RepoAdd, RepoRef, Request};
use archon_core::queue::QueueState;
use archon_core::repo::ShipMode;
use common::initiative::{finish_item, install_gh, prs};
use common::{Harness, PIPELINE, REPO_NAME, plan};

fn request() -> EnqueueInitiative {
    EnqueueInitiative {
        title: "Rename account field".into(),
        contract: "Every API uses account_id.".into(),
        atomic: true,
        idempotency_key: Some("account-v2".into()),
        members: vec![member(REPO_NAME), member("client"), member("docs")],
    }
}

fn member(repo: &str) -> InitiativeMember {
    InitiativeMember {
        repo: RepoRef(repo.into()),
        brief: format!("Change {repo} to account_id"),
        title: None,
        pipeline: Some(PIPELINE.into()),
        model_plan: Some(plan()),
        after: Vec::new(),
    }
}

async fn setup() -> Harness {
    let h = Harness::new().await;
    install_gh(&h);
    for name in ["client", "docs"] {
        let path = common::init_repo(&h.dir.path().join(name));
        h.cli(Request::RepoAdd(RepoAdd {
            path,
            name: Some(name.into()),
            ship: Some(ShipMode::Pr),
            base_branch: Some("main".into()),
            default_pipeline: Some(PIPELINE.into()),
            auto_merge: false,
        }))
        .await
        .unwrap();
    }
    h
}

async fn enqueue(h: &Harness, request: EnqueueInitiative) -> InitiativeDetail {
    match h.cli(Request::EnqueueInitiative(request)).await.unwrap() {
        Reply::Initiative(detail) => *detail,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn three_repositories_link_drafts_and_emit_one_finish_across_restart() {
    let mut h = setup().await;
    let detail = enqueue(&h, request()).await;
    h.dispatch().await;
    finish_item(&h, &detail.members[0].id).await;
    assert!(
        prs(&h)
            .as_object()
            .unwrap()
            .values()
            .all(|p| p["isDraft"] == true)
    );
    h.restart();
    archon_daemon::executor::recover(&h.ctx).await.unwrap();
    h.dispatch().await;
    for member in &detail.members[1..] {
        finish_item(&h, &member.id).await;
    }
    h.dispatch().await;
    let all = prs(&h);
    assert_eq!(all.as_object().unwrap().len(), 3);
    for pr in all.as_object().unwrap().values() {
        assert_eq!(pr["isDraft"], false);
        let body = pr["body"].as_str().unwrap();
        assert_eq!(body.matches("Part of Rename account field:").count(), 1);
        assert_eq!(body.matches("https://github.com/o/r/pull/").count(), 2);
    }
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .initiative(&detail.initiative.id)
            .unwrap()
            .initiative
            .state,
        InitiativeState::Done
    );
    h.restart();
    h.dispatch().await;
    let outbox = h.kv("trigger_outbox").await.unwrap();
    assert_eq!(outbox.matches("initiative_finished").count(), 1);
    assert!(!outbox.contains("item_finished"));
}

#[tokio::test]
async fn enqueue_is_atomic_and_idempotent_and_live_contract_delivery_recovers() {
    let mut h = setup().await;
    let mut invalid = request();
    invalid.members[2].after = vec![0, 0];
    assert!(h.cli(Request::EnqueueInitiative(invalid)).await.is_err());
    assert!(h.ctx.store.lock().await.initiatives().unwrap().is_empty());
    let detail = enqueue(&h, request()).await;
    assert_eq!(enqueue(&h, request()).await, detail);
    let mut different = request();
    different.contract = "different".into();
    assert!(h.cli(Request::EnqueueInitiative(different)).await.is_err());
    h.dispatch().await;
    h.backend
        .fail_next_message(archon_core::ArchonError::Unavailable(
            "transport lost".into(),
        ));
    assert!(
        h.cli(Request::UpdateInitiative {
            initiative: detail.initiative.id.clone(),
            contract_append: "Clients accept old names during rollout.".into()
        })
        .await
        .is_err()
    );
    h.restart();
    archon_daemon::executor::recover(&h.ctx).await.unwrap();
    h.dispatch().await;
    let deliveries = h.backend.messages();
    assert!(
        deliveries
            .iter()
            .any(|(_, s)| s.contains("old names during rollout"))
    );
    let count = h.backend.message_calls().len();
    h.dispatch().await;
    assert_eq!(h.backend.message_calls().len(), count);
    let too_big = "x".repeat(2048);
    assert!(
        h.cli(Request::UpdateInitiative {
            initiative: detail.initiative.id,
            contract_append: too_big
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn same_repository_members_stack_from_their_shipped_sibling() {
    let h = setup().await;
    let mut req = request();
    req.members = vec![member(REPO_NAME), member(REPO_NAME)];
    req.members[1].after = vec![0];
    let detail = enqueue(&h, req).await;
    h.dispatch().await;
    let parent = h.run(&detail.members[0].id).await;
    common::initiative::commit(&parent.workspace, "shared.txt", "account_id\n");
    finish_item(&h, &detail.members[0].id).await;
    let child = h.run(&detail.members[1].id).await;
    assert!(child.workspace.join("shared.txt").exists());
    let base = h.ctx.store.lock().await.stack_base(&child.item).unwrap();
    assert_eq!(base.unwrap().0, parent.branch);
    finish_item(&h, &child.item).await;
    assert_eq!(prs(&h)[&child.branch]["baseRefName"], parent.branch);
    assert_eq!(prs(&h)[&parent.branch]["isDraft"], false);
    assert_eq!(prs(&h)[&child.branch]["isDraft"], false);
}

#[tokio::test]
async fn cancellation_stops_unfinished_members_and_retry_preserves_successful_ones() {
    let h = setup().await;
    let detail = enqueue(&h, request()).await;
    h.dispatch().await;
    finish_item(&h, &detail.members[0].id).await;
    h.cli(Request::CancelInitiative {
        initiative: detail.initiative.id.clone(),
    })
    .await
    .unwrap();
    assert!(matches!(
        h.state(&detail.members[0].id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(matches!(
        h.state(&detail.members[1].id).await,
        QueueState::Cancelled { .. }
    ));
    h.cli(Request::RetryInitiative {
        initiative: detail.initiative.id.clone(),
    })
    .await
    .unwrap();
    assert!(matches!(
        h.state(&detail.members[0].id).await,
        QueueState::Succeeded { .. }
    ));
    assert_eq!(h.state(&detail.members[1].id).await, QueueState::Queued);
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .initiative(&detail.initiative.id)
            .unwrap()
            .initiative
            .state,
        InitiativeState::Open
    );
}

#[tokio::test]
async fn a_failed_member_holds_dependents_and_retry_reopens_the_group() {
    let h = setup().await;
    let mut req = request();
    req.members[1].after = vec![0];
    req.members[2].after = vec![1];
    let detail = enqueue(&h, req).await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let outcome = archon_core::run::StageOutcome::Failed {
        error: "cannot implement contract".into(),
        infra: false,
    };
    archon_daemon::executor::stage_settled(&h.ctx, stage.aether_agent.as_ref().unwrap(), outcome)
        .await
        .unwrap();
    archon_daemon::dispatcher::dispatch_once(&h.ctx)
        .await
        .unwrap();
    assert_eq!(h.state(&detail.members[1].id).await, QueueState::Queued);
    assert_eq!(h.state(&detail.members[2].id).await, QueueState::Queued);
    h.dispatch().await;
    let group = h
        .ctx
        .store
        .lock()
        .await
        .initiative(&detail.initiative.id)
        .unwrap();
    assert_eq!(group.initiative.state, InitiativeState::Failed);
    assert!(group.members.iter().skip(1).all(|m| !m.state.is_active()));
    assert!(h.backend.created() < 3);
    h.cli(Request::RetryInitiative {
        initiative: detail.initiative.id.clone(),
    })
    .await
    .unwrap();
    let group = h
        .ctx
        .store
        .lock()
        .await
        .initiative(&detail.initiative.id)
        .unwrap();
    assert_eq!(group.initiative.state, InitiativeState::Open);
    assert!(group.members.iter().all(|m| m.state == QueueState::Queued));
}

#[tokio::test]
async fn a_store_failure_after_the_first_member_rolls_back_the_whole_initiative() {
    let h = setup().await;
    let req = request();
    let first = h.enqueue("template").await;
    let initiative = archon_core::initiative::Initiative::new(
        &req.title,
        &req.contract,
        req.atomic,
        archon_core::now(),
    )
    .unwrap();
    let mut members = vec![first.clone(); 3];
    for member in &mut members {
        member.id = archon_core::id::QueueItemId::new();
        member.initiative = Some(initiative.id.clone());
    }
    members[1].repo = Some(archon_core::id::RepoId::new());
    let mut store = h.ctx.store.lock().await;
    assert!(
        store
            .insert_initiative(&initiative, &members, &req)
            .is_err()
    );
    assert!(store.initiatives().unwrap().is_empty());
    assert_eq!(store.items(&Default::default()).unwrap(), vec![first]);
}

#[tokio::test]
async fn a_member_needs_only_repo_and_brief_and_its_default_model_obeys_policy() {
    let h = setup().await;
    h.repo_config("[models]\nallow = [\"codex/gpt-5\"]\nmax_effort = \"medium\"\n");
    let mut req = request();
    req.members.truncate(1);
    req.members[0].model_plan = None;
    req.members[0].pipeline = None;
    let detail = enqueue(&h, req).await;
    let plan = &detail.members[0].model_plan;
    assert!(!plan.0.is_empty());
    assert!(
        plan.0
            .values()
            .flatten()
            .all(|c| c.model.to_string() == "codex/gpt-5"
                && c.effort == archon_core::model::Effort::Medium)
    );
    h.dispatch().await;
    assert!(matches!(
        h.state(&detail.members[0].id).await,
        QueueState::Running { .. }
    ));
}
