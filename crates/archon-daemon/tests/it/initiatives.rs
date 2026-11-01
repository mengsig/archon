//! Coordinated admission, delivery and recovery across repositories.
use crate::common;

use archon_core::autonomy::{MergeMode, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::initiative::{
    ContractChange, EnqueueInitiative, InitiativeDetail, InitiativeMember, InitiativeState,
};
use archon_core::model::Effort;
use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
use archon_core::protocol::{Reply, RepoAdd, RepoRef, Request};
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::queue::QueueState;
use archon_core::repo::ShipMode;
use common::initiative::{finish_item, install_gh, prs};
use common::{Harness, MODEL, PIPELINE, REPO_NAME, plan};
use serde_json::json;

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
        onto: None,
        repo: RepoRef(repo.into()),
        brief: format!("Change {repo} to account_id"),
        title: None,
        pipeline: Some(PIPELINE.into()),
        model_plan: Some(plan().into()),
        autonomy: Default::default(),
        after: Vec::new(),
    }
}

async fn setup() -> Harness {
    let h = Harness::new().await;
    install_gh(&h);
    for name in ["client", "docs"] {
        let path = common::init_repo_with_origin(&h.dir.path().join(name));
        h.cli(Request::RepoAdd(RepoAdd {
            path,
            name: Some(name.into()),
            ship: Some(ShipMode::Pr),
            base_branch: Some("main".into()),
            default_pipeline: Some(PIPELINE.into()),
            auto_merge: false,
            github_watch: false,
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

/// A member queued `onto` a pull request leaves that pull request's body and
/// draft state as its owner set them; the others still link to it.
#[tokio::test]
async fn an_onto_members_pull_request_keeps_its_body_and_draft_state() {
    let h = setup().await;
    let detail = enqueue(&h, request()).await;
    let first = &detail.members[0];
    let mut onto = h.item(&first.id).await;
    let head = common::git(&h.repo, &["rev-parse", "main"]);
    onto.onto = Some(Box::new(OntoPullRequest {
        target: OntoTarget::Pr(100),
        pr: PullRequestRef::from_url("https://github.com/o/r/pull/100").unwrap(),
        node_id: None,
        head_repository: "o/r".into(),
        head_ref: "feature".into(),
        base_ref: "main".into(),
        head_sha: head,
        review_only: false,
    }));
    h.ctx.store.lock().await.update_item(&onto).unwrap();
    h.dispatch().await;
    for member in &detail.members {
        finish_item(&h, &member.id).await;
    }
    h.dispatch().await;

    let branch = h.run(&first.id).await.branch;
    let all = prs(&h);
    let delivered_into = &all[&branch];
    assert_eq!(
        delivered_into["body"], "Change details.",
        "{delivered_into}"
    );
    assert_eq!(delivered_into["isDraft"], true, "{delivered_into}");
    let url = delivered_into["url"].as_str().unwrap();
    for (other, pr) in all
        .as_object()
        .unwrap()
        .iter()
        .filter(|(b, _)| **b != branch)
    {
        let body = pr["body"].as_str().unwrap();
        assert!(body.contains(url), "{other}: {body}");
    }
    let calls = common::initiative::gh_calls(&h);
    // Only its risk label and section change, never its title, base or draft state.
    let risk = |call: &Vec<String>| {
        call[3..] == ["--body-file", "-"] || call.contains(&"--add-label".into())
    };
    let touched =
        |call: &&Vec<String>| call.iter().any(|arg| arg == url) && call[1] != "view" && !risk(call);
    assert!(!calls.iter().any(|c| touched(&c)), "{calls:?}");
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
            change: ContractChange::Append("Clients accept old names during rollout.".into()),
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
            change: ContractChange::Append(too_big),
        })
        .await
        .is_err()
    );
}

/// A replaced contract reaches the members still running whole, and
/// `[limits]` sets how many members and contract bytes are allowed.
#[tokio::test]
async fn a_contract_is_revised_within_the_configured_limits() {
    let mut config = archon_core::config::Config::default();
    config.limits.initiative_members = 2;
    config.limits.initiative_contract_bytes = 4096;
    let h = Harness::with_config(config).await;
    install_gh(&h);
    let path = common::init_repo_with_origin(&h.dir.path().join("client"));
    h.cli(Request::RepoAdd(RepoAdd {
        path,
        name: Some("client".into()),
        ship: Some(ShipMode::Pr),
        base_branch: Some("main".into()),
        default_pipeline: Some(PIPELINE.into()),
        auto_merge: false,
        github_watch: false,
    }))
    .await
    .unwrap();
    let error = h
        .cli(Request::EnqueueInitiative(request()))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("`[limits] initiative_members`"),
        "{error}"
    );
    let mut two = request();
    two.members.truncate(2);
    let detail = enqueue(&h, two).await;
    h.dispatch().await;
    let revise = |change| Request::UpdateInitiative {
        initiative: detail.initiative.id.clone(),
        change,
    };
    let longer = format!("Use customer_id. {}", "x".repeat(3000));
    let Reply::Initiative(revised) = h
        .cli(revise(ContractChange::Replace(longer.clone())))
        .await
        .unwrap()
    else {
        panic!("an initiative");
    };
    assert_eq!(revised.initiative.contract, longer);
    let delivered: Vec<String> = h.backend.messages().into_iter().map(|(_, m)| m).collect();
    assert!(!delivered.is_empty(), "the running members are told");
    assert!(delivered.iter().all(|m| m.contains("Use customer_id.")));
    assert!(
        delivered.iter().all(|m| !m.contains("account_id")),
        "{delivered:?}"
    );
    let over = ContractChange::Append("y".repeat(1100));
    assert!(h.cli(revise(over)).await.is_err(), "4096 bytes at most");
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

/// Codex has a fast and a strong model; Claude only a strong one.
fn tier_catalog() -> ModelCatalog {
    let entry = |model: &str, tier| CatalogModel {
        model: model.parse().unwrap(),
        tier: Some(tier),
        is_default: false,
    };
    ModelCatalog::new(vec![
        entry("codex/mini", Tier::Fast),
        entry("codex/sol", Tier::Strong),
        entry(MODEL, Tier::Strong),
    ])
}

/// A one-member request whose member gives `plan`, on a harness that knows
/// [`tier_catalog`].
async fn tiered(plan: serde_json::Value) -> (Harness, EnqueueInitiative) {
    let h = setup().await;
    h.ctx.store.lock().await.set_model_catalog(tier_catalog());
    let mut req = request();
    req.members.truncate(1);
    req.members[0].model_plan = Some(serde_json::from_value(plan).unwrap());
    (h, req)
}

/// A member's tier resolves as an enqueue's does, on the provider it names.
/// A member gives no model_reasons, so an effort above the default stands.
#[tokio::test]
async fn a_member_tier_resolves_to_a_model_on_its_provider() {
    let (h, req) = tiered(json!({
        "implement": [{"tier": "strong", "provider": "codex", "effort": "high"}],
        "review": [{"tier": "fast", "provider": "codex"}, {"model": MODEL}],
    }))
    .await;
    let detail = enqueue(&h, req).await;
    let plan = &detail.members[0].model_plan;
    let implement = &plan.0["implement"][0];
    assert_eq!(implement.model.to_string(), "codex/sol");
    assert_eq!(implement.effort, Effort::High);
    let tier = implement.tier.as_ref().expect("tier metadata");
    assert_eq!(tier.requested, Tier::Strong);
    assert_eq!(tier.resolved, Tier::Strong);
    assert_eq!(tier.provider.as_deref(), Some("codex"));
    let review = &plan.0["review"];
    assert_eq!(review[0].model.to_string(), "codex/mini");
    assert_eq!(review[0].effort, Effort::Medium, "the stage default");
    assert_eq!(
        (review[1].model.to_string(), review[1].tier.as_ref()),
        (MODEL.into(), None)
    );
    assert!(plan.choices_for("ship").is_some(), "Archon fills the rest");
}

/// What a member's plan is refused for, with no initiative created.
async fn policy_refusal(repo_config: &str, plan: serde_json::Value) -> String {
    let (h, req) = tiered(plan).await;
    h.repo_config(repo_config);
    let refused = h.cli(Request::EnqueueInitiative(req)).await;
    let Err(ArchonError::Policy(why)) = refused else {
        panic!("{refused:?}")
    };
    assert!(h.ctx.store.lock().await.initiatives().unwrap().is_empty());
    why
}

#[tokio::test]
async fn a_member_plan_is_held_to_the_model_policy_like_an_enqueue() {
    let pinned = json!({"implement": [{"model": MODEL}]});
    let unlisted = policy_refusal("[models]\nallow = [\"codex/sol\"]\n", pinned.clone()).await;
    assert!(
        unlisted.contains(MODEL) && unlisted.contains("not allowed"),
        "{unlisted}"
    );
    let denied = policy_refusal(&format!("[models]\ndeny = [\"{MODEL}\"]\n"), pinned).await;
    assert!(
        denied.contains(MODEL) && denied.contains("not allowed"),
        "{denied}"
    );
    let high = json!({"implement": [{"tier": "strong", "provider": "codex", "effort": "high"}]});
    let capped = policy_refusal("[models]\nmax_effort = \"medium\"\n", high).await;
    assert!(
        capped.contains("exceeds the configured maximum medium"),
        "{capped}"
    );
}

/// A member takes an enqueue's knobs, but an atomic initiative's members are
/// merged by hand, together: one asking to merge itself refuses the whole set.
#[tokio::test]
async fn a_member_takes_its_own_knobs_but_never_merges_an_atomic_set_alone() {
    let h = setup().await;
    let mut req = request();
    req.members.truncate(2);
    req.members[0].pipeline = None;
    req.members[0].model_plan = None;
    req.members[0].autonomy.review = Some(ReviewLevel::None);
    req.members[1].autonomy.merge = Some(MergeMode::Auto);
    let refused = h.cli(Request::EnqueueInitiative(req.clone())).await;
    let Err(ArchonError::Invalid(why)) = refused else {
        panic!("{refused:?}")
    };
    assert!(why.contains("by hand"), "{why}");
    assert!(h.ctx.store.lock().await.initiatives().unwrap().is_empty());

    req.members[1].autonomy.merge = None;
    let detail = enqueue(&h, req).await;
    let unreviewed = &detail.members[0];
    assert_eq!(unreviewed.pipeline, "direct", "a review level runs direct");
    assert_eq!(unreviewed.autonomy.review, ReviewLevel::None);
    let manual = |m: &archon_core::queue::QueueItem| m.autonomy.merge == MergeMode::Manual;
    assert!(detail.members.iter().all(manual));
}

#[tokio::test]
async fn atomic_members_keep_manual_merge_after_enqueue_and_success() {
    use archon_core::autonomy::AutonomyPatch;
    use archon_core::protocol::{ItemPatch, RepoPatch};
    use archon_core::steer::Steer;
    let h = setup().await;
    let mut req = request();
    req.members.truncate(1);
    let detail = enqueue(&h, req).await;
    let id = &detail.members[0].id;
    let auto = AutonomyPatch::parse_list("merge=auto").unwrap();
    h.cli(Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch {
            autonomy_max: Some(auto),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    for succeeded in [false, true] {
        if succeeded {
            h.dispatch().await;
            finish_item(&h, id).await;
            h.dispatch().await;
            assert!(matches!(h.state(id).await, QueueState::Succeeded { .. }));
        }
        let update = Request::UpdateItem {
            item: id.clone(),
            patch: ItemPatch {
                autonomy: Some(auto),
                ..Default::default()
            },
        };
        let steer = Request::SteerItem {
            item: id.clone(),
            steer: Steer {
                autonomy: Some(auto),
                ..Default::default()
            },
        };
        for request in [update, steer] {
            let error = h.orchestrator(request).await.unwrap_err();
            assert!(
                error.to_string().contains("merged by hand, together"),
                "{error}"
            );
        }
        let error = archon_daemon::handlers::update_item_autonomy_above_ceiling(&h.ctx, id, auto)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("merged by hand, together"),
            "{error}"
        );
        assert_eq!(h.item(id).await.autonomy.merge, MergeMode::Manual);
    }
}

/// A Done initiative whose member runs again (retried by the operator) must
/// neither stop the daemon when the member ships nor on a later maintenance pass.
#[tokio::test]
async fn a_done_initiative_with_a_rerun_member_ships_and_reconciles_without_panicking() {
    let h = setup().await;
    let detail = enqueue(&h, request()).await;
    h.dispatch().await;
    for member in &detail.members {
        finish_item(&h, &member.id).await;
    }
    h.dispatch().await;
    let state = || async {
        h.ctx
            .store
            .lock()
            .await
            .initiative(&detail.initiative.id)
            .unwrap()
            .initiative
            .state
    };
    assert_eq!(state().await, InitiativeState::Done);
    let again = &detail.members[0].id;
    h.cli(Request::RetryItem {
        item: again.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    })
    .await
    .unwrap();
    assert_eq!(
        state().await,
        InitiativeState::Open,
        "retrying a member reopens its initiative"
    );
    h.dispatch().await;
    assert!(!matches!(
        h.state(again).await,
        QueueState::Succeeded { .. }
    ));
    archon_daemon::executor::coordination::reconcile(&h.ctx)
        .await
        .unwrap();
    assert!(
        prs(&h)
            .as_object()
            .unwrap()
            .values()
            .all(|p| p["isDraft"] == true)
    );
    finish_rerun(&h, again).await;
    archon_daemon::executor::coordination::reconcile(&h.ctx)
        .await
        .unwrap();
    assert!(matches!(h.state(again).await, QueueState::Succeeded { .. }));
    assert!(
        prs(&h)
            .as_object()
            .unwrap()
            .values()
            .all(|p| p["isDraft"] == false)
    );
    assert_eq!(state().await, InitiativeState::Done);
    let other = &detail.members[1].id;
    assert!(matches!(h.state(other).await, QueueState::Succeeded { .. }));
}

async fn finish_rerun(h: &Harness, item: &archon_core::id::QueueItemId) {
    for _ in 0..5 {
        let run = h
            .ctx
            .store
            .lock()
            .await
            .runs_for_item(item)
            .unwrap()
            .pop()
            .unwrap();
        for stage in h.running().await.iter().filter(|s| s.run == run.id) {
            let outcome = if stage.stage_id == "review" {
                common::verdict(true)
            } else {
                common::completed("passed")
            };
            archon_daemon::executor::stage_settled(
                &h.ctx,
                stage.aether_agent.as_ref().unwrap(),
                outcome,
            )
            .await
            .unwrap();
        }
        h.dispatch().await;
        if matches!(h.state(item).await, QueueState::Succeeded { .. }) {
            return;
        }
    }
    panic!("rerun did not succeed: {:?}", h.state(item).await);
}

async fn retry(h: &Harness, item: &archon_core::id::QueueItemId) {
    h.cli(Request::RetryItem {
        item: item.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    })
    .await
    .unwrap();
}

async fn initiative_state(h: &Harness, detail: &InitiativeDetail) -> InitiativeState {
    let store = h.ctx.store.lock().await;
    store
        .initiative(&detail.initiative.id)
        .unwrap()
        .initiative
        .state
}

/// A done initiative whose members run again follows them: it reopens, ends
/// failed when one fails, and never stops the daemon or unrelated work.
#[tokio::test]
async fn a_done_initiative_follows_its_retried_members_and_leaves_other_work_running() {
    let h = setup().await;
    let path = common::init_repo_with_origin(&h.dir.path().join("other"));
    h.cli(Request::RepoAdd(RepoAdd {
        path,
        name: Some("other".into()),
        ship: Some(ShipMode::Pr),
        base_branch: Some("main".into()),
        default_pipeline: Some(PIPELINE.into()),
        auto_merge: false,
        github_watch: false,
    }))
    .await
    .unwrap();
    let detail = enqueue(&h, request()).await;
    h.dispatch().await;
    for member in &detail.members {
        finish_item(&h, &member.id).await;
    }
    h.dispatch().await;
    assert_eq!(initiative_state(&h, &detail).await, InitiativeState::Done);

    let unrelated = h
        .enqueue_in_repo("other", PIPELINE, "Unrelated", plan())
        .await
        .unwrap();
    h.dispatch().await;

    let (ships, fails) = (&detail.members[0].id, &detail.members[1].id);
    retry(&h, ships).await;
    retry(&h, fails).await;
    assert_eq!(initiative_state(&h, &detail).await, InitiativeState::Open);
    h.dispatch().await;
    let run = h
        .ctx
        .store
        .lock()
        .await
        .runs_for_item(fails)
        .unwrap()
        .pop()
        .unwrap();
    for stage in h.running().await.iter().filter(|s| s.run == run.id) {
        let outcome = archon_core::run::StageOutcome::Failed {
            error: "cannot implement contract".into(),
            infra: false,
        };
        archon_daemon::executor::stage_settled(
            &h.ctx,
            stage.aether_agent.as_ref().unwrap(),
            outcome,
        )
        .await
        .unwrap();
    }
    finish_rerun(&h, ships).await;
    archon_daemon::executor::coordination::reconcile(&h.ctx)
        .await
        .unwrap();

    assert!(matches!(h.state(ships).await, QueueState::Succeeded { .. }));
    assert!(matches!(h.state(fails).await, QueueState::Failed { .. }));
    assert_eq!(initiative_state(&h, &detail).await, InitiativeState::Failed);
    assert!(
        h.state(&unrelated.id).await.is_active(),
        "unrelated work in another repository keeps running"
    );
}

/// A member of a cancelled initiative is not retried alone: maintenance
/// would cancel it again.
#[tokio::test]
async fn a_member_of_a_cancelled_initiative_is_retried_through_the_initiative() {
    let h = setup().await;
    let detail = enqueue(&h, request()).await;
    h.dispatch().await;
    h.cli(Request::CancelInitiative {
        initiative: detail.initiative.id.clone(),
    })
    .await
    .unwrap();
    let refused = h
        .cli(Request::RetryItem {
            item: detail.members[0].id.clone(),
            model_plan: None,
            model_reasons: None,
            fresh: false,
            brief_append: None,
            stage_timeouts: Default::default(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("archon initiatives retry"), "{refused}");
}
