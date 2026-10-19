//! Request handlers: operator-only requests refused to agents, enqueue
//! validation and model policy, item updates and retries, repository checks.

use crate::common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::autonomy::{AutonomyPatch, MergeMode};
use archon_core::config::Config;
use archon_core::cost::{CostGroup, CostQuery, CostReport, DayBound};
use archon_core::error::ArchonError;
use archon_core::github::{GithubThread, OperatorUsers, WatcherChange};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::protocol::{
    ClientKind, EnqueueRequest, ErrorCode, ErrorReply, ItemPatch, ItemQuery, PauseTarget, Reply,
    RepoAdd, RepoPatch, RepoRef, Request,
};
use archon_core::queue::{Caps, QueueState};
use archon_core::repo::ShipMode;
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

/// A budget set on an item is acknowledged as unenforced only while budgets
/// are off; the item keeps it either way.
#[tokio::test]
async fn setting_a_budget_says_whether_it_is_enforced() {
    let h = Harness::new().await;
    let item = h.enqueue("budgeted").await;
    let patch = |usd| Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            budget_usd: Some(usd),
            ..ItemPatch::default()
        },
    };
    let Reply::Item(_) = h.cli(patch(2.0)).await.unwrap() else {
        panic!("budgets are on by default")
    };
    let off = Caps {
        budgets_enabled: false,
        ..Caps::default()
    };
    h.cli(Request::SetCaps { caps: off }).await.unwrap();
    let Reply::ItemBudgetsOff(stored) = h.cli(patch(3.0)).await.unwrap() else {
        panic!("a budget set while budgets are off is flagged")
    };
    assert_eq!(stored.budget_usd, Some(3.0));
    let quiet = Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            priority: Some(archon_core::queue::Priority::High),
            ..ItemPatch::default()
        },
    };
    assert!(matches!(h.cli(quiet).await.unwrap(), Reply::Item(_)));
}

/// A request of each operator-only kind.
fn operator_only_requests() -> Vec<Request> {
    vec![
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
    ]
}

#[tokio::test]
async fn agents_are_refused_operator_only_requests_through_any_client() {
    let h = Harness::new().await;
    h.orchestrator(Request::Status(Default::default()))
        .await
        .unwrap();
    for request in operator_only_requests() {
        assert!(request.is_operator_only(), "{request:?}");
        let forwarded = Request::ForAgent {
            caller: AetherAgentId::parse(common::ORCHESTRATOR).unwrap(),
            request: Box::new(request.clone()),
        };
        // An agent's request is refused whichever client carries it.
        for (client, request) in [
            (ClientKind::ArchonMcp, request),
            (ClientKind::ArchonCli, forwarded.clone()),
            (ClientKind::ArchonBench, forwarded),
        ] {
            let refused = handlers::handle(&h.ctx, client, request)
                .await
                .expect_err("refused");
            assert!(matches!(refused, ArchonError::Forbidden(_)), "{refused:?}");
            assert_eq!(ErrorReply::from(&refused).code, ErrorCode::Forbidden);
        }
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
    assert!(h.ctx.operator_settings.lock().unwrap().global.is_empty());
    let status = handlers::handle(
        &h.ctx,
        ClientKind::ArchonMcp,
        Request::Status(Default::default()),
    )
    .await;
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

    h.repo_config("[models]\nmax_effort = \"low\"\n");
    let refused = h
        .enqueue_with("too much effort", plan())
        .await
        .expect_err("capped");
    assert!(matches!(refused, ArchonError::Policy(_)), "{refused:?}");
    assert_eq!(item_count(&h).await, 0);

    let mut modest = plan();
    modest
        .0
        .insert("implement".into(), vec![choice(MODEL, Effort::Low)]);
    modest
        .0
        .insert("review".into(), vec![choice(MODEL, Effort::Low)]);
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
    let mut too_many = plan();
    too_many
        .0
        .insert("implement".into(), vec![choice(MODEL, Effort::Medium); 2]);
    let mut unknown = plan();
    unknown
        .0
        .insert("deploy".into(), vec![choice(MODEL, Effort::Low)]);
    for (why, bad) in [("too many", too_many), ("unknown", unknown)] {
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
    assert_eq!(item_count(&h).await, 0);
}

/// A plan names the stages whose model the caller chooses: the others get
/// the orchestrator's model at the effort the pipeline's tier suggests.
#[tokio::test]
async fn enqueue_fills_the_stages_a_plan_leaves_out() {
    let h = Harness::new().await;
    let default = vec![ModelChoice {
        model: h.ctx.config.orchestrator.model.clone(),
        effort: Effort::Medium,
        tier: None,
    }];
    let mut partial = plan();
    partial.0.remove("ship");
    let item = h.enqueue_with("a partial plan", partial).await.unwrap();
    assert_eq!(item.model_plan.0["implement"], plan().0["implement"]);
    assert_eq!(item.model_plan.0["review"], plan().0["review"]);
    assert_eq!(item.model_plan.0["ship"], default);

    let unplanned = h
        .enqueue_with("no plan at all", ModelPlan::default())
        .await
        .unwrap();
    let stages: Vec<&str> = unplanned.model_plan.0.keys().map(String::as_str).collect();
    assert_eq!(stages, ["implement", "review", "ship"]);
    assert!(unplanned.model_plan.0.values().all(|c| *c == default));
    // `performance` asks for the strongest model: its stages run at high effort.
    let performance = h
        .enqueue_in("performance", "a faster path", ModelPlan::default())
        .await
        .unwrap();
    assert_eq!(performance.model_plan.0["measure"][0].effort, Effort::High);
}

/// A default is a choice like any other: the model policy still decides.
#[tokio::test]
async fn a_filled_plan_stays_within_the_model_policy() {
    let mut config = Config::default();
    config.models.allow = vec![REVIEW_MODEL.parse().unwrap()];
    config.models.max_effort = Some(Effort::Low);
    let h = Harness::with_config(config).await;
    let item = h
        .enqueue_with("no plan at all", ModelPlan::default())
        .await
        .expect("the one allowed model fills the plan");
    for choices in item.model_plan.0.values() {
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].model.to_string(), REVIEW_MODEL);
        assert_eq!(choices[0].effort, Effort::Low, "capped by the policy");
    }
    let mut over = ModelPlan::default();
    over.0
        .insert("implement".into(), vec![choice(MODEL, Effort::Low)]);
    let refused = h.enqueue_with("a denied model", over).await.unwrap_err();
    assert!(matches!(refused, ArchonError::Policy(_)), "{refused:?}");
}

/// `plan()` with the implementer at low effort.
fn cheaper_plan() -> ModelPlan {
    let mut cheaper = plan();
    let low = vec![choice(MODEL, Effort::Low)];
    cheaper.0.insert("implement".into(), low);
    cheaper
}

#[tokio::test]
async fn a_running_item_is_steered_onto_a_new_model_plan() {
    let h = Harness::new().await;
    let item = h.enqueue("change of plan").await;
    let update = |plan: ModelPlan| Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            model_plan: Some(plan.into()),
            ..ItemPatch::default()
        },
    };
    h.cli(update(cheaper_plan()))
        .await
        .expect("queued items take a new plan");
    h.dispatch().await;
    h.cli(update(plan()))
        .await
        .expect("a running item is steered onto its new plan");
    assert_eq!(h.backend.requests()[0].choice, choice(MODEL, Effort::Low));
    let implementer = h.running().await[0].aether_agent.clone().expect("agent");
    let retuned: Vec<_> = h
        .backend
        .updates()
        .into_iter()
        .filter_map(|(agent, update)| update.choice.map(|choice| (agent, choice)))
        .collect();
    assert_eq!(retuned, vec![(implementer, choice(MODEL, Effort::Medium))]);
    assert_eq!(h.item(&item.id).await.model_plan, plan());
    assert!(h.backend.cancelled().is_empty(), "nothing was cancelled");
}

#[tokio::test]
async fn a_retry_takes_a_new_plan_and_keeps_the_worktree() {
    let h = Harness::new().await;
    let item = h.enqueue("second try").await;
    h.dispatch().await;
    let retry = |plan: Option<ModelPlan>| Request::RetryItem {
        item: item.id.clone(),
        model_plan: plan.map(Into::into),
        model_reasons: None,
        fresh: false,
        brief_append: None,
    };
    let refused = h.cli(retry(None)).await.expect_err("only a finished item");
    assert!(matches!(refused, ArchonError::InvalidTransition(_)));
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    let first_agent = h.running().await[0].aether_agent.clone().expect("agent");
    let workspace = h.backend.requests()[0].workspace.clone();
    std::fs::write(workspace.join("draft.txt"), "prior work\n").expect("draft");
    h.cli(cancel).await.expect("cancel");
    h.cli(retry(Some(cheaper_plan()))).await.expect("retry");
    assert_eq!(h.state(&item.id).await, QueueState::Queued);
    h.dispatch().await;
    let last = h.backend.requests().pop().expect("a new implementer");
    let low = choice(MODEL, Effort::Low);
    assert_eq!(last.choice, low, "the retry uses the new plan");
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

fn init_repo(h: &Harness, name: &str, commit: bool) -> std::path::PathBuf {
    let path = h.dir.path().canonicalize().expect("dir").join(name);
    std::fs::create_dir_all(&path).expect("repo dir");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q", "-b", "trunk"]);
    if commit {
        git(&["commit", "-q", "--allow-empty", "-m", "first"]);
    }
    path
}

fn add_without_base(path: std::path::PathBuf, name: &str, ship: Option<ShipMode>) -> Request {
    Request::RepoAdd(RepoAdd {
        path,
        name: Some(name.into()),
        ship,
        base_branch: None,
        default_pipeline: None,
        auto_merge: false,
        github_watch: false,
    })
}

#[tokio::test]
async fn a_repository_added_without_a_base_records_its_current_branch() {
    let h = Harness::new().await;
    let path = init_repo(&h, "fresh", true);
    let added = h
        .cli(add_without_base(path, "fresh", Some(ShipMode::Local)))
        .await;
    let Ok(Reply::Repo(repo)) = added else {
        panic!("add: {added:?}");
    };
    assert_eq!(repo.base_branch.as_deref(), Some("trunk"));
}

#[tokio::test]
async fn a_repository_without_commits_or_on_a_detached_head_is_refused() {
    let h = Harness::new().await;
    let empty = init_repo(&h, "empty", false);
    let refused = h
        .cli(add_without_base(empty, "empty", Some(ShipMode::Local)))
        .await
        .expect_err("no commits");
    assert!(refused.to_string().contains("no commits"), "{refused}");

    let detached = init_repo(&h, "detached", true);
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&detached)
        .args(["checkout", "-q", "--detach"])
        .status()
        .expect("git");
    assert!(status.success());
    let refused = h
        .cli(add_without_base(
            detached,
            "detached",
            Some(ShipMode::Local),
        ))
        .await
        .expect_err("detached");
    assert!(refused.to_string().contains("detached HEAD"), "{refused}");
}

#[tokio::test]
async fn a_pr_repository_without_an_origin_is_refused() {
    let h = Harness::new().await;
    let path = init_repo(&h, "noorigin", true);
    let refused = h
        .cli(add_without_base(path, "noorigin", None))
        .await
        .expect_err("no origin");
    assert!(
        refused.to_string().contains("no origin remote"),
        "{refused}"
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
        github_watch: false,
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
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
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
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
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
            && text.contains("direct")
            && text.contains(common::PIPELINE),
        "{text}"
    );
    let error = h
        .cli(Request::Resume(unknown.clone()))
        .await
        .expect_err("resuming a pipeline that is not paused and not known");
    assert!(error.to_string().contains("known pipelines"), "{error}");
    let mut state = h.ctx.store.lock().await.pause_state().unwrap();
    state.pipelines.push("no-such-pipeline".into());
    h.ctx.store.lock().await.set_pause_state(&state).unwrap();
    h.cli(Request::Resume(unknown))
        .await
        .expect("resuming a stale name that is paused stays possible");
    let known = PauseTarget::Pipeline {
        name: common::PIPELINE.into(),
    };
    h.cli(Request::Pause(known))
        .await
        .expect("a repository pipeline");
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
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
    let refused = h
        .orchestrator(resume.clone())
        .await
        .expect_err("the orchestrator cannot lift a hard pause");
    assert!(
        matches!(&refused, ArchonError::Forbidden(why) if why.contains("hard")),
        "{refused:?}"
    );
    let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
        panic!("status");
    };
    assert!(status.pause.global, "the hard pause holds");
    h.cli(resume).await.expect("the operator lifts it");
    let soft = Request::Pause(PauseTarget::Global { hard: false });
    h.orchestrator(soft).await.expect("soft pause");
    let resume = Request::Resume(PauseTarget::Global { hard: false });
    h.orchestrator(resume)
        .await
        .expect("the orchestrator lifts a soft pause");
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
        cost_complete: None,
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
            cost_complete: None,
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
        let Ok(Reply::Status(status)) = h.cli(Request::Status(Default::default())).await else {
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
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(repo_ref()),
        pipeline: None,
        title: "fix the flaky test".into(),
        brief: "asked for on GitHub".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: Some(origin.clone()),
    };
    let reply = h.orchestrator(Request::Enqueue(request)).await;
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
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(repo_ref()),
        pipeline: Some(common::PIPELINE.into()),
        title: "valid".into(),
        brief: "valid brief".into(),
        priority: Default::default(),
        depends_on: vec![],
        tags: vec![],
        model_plan: plan().into(),
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
        let error = h.orchestrator(Request::Enqueue(request)).await.unwrap_err();
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
