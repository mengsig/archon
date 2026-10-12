//! Request handlers: operator-only requests refused to MCP clients, enqueue
//! validation and model policy, item updates and retries, repository checks.

mod common;

use archon_core::autonomy::{AutonomyPatch, MergeMode};
use archon_core::config::Config;
use archon_core::cost::{CostGroup, CostQuery, CostReport, DayBound};
use archon_core::error::ArchonError;
use archon_core::github::{GithubThread, OperatorUsers, WatcherChange};
use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{
    ClientKind, EnqueueRequest, ErrorCode, ErrorReply, ItemPatch, ItemQuery, PauseTarget, Reply,
    RepoAdd, RepoPatch, RepoRef, Request,
};
use archon_core::queue::{Caps, QueueState};
use archon_daemon::handlers;
use archon_daemon::triggers::{self, TriggerEvent};
use archon_queue::store::SpendRow;
use common::{Harness, MODEL, REPO_NAME, REVIEW_MODEL, choice, plan};

fn repo_ref() -> RepoRef {
    RepoRef(REPO_NAME.into())
}

async fn item_count(h: &Harness) -> usize {
    match h.cli(Request::ListItems(ItemQuery::default())).await {
        Ok(Reply::Items(items)) => items.len(),
        other => panic!("list items: {other:?}"),
    }
}

#[tokio::test]
async fn mcp_clients_are_refused_operator_only_requests() {
    let h = Harness::new().await;
    let operator_only = [
        Request::Shutdown,
        Request::RepoRemove { repo: repo_ref() },
        Request::RepoUpdate {
            repo: repo_ref(),
            patch: RepoPatch {
                autonomy_max: Some(AutonomyPatch {
                    merge: Some(MergeMode::Auto),
                    ..AutonomyPatch::default()
                }),
                ..RepoPatch::default()
            },
        },
        Request::SetCaps {
            caps: Caps::default(),
        },
        Request::Say { text: "hi".into() },
        Request::ConfigureGithubWatcher {
            change: WatcherChange::Operators {
                operator_users: Some(OperatorUsers::parse(&["octocat"]).unwrap()),
            },
        },
        Request::ConfigureGithubWatcher {
            change: WatcherChange::Operators {
                operator_users: None,
            },
        },
    ];
    for request in operator_only {
        assert!(request.is_operator_only(), "{request:?}");
        let refused = handlers::handle(&h.ctx, ClientKind::ArchonMcp, request)
            .await
            .expect_err("refused");
        assert!(matches!(refused, ArchonError::Forbidden(_)), "{refused:?}");
        assert_eq!(ErrorReply::from(&refused).code, ErrorCode::Forbidden);
    }
    let repos = handlers::handle(&h.ctx, ClientKind::ArchonMcp, Request::RepoList).await;
    let Ok(Reply::Repos(repos)) = repos else {
        panic!("MCP may list repositories: {repos:?}");
    };
    assert_eq!(repos.len(), 1, "the refused remove changed nothing");
    assert!(
        repos[0].autonomy_max.is_empty(),
        "the refused update changed nothing"
    );
    assert!(h.ctx.caps_override.lock().await.is_none());
    let status = handlers::handle(&h.ctx, ClientKind::ArchonMcp, Request::Status).await;
    assert!(matches!(status, Ok(Reply::Status(_))), "{status:?}");
}

#[tokio::test]
async fn enqueue_refuses_models_outside_the_repository_policy() {
    let h = Harness::new().await;
    h.repo_config(&format!("[models]\ndeny = [\"{REVIEW_MODEL}\"]\n"));
    let refused = h
        .enqueue_with("denied reviewer", plan())
        .await
        .expect_err("denied");
    assert!(matches!(refused, ArchonError::Policy(_)), "{refused:?}");

    h.repo_config("[models]\nmax_effort = \"medium\"\n");
    let refused = h
        .enqueue_with("too much effort", plan())
        .await
        .expect_err("capped");
    assert!(matches!(refused, ArchonError::Policy(_)), "{refused:?}");
    assert_eq!(item_count(&h).await, 0);

    let mut modest = plan();
    modest
        .0
        .insert("implement".into(), vec![choice(MODEL, Effort::Medium)]);
    h.enqueue_with("within policy", modest)
        .await
        .expect("allowed");
    assert_eq!(item_count(&h).await, 1);
}

#[tokio::test]
async fn enqueue_refuses_models_outside_the_global_allowlist() {
    let mut config = Config::default();
    config.models.allow = vec![MODEL.parse().expect("model")];
    let h = Harness::with_config(config).await;
    let refused = h
        .enqueue_with("codex not allowed", plan())
        .await
        .expect_err("denied");
    assert!(matches!(refused, ArchonError::Policy(_)), "{refused:?}");
    let reply = h
        .cli(Request::ModelPolicy { repo: None })
        .await
        .expect("policy");
    assert!(matches!(reply, Reply::ModelPolicy(_)));
    assert_eq!(item_count(&h).await, 0);
}

#[tokio::test]
async fn enqueue_validates_the_plan_against_the_pipeline() {
    let h = Harness::new().await;
    let mut missing = plan();
    missing.0.remove("ship");
    let mut too_many = plan();
    too_many
        .0
        .insert("implement".into(), vec![choice(MODEL, Effort::High); 2]);
    let mut unknown = plan();
    unknown
        .0
        .insert("deploy".into(), vec![choice(MODEL, Effort::Low)]);
    for (why, bad) in [
        ("missing", missing),
        ("too many", too_many),
        ("unknown", unknown),
    ] {
        let refused = h.enqueue_with(why, bad).await.expect_err(why);
        assert!(
            matches!(refused, ArchonError::Invalid(_)),
            "{why}: {refused:?}"
        );
    }
    let refused = h
        .enqueue_with("   ", plan())
        .await
        .expect_err("empty title");
    assert!(matches!(refused, ArchonError::Invalid(_)));
    let refused = h
        .enqueue_with("no plan", ModelPlan::default())
        .await
        .expect_err("empty plan");
    assert!(matches!(refused, ArchonError::Invalid(_)));
    assert_eq!(item_count(&h).await, 0);
}

#[tokio::test]
async fn model_plan_changes_wait_for_a_retry_which_keeps_the_worktree() {
    let h = Harness::new().await;
    let item = h.enqueue("change of plan").await;
    let mut cheaper = plan();
    cheaper
        .0
        .insert("implement".into(), vec![choice(MODEL, Effort::Low)]);
    let patch = |plan: &ModelPlan| ItemPatch {
        model_plan: Some(plan.clone()),
        model_reasons: None,
        ..ItemPatch::default()
    };
    let update = |p| Request::UpdateItem {
        item: item.id.clone(),
        patch: p,
    };
    h.cli(update(patch(&cheaper)))
        .await
        .expect("queued items take a new plan");
    h.dispatch().await;
    let refused = h.cli(update(patch(&plan()))).await.expect_err("running");
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused:?}"
    );
    assert_eq!(h.backend.requests()[0].choice, choice(MODEL, Effort::Low));

    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
    };
    let refused = h
        .cli(retry.clone())
        .await
        .expect_err("only failed or cancelled");
    assert!(matches!(refused, ArchonError::InvalidTransition(_)));
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    let first_agent = h.running().await[0].aether_agent.clone().expect("agent");
    let workspace = h.backend.requests()[0].workspace.clone();
    std::fs::write(workspace.join("draft.txt"), "prior work\n").expect("draft");
    h.cli(cancel).await.expect("cancel");
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: Some(plan()),
        model_reasons: None,
        fresh: false,
        brief_append: None,
    };
    h.cli(retry).await.expect("retry");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    h.dispatch().await;
    let last = h.backend.requests().pop().expect("a new implementer");
    assert_eq!(
        last.choice,
        choice(MODEL, Effort::High),
        "the retry uses the new plan"
    );
    assert_eq!(
        last.workspace, workspace,
        "a retried item keeps its worktree"
    );
    let draft = std::fs::read_to_string(workspace.join("draft.txt")).expect("kept");
    assert_eq!(draft, "prior work\n", "and the prior attempt's work in it");
    let first_node = h.backend.node_ids()[0].clone();
    assert_eq!(
        h.backend.cancelled(),
        vec![first_agent, first_node],
        "the first implementer and its run node were stopped"
    );
}

#[tokio::test]
async fn repositories_are_checked_when_added() {
    let h = Harness::new().await;
    let add = |path: std::path::PathBuf, base: &str| RepoAdd {
        path,
        name: Some("other".into()),
        ship: None,
        base_branch: Some(base.into()),
        default_pipeline: None,
        auto_merge: false,
    };
    let relative = h.cli(Request::RepoAdd(add("repo".into(), "main"))).await;
    assert!(
        matches!(relative, Err(ArchonError::Invalid(_))),
        "{relative:?}"
    );
    let plain = h.dir.path().canonicalize().expect("dir").join("plain");
    std::fs::create_dir_all(&plain).expect("plain dir");
    let not_git = h.cli(Request::RepoAdd(add(plain, "main"))).await;
    assert!(not_git.is_err(), "a plain directory is not a repository");
    let no_base = h
        .cli(Request::RepoAdd(add(h.repo.clone(), "develop")))
        .await;
    assert!(no_base.is_err(), "the base branch must exist");
    let Ok(Reply::Repos(repos)) = h.cli(Request::RepoList).await else {
        panic!("list");
    };
    assert_eq!(repos.len(), 1);

    let item = h.enqueue("keeps the repo").await;
    let removal = h.cli(Request::RepoRemove { repo: repo_ref() }).await;
    assert!(removal.is_err(), "a repository with live items stays");
    h.cli(Request::CancelItem {
        item: item.id,
        reason: None,
    })
    .await
    .expect("cancel");
    h.cli(Request::RepoRemove { repo: repo_ref() })
        .await
        .expect("removed");
}

#[tokio::test]
async fn pausing_an_unknown_repository_is_an_error() {
    let h = Harness::new().await;
    let target = PauseTarget::Repo {
        name: RepoRef("nope".into()),
    };
    let refused = h
        .cli(Request::Pause(target))
        .await
        .expect_err("unknown repo");
    assert!(matches!(refused, ArchonError::NotFound(_)), "{refused:?}");
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
        panic!("status");
    };
    assert!(!status.pause.global && status.pause.repos.is_empty());
}

#[tokio::test]
async fn a_hard_pause_without_aether_still_pauses_dispatch_and_says_so() {
    let h = Harness::new().await;
    let hard = Request::Pause(PauseTarget::Global { hard: true });
    h.cli(hard.clone())
        .await
        .expect("no running agent needs Aether");
    h.cli(Request::Resume(PauseTarget::Global { hard: false }))
        .await
        .expect("nothing to release");

    h.enqueue("running").await;
    h.dispatch().await;
    let running = h.running().await.len();
    assert!(running > 0, "a stage agent runs");
    let error = h.cli(hard).await.expect_err("Aether is not connected");
    let text = error.to_string();
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error:?}");
    assert!(
        text.contains("dispatch is paused")
            && text.contains(&format!("{running} of {running} running stage agent(s)")),
        "{text}"
    );
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
        panic!("status");
    };
    assert!(status.pause.global, "the soft part of the pause holds");
}

#[tokio::test]
async fn pausing_an_unknown_pipeline_is_refused_with_the_known_ones() {
    let h = Harness::new().await;
    let unknown = PauseTarget::Pipeline {
        name: "no-such-pipeline".into(),
    };
    let error = h
        .cli(Request::Pause(unknown.clone()))
        .await
        .expect_err("unknown pipeline");
    let text = error.to_string();
    assert!(matches!(error, ArchonError::Invalid(_)), "{error:?}");
    assert!(
        text.contains("\"no-such-pipeline\"")
            && text.contains("quick-pr")
            && text.contains(common::PIPELINE),
        "{text}"
    );
    h.cli(Request::Resume(unknown))
        .await
        .expect("resuming a stale name stays possible");
    let known = PauseTarget::Pipeline {
        name: common::PIPELINE.into(),
    };
    h.cli(Request::Pause(known))
        .await
        .expect("a repository pipeline");
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
        panic!("status");
    };
    assert_eq!(status.pause.pipelines, vec![common::PIPELINE.to_owned()]);
}

#[tokio::test]
async fn mcp_clients_cannot_lift_the_operators_hard_pause() {
    let h = Harness::new().await;
    h.cli(Request::Pause(PauseTarget::Global { hard: true }))
        .await
        .expect("no running agent to pause in Aether");
    let resume = Request::Resume(PauseTarget::Global { hard: false });
    let refused = handlers::handle(&h.ctx, ClientKind::ArchonMcp, resume.clone())
        .await
        .expect_err("MCP cannot lift a hard pause");
    assert!(matches!(refused, ArchonError::Forbidden(_)), "{refused:?}");
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
        panic!("status");
    };
    assert!(status.pause.global, "the hard pause holds");
    h.cli(resume).await.expect("the operator lifts it");
    let soft = Request::Pause(PauseTarget::Global { hard: false });
    handlers::handle(&h.ctx, ClientKind::ArchonMcp, soft)
        .await
        .expect("soft pause");
    let resume = Request::Resume(PauseTarget::Global { hard: false });
    handlers::handle(&h.ctx, ClientKind::ArchonMcp, resume)
        .await
        .expect("MCP lifts a soft pause");
}

#[tokio::test]
async fn item_detail_reports_each_stage_agents_ledger_cost() {
    let h = Harness::new().await;
    let item = h.enqueue("costed").await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "the first stage is running");
    let agent = running[0]
        .aether_agent
        .clone()
        .expect("running has an agent");
    let row = SpendRow {
        agent,
        item: Some(item.id.clone()),
        cost_usd: 0.25,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        estimated_cost_usd: 0.5,
        input_tokens: 10,
        output_tokens: 5,
        at: archon_core::now(),
    };
    h.ctx.store.lock().await.record_spend(&row).expect("spend");
    let Ok(Reply::ItemDetail(detail)) = h.cli(Request::GetItem { item: item.id }).await else {
        panic!("get item");
    };
    assert_eq!(detail.stages.len(), 1);
    assert_eq!(
        detail.stages[0].cost_usd, 0.75,
        "estimates count by default"
    );
    assert_eq!(detail.spent_usd, 0.75);
}

async fn cost_report(h: &Harness, query: CostQuery) -> Result<CostReport, ArchonError> {
    let request = Request::CostReport(query);
    match handlers::handle(&h.ctx, ClientKind::ArchonMcp, request).await? {
        Reply::CostReport(report) => Ok(*report),
        other => panic!("cost report: {other:?}"),
    }
}

#[tokio::test]
async fn cost_reports_count_the_ledger_as_the_budgets_do() {
    for estimates in [true, false] {
        let mut config = Config::default();
        config.limits.count_estimated_cost = estimates;
        let h = Harness::with_config(config).await;
        let item = h.enqueue("costed").await;
        h.dispatch().await;
        let agent = h.running().await[0].aether_agent.clone().expect("agent");
        let row = SpendRow {
            agent,
            item: Some(item.id.clone()),
            cost_usd: 0.25,
            cost_basis: archon_core::cost::CostBasis::ProviderReported,
            estimated_cost_usd: 0.5,
            input_tokens: 10,
            output_tokens: 5,
            at: archon_core::now(),
        };
        h.ctx.store.lock().await.record_spend(&row).expect("spend");
        let counted = if estimates { 0.75 } else { 0.25 };
        for (by, key) in [(CostGroup::Repo, REPO_NAME), (CostGroup::Model, MODEL)] {
            let query = CostQuery {
                by,
                ..CostQuery::default()
            };
            let report = cost_report(&h, query).await.expect("an MCP client may ask");
            assert_eq!(report.rows.len(), 1, "{report:?}");
            assert_eq!(report.rows[0].key.as_deref(), Some(key));
            assert_eq!(report.total.counted_usd, counted, "estimates {estimates}");
            assert_eq!(report.estimates_counted, estimates);
        }
        let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
            panic!("status");
        };
        assert_eq!(status.spend_today_usd, counted, "the daily budget agrees");
    }
}

#[tokio::test]
async fn a_backwards_cost_range_is_an_invalid_argument() {
    let h = Harness::new().await;
    let query = CostQuery {
        since: Some(DayBound::parse("2026-09-10").unwrap()),
        until: Some(DayBound::parse("2026-09-01").unwrap()),
        by: CostGroup::Day,
    };
    let refused = cost_report(&h, query).await.unwrap_err();
    assert_eq!(ErrorReply::from(&refused).code, ErrorCode::InvalidArgument);
    assert!(refused.to_string().contains("after it ends"), "{refused}");
}

#[tokio::test]
async fn an_item_asked_for_on_github_reports_back_to_its_thread() {
    let h = Harness::new().await;
    let origin: GithubThread =
        serde_json::from_value(serde_json::json!({"repository": "Owner/Repo", "number": 12}))
            .unwrap();
    let request = EnqueueRequest {
        repo: Some(repo_ref()),
        pipeline: None,
        title: "fix the flaky test".into(),
        brief: "asked for on GitHub".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: Some(origin.clone()),
    };
    let reply = handlers::handle(&h.ctx, ClientKind::ArchonMcp, Request::Enqueue(request)).await;
    let Ok(Reply::Item(item)) = reply else {
        panic!("enqueue replied {reply:?}");
    };
    assert_eq!(item.origin, Some(origin.clone()));
    assert_eq!(h.item(&item.id).await.origin, Some(origin.clone()));
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("answered on the thread instead".into()),
    };
    h.cli(cancel).await.expect("cancel");
    let news = triggers::held(&h.ctx).await.unwrap();
    let finished = news.iter().find_map(|event| match event {
        TriggerEvent::ItemFinished {
            item: id, origin, ..
        } if *id == item.id => Some(origin.clone()),
        _ => None,
    });
    assert_eq!(finished, Some(Some(origin)), "the outcome names its thread");
}

#[tokio::test]
async fn only_open_blocking_questions_for_live_stage_agents_derive_a_wait() {
    use archon_core::aether_ids::AetherQuestionRef;
    use archon_core::id::QuestionId;
    use archon_core::notify::Question;
    let h = Harness::new().await;
    let item = h.enqueue("needs input").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let mut question = Question {
        id: QuestionId::new(),
        number: 0,
        text: "Which greeting?".into(),
        options: vec![],
        item: Some(item.id.clone()),
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    h.ctx.store.lock().await.insert_question(&question).unwrap();
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    assert!(
        detail.waiting_on.is_none(),
        "advisory questions do not pause the stage"
    );
    question.id = QuestionId::new();
    question.aether = Some(AetherQuestionRef {
        agent: stage.aether_agent.clone().unwrap(),
        seq: 1,
    });
    h.ctx.store.lock().await.insert_question(&question).unwrap();
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    assert_eq!(
        detail.waiting_reason.as_ref().unwrap().code,
        archon_core::protocol::WaitingReasonCode::OperatorGate
    );
    h.cli(Request::Pause(archon_core::protocol::PauseTarget::Global {
        hard: false,
    }))
    .await
    .unwrap();
    let Reply::ItemDetail(paused) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    assert_eq!(
        paused.waiting_reason, detail.waiting_reason,
        "operator answer outranks global holds"
    );
    h.cli(Request::Resume(
        archon_core::protocol::PauseTarget::Global { hard: false },
    ))
    .await
    .unwrap();
    let wait = detail.waiting_on.unwrap();
    assert_eq!(wait.subject, "question");
    assert_eq!(wait.prompt, "Which greeting?");
    assert_eq!(
        wait.next_action,
        format!("archon answer {} <answer>", question.id)
    );
    let Reply::ItemSummaries(rows) = h
        .cli(Request::ListItemSummaries(Default::default()))
        .await
        .unwrap()
    else {
        panic!("summaries")
    };
    assert_eq!(rows[0].waiting_on.as_ref(), Some(&wait));
    h.ctx
        .store
        .lock()
        .await
        .answer_question(&question.id, "hello", archon_core::now())
        .unwrap();
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    assert!(detail.waiting_on.is_none());
    question.id = QuestionId::new();
    question.aether.as_mut().unwrap().seq = 2;
    h.ctx.store.lock().await.insert_question(&question).unwrap();
    h.settle_running(|_| common::completed("implemented")).await;
    let Reply::ItemDetail(detail) = h.cli(Request::GetItem { item: item.id }).await.unwrap() else {
        panic!("detail")
    };
    assert!(
        detail.waiting_on.is_none(),
        "a settled agent's stale question cannot mark a new stage waiting"
    );
}

#[tokio::test]
async fn mcp_enqueue_and_updates_refuse_invalid_queue_inputs() {
    let h = Harness::new().await;
    let base = EnqueueRequest {
        repo: Some(repo_ref()),
        pipeline: Some(common::PIPELINE.into()),
        title: "valid".into(),
        brief: "valid brief".into(),
        priority: Default::default(),
        depends_on: vec![],
        tags: vec![],
        model_plan: plan(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    };
    let mut invalid = Vec::new();
    for budget in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        invalid.push(EnqueueRequest {
            budget_usd: Some(budget),
            ..base.clone()
        });
    }
    for brief in ["", " \n\t"] {
        invalid.push(EnqueueRequest {
            brief: brief.into(),
            ..base.clone()
        });
    }
    for title in [
        "two\nlines",
        "two\rlines",
        "title\n",
        "two\u{2028}lines",
        "two\u{2029}lines",
    ] {
        invalid.push(EnqueueRequest {
            title: title.into(),
            ..base.clone()
        });
    }
    for tag in ["", "   "] {
        invalid.push(EnqueueRequest {
            tags: vec![tag.into()],
            ..base.clone()
        });
    }
    for request in invalid {
        let error = handlers::handle(&h.ctx, ClientKind::ArchonMcp, Request::Enqueue(request))
            .await
            .unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)), "{error}");
    }
    assert_eq!(item_count(&h).await, 0);
    let Reply::Item(item) = h
        .cli(Request::Enqueue(EnqueueRequest {
            budget_usd: Some(0.001),
            ..base
        }))
        .await
        .unwrap()
    else {
        panic!("item");
    };
    for patch in [
        ItemPatch {
            budget_usd: Some(0.0),
            ..Default::default()
        },
        ItemPatch {
            tags: Some(vec!["".into()]),
            ..Default::default()
        },
    ] {
        let error = h
            .cli(Request::UpdateItem {
                item: item.id.clone(),
                patch,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)));
    }
    assert_eq!(h.item(&item.id).await.budget_usd, Some(0.001));
}
