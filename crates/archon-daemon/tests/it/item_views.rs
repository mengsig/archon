//! The CLI's item views: summaries with the live stage, spend and PR URL,
//! and resolving a short id prefix to one item.

use crate::common;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{
    ItemQuery, ItemSummary, MAX_WAITING_PAGE, Reply, Request, StatusQuery,
};
use archon_core::queue::QueueItem;
use archon_core::run::{StageOutcome, StageRun};
use archon_queue::store::SpendRow;
use common::{Harness, completed, verdict};

const PR: &str = "https://example.invalid/pr/7";

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(serde_json::json!({ "pr_url": PR })),
        },
        _ => completed("implemented"),
    }
}

async fn summaries(h: &Harness) -> Vec<ItemSummary> {
    match h
        .cli(Request::ListItemSummaries(ItemQuery::default()))
        .await
    {
        Ok(Reply::ItemSummaries(rows)) => rows,
        other => panic!("list item summaries: {other:?}"),
    }
}

async fn resolve(h: &Harness, prefix: &str) -> Result<QueueItem, ArchonError> {
    let request = Request::ResolveItem {
        prefix: prefix.into(),
    };
    match h.cli(request).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("resolve replied {other:?}"),
    }
}

async fn spend(h: &Harness, item: &QueueItemId, stage: &StageRun, cost_usd: f64) {
    let row = SpendRow {
        agent: stage
            .aether_agent
            .clone()
            .expect("a running stage has an agent"),
        item: Some(item.clone()),
        cost_usd,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: 0.0,
        input_tokens: 1,
        output_tokens: 1,
        at: archon_core::now(),
    };
    h.ctx.store.lock().await.record_spend(&row).expect("spend");
}

#[tokio::test]
async fn summaries_show_the_live_stage_then_the_pr_and_the_spend() {
    let h = Harness::new().await;
    let item = h.enqueue("add a greeting").await;
    let rows = summaries(&h).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].stage.as_deref(), rows[0].spent_usd), (None, 0.0));

    h.dispatch().await;
    let implementer = h.running().await.remove(0);
    spend(&h, &item.id, &implementer, 0.5).await;
    let rows = summaries(&h).await;
    assert_eq!(rows[0].item.id, item.id);
    assert_eq!(rows[0].stage.as_deref(), Some("implement"));
    assert_eq!((rows[0].spent_usd, rows[0].pr_url.as_deref()), (0.5, None));

    for stage in ["implement", "review", "ship"] {
        let settled = h.settle_running(passing).await;
        assert!(settled.iter().all(|s| s.stage_id == stage), "{settled:?}");
        h.dispatch().await;
    }
    let rows = summaries(&h).await;
    assert_eq!(rows[0].item.state.kind(), "succeeded");
    assert_eq!(rows[0].stage, None, "a finished item is at no stage");
    assert_eq!(
        rows[0].pr_url.as_deref(),
        Some("https://github.com/o/r/pull/1"),
        "the summary exposes GitHub's verified URL, replacing the agent's claim"
    );
    assert_eq!(rows[0].spent_usd, 0.5);
    assert!(
        rows[0].waiting_reason.is_none(),
        "terminal items never wait"
    );
}

/// The characters after `qi_` two ids share.
fn shared_start(a: &QueueItemId, b: &QueueItemId) -> String {
    let (a, b) = (&a.as_str()[3..], &b.as_str()[3..]);
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

#[tokio::test]
async fn a_prefix_resolves_to_its_one_item_in_any_case() {
    let h = Harness::new().await;
    let one = h.enqueue("one").await;
    let two = h.enqueue("two").await;
    let shared = shared_start(&one.id, &two.id);
    let unique = &one.id.as_str()[3..shared.len() + 4];
    for text in [
        unique.to_owned(),
        unique.to_ascii_lowercase(),
        format!("qi_{unique}"),
        one.id.to_string(),
    ] {
        let found = resolve(&h, &text).await.expect("a unique prefix");
        assert_eq!(found.id, one.id, "{text}");
    }
}

#[tokio::test]
async fn an_ambiguous_prefix_names_every_item_it_matches() {
    let h = Harness::new().await;
    let one = h.enqueue("one").await;
    let two = h.enqueue("two").await;
    let shared = shared_start(&one.id, &two.id);
    assert!(!shared.is_empty(), "ids of one moment share their start");
    let error = resolve(&h, &shared.to_ascii_lowercase())
        .await
        .expect_err("ambiguous");
    let ArchonError::Invalid(message) = &error else {
        panic!("ambiguity is an invalid argument: {error:?}");
    };
    assert!(
        message.contains(&format!("qi_{shared} is ambiguous")),
        "{message}"
    );
    assert!(message.contains(&format!("{} (one)", one.id)), "{message}");
    assert!(message.contains(&format!("{} (two)", two.id)), "{message}");
}

#[tokio::test]
async fn an_unknown_or_malformed_prefix_is_refused_clearly() {
    let h = Harness::new().await;
    h.enqueue("one").await;
    // Ids from 2026 start with 01; a leading 7 is thousands of years away.
    let error = resolve(&h, "7ZZZ").await.expect_err("unknown");
    assert!(matches!(error, ArchonError::NotFound(_)), "{error:?}");
    assert!(error.to_string().contains("qi_7ZZZ"), "{error}");
    let error = resolve(&h, "qi_01*").await.expect_err("malformed");
    assert!(matches!(error, ArchonError::Invalid(_)), "{error:?}");
    assert!(
        error.to_string().contains("is not a valid item id"),
        "{error}"
    );
}

use archon_core::protocol::{
    ItemPatch, PauseTarget, RepoRef, WaitingReason, WaitingReasonCode as Code,
};
use archon_core::queue::{Caps, QueueState};

async fn reason(h: &Harness, id: &QueueItemId, code: Code) -> WaitingReason {
    let Reply::ItemDetail(detail) = h.cli(Request::GetItem { item: id.clone() }).await.unwrap()
    else {
        panic!("detail")
    };
    let wait = detail
        .waiting_reason
        .clone()
        .expect("read-time waiting reason");
    assert_eq!(wait.code, code);
    assert!(!wait.message.is_empty());
    assert!(!wait.next_action.is_empty());
    let row = summaries(h)
        .await
        .into_iter()
        .find(|row| &row.item.id == id)
        .unwrap();
    assert_eq!(row.waiting_reason.as_ref(), Some(&wait));
    let Reply::Status(status) = h.cli(Request::Status(Default::default())).await.unwrap() else {
        panic!("status")
    };
    let status_row = status
        .waiting_items
        .iter()
        .find(|row| &row.item.id == id)
        .unwrap();
    assert_eq!(status_row.waiting_reason.as_ref(), Some(&wait));
    let json = serde_json::to_value(&detail).unwrap();
    assert_eq!(
        json["waiting_reason"]["code"],
        serde_json::to_value(code).unwrap()
    );
    assert_eq!(json["item"]["state"]["state"], detail.item.state.kind());
    assert!(
        serde_json::to_value(&detail.item)
            .unwrap()
            .get("waiting_reason")
            .is_none(),
        "not stored on QueueItem"
    );
    wait
}

#[tokio::test]
async fn next_in_line_and_empty_status_are_derived_without_dispatching() {
    let h = Harness::new().await;
    let Reply::Status(status) = h.cli(Request::Status(Default::default())).await.unwrap() else {
        panic!("status")
    };
    assert!(status.waiting_items.is_empty());
    let item = h.enqueue("ready").await;
    reason(&h, &item.id, Code::NextInLine).await;
    assert_eq!(
        h.ctx.store.lock().await.item(&item.id).unwrap().state,
        QueueState::Queued
    );
    assert!(h.running().await.is_empty());
}

#[tokio::test]
async fn pause_reasons_refresh_at_read_time_in_all_views() {
    let h = Harness::new().await;
    let item = h.enqueue("paused").await;
    for (target, code, action) in [
        (
            PauseTarget::Global { hard: false },
            Code::GlobalPause,
            "archon resume",
        ),
        (
            PauseTarget::Repo {
                name: RepoRef(common::REPO_NAME.into()),
            },
            Code::RepoPause,
            "archon resume --repo",
        ),
        (
            PauseTarget::Pipeline {
                name: common::PIPELINE.into(),
            },
            Code::PipelinePause,
            "archon resume --pipeline",
        ),
    ] {
        h.cli(Request::Pause(target.clone())).await.unwrap();
        let wait = reason(&h, &item.id, code).await;
        assert!(wait.next_action.starts_with(action));
        h.cli(Request::Resume(target)).await.unwrap();
        reason(&h, &item.id, Code::NextInLine).await;
    }
}

#[tokio::test]
async fn budget_reasons_use_dispatch_spend_and_precedence() {
    let h = Harness::new().await;
    let active = h.enqueue("active").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    spend(&h, &active.id, &stage, 5.0).await;
    let item = h.enqueue("budget hold").await;
    let mut caps = Caps {
        budget_usd_total: 5.0,
        budget_usd_daily: 5.0,
        ..Caps::default()
    };
    h.cli(Request::SetCaps { caps: caps.clone() })
        .await
        .unwrap();
    let total = reason(&h, &item.id, Code::TotalBudget).await;
    assert!(total.next_action.contains("--budget-total 6.00"));
    caps.budget_usd_total = 0.0;
    h.cli(Request::SetCaps { caps: caps.clone() })
        .await
        .unwrap();
    let daily = reason(&h, &item.id, Code::DailyBudget).await;
    assert!(daily.next_action.contains("--budget-daily 6.00"));
    assert!(daily.message.contains("local midnight"));
    caps.budget_usd_daily = 0.0;
    h.cli(Request::SetCaps { caps }).await.unwrap();
    reason(&h, &item.id, Code::NextInLine).await;
}

#[tokio::test]
async fn reached_budgets_hold_nothing_while_budgets_are_off() {
    let h = Harness::new().await;
    let active = h.enqueue("active").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    spend(&h, &active.id, &stage, 5.0).await;
    let item = h.enqueue("budget hold").await;
    let mut caps = Caps {
        budget_usd_total: 5.0,
        budget_usd_daily: 5.0,
        ..Caps::default()
    };
    h.cli(Request::SetCaps { caps: caps.clone() })
        .await
        .unwrap();
    reason(&h, &item.id, Code::TotalBudget).await;
    caps.budgets_enabled = false;
    h.cli(Request::SetCaps { caps }).await.unwrap();
    reason(&h, &item.id, Code::NextInLine).await;
}

#[tokio::test]
async fn capacity_reason_includes_live_agents_and_repo_file_overrides() {
    let h = Harness::new().await;
    h.enqueue("active").await;
    h.dispatch().await;
    let item = h.enqueue("capacity hold").await;
    h.cli(Request::SetCaps {
        caps: Caps {
            max_parallel_agents: 1,
            ..Caps::default()
        },
    })
    .await
    .unwrap();
    reason(&h, &item.id, Code::Capacity).await;
    h.cli(Request::SetCaps {
        caps: Caps::default(),
    })
    .await
    .unwrap();
    std::fs::write(
        h.repo.join(".archon/config.toml"),
        "[limits]\nmax_per_repo = 1\n",
    )
    .unwrap();
    reason(&h, &item.id, Code::Capacity).await;
    std::fs::write(
        h.repo.join(".archon/config.toml"),
        "[limits]\nmax_per_repo = 2\n",
    )
    .unwrap();
    reason(&h, &item.id, Code::NextInLine).await;
}

#[tokio::test]
async fn low_disk_reason_uses_dispatcher_hold_without_a_disk_check() {
    let h = Harness::new().await;
    let item = h.enqueue("disk hold").await;
    h.ctx.disk.update(
        archon_daemon::disk::DiskReading {
            path: h.dir.path().into(),
            free_bytes: 0,
        },
        1,
        std::time::Instant::now(),
    );
    let wait = reason(&h, &item.id, Code::LowDisk).await;
    assert_eq!(wait.next_action, "archon gc");
    assert!(
        h.ctx.disk.holding(),
        "views never update the dispatcher hold"
    );
}

#[tokio::test]
async fn dependency_names_and_failure_precedence_survive_blocking() {
    let h = Harness::new().await;
    let dep = h.enqueue("required migration").await;
    let failed = h.enqueue("required schema").await;
    let item = h.enqueue("dependent").await;
    h.cli(Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            add_depends_on: vec![dep.id.clone(), failed.id.clone()],
            ..ItemPatch::default()
        },
    })
    .await
    .unwrap();
    h.cli(Request::Pause(PauseTarget::Global { hard: false }))
        .await
        .unwrap();
    let wait = reason(&h, &item.id, Code::DependencyWaiting).await;
    assert!(wait.message.contains("required migration"));
    assert_eq!(wait.next_action, format!("archon queue show {}", dep.id));
    h.cli(Request::CancelItem {
        item: failed.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    let wait = reason(&h, &item.id, Code::DependencyFailed).await;
    assert!(wait.message.contains("required schema"));
    assert_eq!(
        wait.next_action,
        format!("archon queue retry {}", failed.id)
    );
    h.dispatch().await;
    assert!(matches!(
        h.ctx.store.lock().await.item(&item.id).unwrap().state,
        QueueState::Blocked { .. }
    ));
    reason(&h, &item.id, Code::DependencyFailed).await;
}

#[tokio::test]
async fn never_fitting_team_names_its_cap_and_suggests_a_working_next_action() {
    let h = Harness::new().await;
    let pipeline = "first-team";
    h.add_pipeline(
        pipeline,
        r#"[pipeline]
name = "first-team"
description = "First stage team"
version = 1
workspace = "worktree"
requires_repo = true
[[stage]]
id = "implement"
kind = "team"
prompt = "implement.md"
roles = ["api", "ui"]
timeout_minutes = 30
"#,
    );
    let plan = archon_core::model::ModelPlan(
        [(
            "implement".into(),
            vec![common::choice(common::MODEL, archon_core::model::Effort::Low); 2],
        )]
        .into(),
    );
    let item = h.enqueue_in(pipeline, "a team", plan).await.unwrap();
    h.cli(Request::SetCaps {
        caps: Caps {
            max_parallel_agents: 1,
            ..Caps::default()
        },
    })
    .await
    .unwrap();
    let wait = reason(&h, &item.id, Code::Capacity).await;
    assert!(wait.message.contains("needs 2 agents together"));
    assert_eq!(
        wait.next_action,
        "archon queue caps --max-parallel-agents 2"
    );
    h.cli(Request::SetCaps {
        caps: Caps {
            max_parallel_agents: 2,
            ..Caps::default()
        },
    })
    .await
    .unwrap();
    reason(&h, &item.id, Code::NextInLine).await;
    std::fs::write(
        h.repo.join(".archon/config.toml"),
        "[limits]\nmax_per_repo = 1\n",
    )
    .unwrap();
    let wait = reason(&h, &item.id, Code::Capacity).await;
    assert!(wait.next_action.contains("max_per_repo = 2"));
    assert!(
        wait.next_action
            .contains(&h.repo.join(".archon/config.toml").display().to_string())
    );
    h.cli(Request::SetCaps {
        caps: Caps {
            max_parallel_agents: 1,
            max_per_repo: 1,
            max_per_pipeline: [(pipeline.into(), 1)].into(),
            ..Caps::default()
        },
    })
    .await
    .unwrap();
    let wait = reason(&h, &item.id, Code::Capacity).await;
    for cap in [
        "--max-parallel-agents 2",
        "--max-per-repo 2",
        "max_per_repo = 2",
        "limits.per_pipeline",
    ] {
        assert!(wait.next_action.contains(cap), "{}", wait.next_action);
    }
}

async fn usage(h: &Harness, stage: &StageRun, basis: &str, amount: f64, estimate: Option<f64>) {
    let fixture = match basis {
        "list_price" => include_str!("../../../archon-aether/tests/fixtures/usage_list_price.json"),
        "provider_reported" => {
            include_str!("../../../archon-aether/tests/fixtures/usage_provider_reported.json")
        }
        _ => panic!("unsupported fixture basis"),
    };
    let mut wire: serde_json::Value = serde_json::from_str(fixture).unwrap();
    wire["agent_id"] = serde_json::json!(stage.aether_agent);
    wire["task_id"] = serde_json::json!(stage.aether_task);
    wire["at"] = serde_json::json!(archon_core::now());
    wire["kind"]["total"]["cost_usd"] = serde_json::json!(amount);
    wire["kind"]["total"]["estimated_cost_usd"] = serde_json::json!(estimate);
    wire["seq"] = serde_json::json!(1);
    wire["at"] = serde_json::json!(archon_core::now());
    let event = archon_aether::events::parse_event(wire).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
}

async fn detail(h: &Harness, item: &QueueItemId) -> archon_core::protocol::ItemDetail {
    match h
        .cli(Request::GetItem { item: item.clone() })
        .await
        .unwrap()
    {
        Reply::ItemDetail(detail) => *detail,
        other => panic!("item detail expected: {other:?}"),
    }
}

async fn succeed(h: &Harness) {
    for _ in 0..3 {
        h.settle_running(passing).await;
        h.dispatch().await;
    }
}

#[tokio::test]
async fn finished_run_cost_tracks_late_usage_and_survives_restart() {
    let mut h = Harness::new().await;
    let item = h.enqueue("late final usage").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    usage(&h, &stage, "list_price", 0.5, None).await;
    succeed(&h).await;
    let before = detail(&h, &item.id).await;
    assert!(before.runs[0].state.is_terminal());
    assert_eq!(before.runs[0].cost_usd, 0.5);
    usage(&h, &stage, "list_price", 0.75, None).await;
    for restart in [false, true] {
        if restart {
            h.restart();
        }
        let after = detail(&h, &item.id).await;
        assert_eq!(after.runs[0].cost_usd, 0.75);
        assert_eq!(
            after.runs[0].cost_usd,
            after.stages.iter().map(|s| s.cost_usd).sum::<f64>()
        );
        assert_eq!(after.spent_usd, 0.75);
    }
}

#[tokio::test]
async fn retry_run_never_absorbs_an_earlier_runs_late_usage() {
    let mut h = Harness::new().await;
    let item = h.enqueue("retry with late usage").await;
    h.dispatch().await;
    let old = h.running().await.remove(0);
    usage(&h, &old, "list_price", 0.5, None).await;
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    h.cli(Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    })
    .await
    .unwrap();
    h.dispatch().await;
    let new = h.running().await.remove(0);
    assert_ne!(old.run, new.run);
    usage(&h, &new, "provider_reported", 0.5, None).await;
    usage(&h, &old, "list_price", 0.75, None).await;
    succeed(&h).await;
    // Even the compatibility snapshot at finish uses this run's own agents.
    assert_eq!(
        h.ctx.store.lock().await.run(&new.run).unwrap().cost_usd,
        0.5
    );
    usage(&h, &old, "list_price", 1.0, None).await;
    usage(&h, &new, "provider_reported", 0.875, None).await;
    h.restart();
    let after = detail(&h, &item.id).await;
    assert_eq!(after.runs.len(), 2);
    for (id, expected) in [(&old.run, 1.0), (&new.run, 0.875)] {
        let run = after.runs.iter().find(|r| &r.id == id).unwrap();
        assert_eq!(run.cost_usd, expected);
        assert_eq!(
            run.cost_usd,
            after
                .stages
                .iter()
                .filter(|s| &s.run == id)
                .map(|s| s.cost_usd)
                .sum::<f64>()
        );
    }
    assert_eq!(after.spent_usd, 1.875);
}

#[tokio::test]
async fn real_wire_cost_bases_reach_reports_without_changing_budget_totals() {
    use archon_core::cost::CostQuery;
    for estimates in [true, false] {
        let mut config = archon_core::config::Config::default();
        config.limits.count_estimated_cost = estimates;
        let h = Harness::with_config(config).await;
        let item = h.enqueue("classify wire usage").await;
        h.dispatch().await;
        let first = h.running().await.remove(0);
        usage(&h, &first, "list_price", 0.5, Some(0.25)).await;
        h.settle_running(passing).await;
        h.dispatch().await;
        let reviewer = h.running().await.remove(0);
        usage(&h, &reviewer, "provider_reported", 0.5, None).await;
        let expected = if estimates { 1.25 } else { 1.0 };
        let Reply::CostReport(report) = h
            .cli(Request::CostReport(CostQuery::default()))
            .await
            .unwrap()
        else {
            panic!("cost report expected")
        };
        assert_eq!(report.total.cost_usd, 0.5);
        assert_eq!(report.total.estimated_cost_usd, 0.75);
        assert_eq!(report.total.unclassified_cost_usd, 0.0);
        assert_eq!(report.total.counted_usd, expected);
        assert_eq!(detail(&h, &item.id).await.spent_usd, expected);
        let Reply::Status(status) = h.cli(Request::Status(Default::default())).await.unwrap()
        else {
            panic!("status expected")
        };
        assert_eq!(status.spend_today_usd, expected);
        assert_eq!(status.spend_total_usd, expected);
    }
}

#[tokio::test]
async fn list_price_still_exhausts_a_budget_when_separate_estimates_are_disabled() {
    for estimates in [true, false] {
        let mut config = archon_core::config::Config::default();
        config.limits.count_estimated_cost = estimates;
        config.limits.budget_usd_per_item = 1.0;
        let h = Harness::with_config(config).await;
        let item = h.enqueue("list-price budget enforcement").await;
        h.dispatch().await;
        let stage = h.running().await.remove(0);
        usage(&h, &stage, "list_price", 1.0, None).await;
        let after = detail(&h, &item.id).await;
        assert!(matches!(
            after.item.state,
            archon_core::queue::QueueState::Failed { .. }
        ));
        assert_eq!(after.spent_usd, 1.0);
        assert!(after.runs[0].state.is_terminal());
        assert!(h.live_stages().await.is_empty());
    }
}

#[tokio::test]
async fn disconnected_aether_has_a_visible_dispatch_hold() {
    let h = Harness::new().await;
    let item = h.enqueue("ready but disconnected").await;
    h.ctx.aether.replace(None).unwrap();
    assert!(!h.ctx.aether.is_connected());
    let wait = reason(&h, &item.id, Code::ModelCatalogUnavailable).await;
    assert!(wait.message.contains("catalog"), "{}", wait.message);
}

#[tokio::test]
async fn a_running_items_next_stage_names_the_catalog_hold() {
    let h = Harness::new().await;
    let item = h.enqueue("two stages through an outage").await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1);
    crate::refresh::catalog_outage(&h).await;
    h.settle_running(passing).await;
    h.dispatch().await;
    assert!(
        h.running().await.is_empty(),
        "the held stage creates no agent"
    );
    assert!(detail(&h, &item.id).await.item.state.is_active());
    let wait = reason(&h, &item.id, Code::ModelCatalogUnavailable).await;
    assert!(wait.message.contains("catalog"), "{}", wait.message);

    crate::refresh::catalog_back(&h);
    h.dispatch().await;
    assert!(
        !h.running().await.is_empty(),
        "the next stage starts by itself"
    );
    assert!(detail(&h, &item.id).await.waiting_reason.is_none());
}

#[tokio::test]
async fn subcent_budget_holds_remain_visible() {
    let h = Harness::new().await;
    let active = h.enqueue("active").await;
    h.dispatch().await;
    spend(&h, &active.id, &h.running().await.remove(0), 0.001).await;
    let item = h.enqueue("tiny hold").await;
    h.cli(Request::SetCaps {
        caps: Caps {
            budget_usd_total: 0.001,
            ..Caps::default()
        },
    })
    .await
    .unwrap();
    let wait = reason(&h, &item.id, Code::TotalBudget).await;
    assert!(wait.message.contains("<$0.01 / <$0.01"), "{}", wait.message);
}

async fn delegate(
    h: &Harness,
    parent: &archon_core::aether_ids::AetherAgentId,
    agent: &str,
) -> archon_core::aether_ids::AetherAgentId {
    let mut wire = archon_aether::fake::event(
        Some(agent),
        None,
        serde_json::json!({
            "type": "agent.created", "name": agent, "model": common::MODEL.parse::<archon_core::model::ModelRef>().unwrap()
        }),
    );
    wire["parent_agent_id"] = serde_json::json!(parent);
    wire["seq"] = serde_json::json!(1);
    wire["at"] = serde_json::json!(archon_core::now());
    let event = archon_aether::events::parse_event(wire).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
    archon_core::aether_ids::AetherAgentId::parse(agent).unwrap()
}

async fn child_usage(h: &Harness, agent: &archon_core::aether_ids::AetherAgentId, amount: f64) {
    let mut wire = archon_aether::fake::event(
        Some(agent.as_str()),
        None,
        serde_json::json!({
            "type": "agent.usage_updated", "total": {"cost_usd": amount, "cost_basis": "provider_reported"}
        }),
    );
    wire["seq"] = serde_json::json!(1);
    wire["at"] = serde_json::json!(archon_core::now());
    let event = archon_aether::events::parse_event(wire).unwrap();
    archon_daemon::pump::handle_fleet_event(&h.ctx, &event)
        .await
        .unwrap();
}

/// A stage that delegated costs what its own agent and the agents beneath it
/// were charged. Both views split it, and the stages add up to the run.
#[tokio::test]
async fn a_stage_cost_includes_its_delegated_agents_and_counts_each_once() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let item = h.enqueue("delegating stage").await;
    h.dispatch().await;
    let lead = h.running().await.remove(0);
    let lead_agent = lead.aether_agent.clone().unwrap();
    let child = delegate(&h, &lead_agent, "agt_split_child").await;
    let grandchild = delegate(&h, &child, "agt_split_grandchild").await;
    usage(&h, &lead, "provider_reported", 0.25, None).await;
    child_usage(&h, &child, 0.5).await;
    child_usage(&h, &grandchild, 1.0).await;

    let shown = detail(&h, &item.id).await;
    assert_eq!(shown.stages.len(), 1);
    assert_eq!(shown.stages[0].cost_usd, 1.75);
    let split = &shown.delegations[&lead.id];
    assert_eq!((split.direct_usd, split.delegated_usd), (0.25, 1.5));
    let agents: Vec<_> = split
        .agents
        .iter()
        .map(|a| (a.agent.clone(), a.parent.clone(), a.cost_usd))
        .collect();
    assert_eq!(
        agents,
        [
            (child.clone(), lead_agent.clone(), 0.5),
            (grandchild.clone(), child.clone(), 1.0)
        ]
    );
    assert!(
        split
            .agents
            .iter()
            .all(|a| a.model.to_string() == common::MODEL)
    );
    let stage_sum: f64 = shown.stages.iter().map(|s| s.cost_usd).sum();
    assert_eq!(
        stage_sum, shown.runs[0].cost_usd,
        "the stages add up to the run"
    );
    assert_eq!(shown.spent_usd, 1.75, "the item counts each agent once");

    let query = archon_core::protocol::StageQuery {
        item: item.id.clone(),
        run: None,
        stage: None,
        attempt: None,
        slot: None,
        offset: 0,
        limit: None,
    };
    let Reply::StageDetail(listing) = h.cli(Request::GetStage(query)).await.unwrap() else {
        panic!("a stage listing");
    };
    assert_eq!(listing.stages[0].cost_usd, 1.75);
    assert_eq!(listing.stages[0].delegation.as_ref(), Some(split));
}

/// A delegated agent's effort is read from Aether when it is created, so
/// the split of a stage still running shows it rather than `effort unknown`.
#[tokio::test]
async fn a_running_stage_shows_the_effort_of_the_agents_it_delegated_to() {
    let h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let item = h.enqueue("delegating stage").await;
    h.dispatch().await;
    let lead = h.running().await.remove(0);
    let lead_agent = lead.aether_agent.clone().unwrap();
    let held = h.backend.delegate(&lead_agent, common::MODEL);
    let child = delegate(&h, &lead_agent, held.as_str()).await;
    let unknown = delegate(&h, &child, "agt_effort_unread").await;
    child_usage(&h, &child, 0.5).await;

    let shown = detail(&h, &item.id).await;
    assert_eq!(h.running().await.len(), 1, "the stage still runs");
    let agents = &shown.delegations[&lead.id].agents;
    let efforts: Vec<_> = agents.iter().map(|a| (a.agent.clone(), a.effort)).collect();
    assert_eq!(
        efforts,
        [
            (child, Some(archon_core::model::Effort::Medium)),
            (unknown, None)
        ],
        "Aether holds no effort for the second"
    );
    assert!(
        agents[0].line().contains("(medium)"),
        "{}",
        agents[0].line()
    );
}

#[tokio::test]
async fn delegated_descendant_cost_stays_with_its_run_after_retry_and_restart() {
    let mut h = Harness::new().await;
    let _aether = common::connect_aether_for_delegations(&h).await;
    let item = h.enqueue("descendant spend").await;
    h.dispatch().await;
    let old = h.running().await.remove(0);
    let child = delegate(&h, old.aether_agent.as_ref().unwrap(), "agt_child_cost").await;
    let grandchild = delegate(&h, &child, "agt_grandchild_cost").await;
    usage(&h, &old, "provider_reported", 0.25, Some(0.125)).await;
    child_usage(&h, &child, 0.5).await;
    child_usage(&h, &grandchild, 1.0).await;
    let before = detail(&h, &item.id).await;
    assert_eq!(before.runs[0].cost_usd, 1.875);
    assert_eq!(before.spent_usd, 1.875);
    h.cli(Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    })
    .await
    .unwrap();
    assert_eq!(
        h.ctx.store.lock().await.run(&old.run).unwrap().cost_usd,
        1.875
    );
    h.cli(Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    })
    .await
    .unwrap();
    h.dispatch().await;
    let new = h.running().await.remove(0);
    usage(&h, &new, "provider_reported", 0.25, None).await;
    child_usage(&h, &grandchild, 2.0).await;
    h.restart();
    let after = detail(&h, &item.id).await;
    assert_eq!(
        after
            .runs
            .iter()
            .find(|r| r.id == old.run)
            .unwrap()
            .cost_usd,
        2.875
    );
    assert_eq!(
        after
            .runs
            .iter()
            .find(|r| r.id == new.run)
            .unwrap()
            .cost_usd,
        0.25
    );
    assert_eq!(after.spent_usd, 3.125);
    assert_eq!(
        h.ctx
            .store
            .lock()
            .await
            .spend_for_run(&old.run, false)
            .unwrap(),
        2.75
    );
}

async fn status_page(
    h: &Harness,
    offset: usize,
    limit: Option<usize>,
) -> Result<Reply, ArchonError> {
    let query = StatusQuery {
        waiting_offset: offset,
        waiting_limit: limit,
    };
    h.cli(Request::Status(query)).await
}

fn waiting_ids(reply: &Reply) -> (Vec<QueueItemId>, u32, Option<u32>) {
    let Reply::Status(status) = reply else {
        panic!("status: {reply:?}")
    };
    let ids = status.waiting_items.iter().map(|row| row.item.id.clone());
    (
        ids.collect(),
        status.waiting_total,
        status.waiting_next_offset,
    )
}

#[tokio::test]
async fn status_pages_its_waiting_items_with_a_next_offset() {
    let h = Harness::new().await;
    let mut queued = Vec::new();
    for title in ["first", "second", "third"] {
        queued.push(h.enqueue(title).await.id);
    }
    let whole = status_page(&h, 0, None).await.unwrap();
    let (all, total, next) = waiting_ids(&whole);
    assert_eq!(
        (all.len(), total, next),
        (3, 3, None),
        "one default page holds them"
    );
    let (first, total, next) = waiting_ids(&status_page(&h, 0, Some(2)).await.unwrap());
    assert_eq!((first.len(), total, next), (2, 3, Some(2)));
    let (last, total, next) = waiting_ids(&status_page(&h, 2, Some(2)).await.unwrap());
    assert_eq!((last.len(), total, next), (1, 3, None));
    let paged: Vec<QueueItemId> = first.into_iter().chain(last).collect();
    assert_eq!(paged, all, "the pages split the list in order");
    let mut sorted = paged.clone();
    sorted.sort();
    queued.sort();
    assert_eq!(sorted, queued);
    let (past, total, next) = waiting_ids(&status_page(&h, 9, Some(2)).await.unwrap());
    assert_eq!((past.len(), total, next), (0, 3, None));
    for limit in [0, MAX_WAITING_PAGE + 1] {
        let refused = status_page(&h, 0, Some(limit)).await;
        assert!(
            matches!(refused, Err(ArchonError::Invalid(_))),
            "{refused:?}"
        );
    }
}

const HAIKU: &str = "claude-code/claude-haiku-5-5";

/// Aether lists the harness models but `without`; the next pass reads it.
async fn serve_catalog_without(h: &Harness, without: &str) {
    let listed: Vec<archon_core::model::ModelRef> = common::catalog::default_models()
        .models()
        .iter()
        .map(|entry| entry.model.clone())
        .filter(|model| model.to_string() != without)
        .collect();
    common::catalog::answer(&h.catalog_server, &common::catalog::untiered(listed));
    h.ctx
        .store
        .lock()
        .await
        .invalidate_model_catalog("catalog changed".into());
}

#[tokio::test]
async fn a_finished_stages_retired_pin_holds_no_later_stage() {
    let h = Harness::new().await;
    let mut plan = common::plan();
    plan.0.insert(
        "implement".into(),
        vec![common::choice(HAIKU, archon_core::model::Effort::Medium)],
    );
    let item = h.enqueue_with("retire a finished pin", plan).await.unwrap();
    h.dispatch().await;
    assert_eq!(h.running().await[0].choice.model.to_string(), HAIKU);
    serve_catalog_without(&h, HAIKU).await;
    h.settle_running(passing).await;
    h.dispatch().await;
    let running = h.running().await;
    assert!(!running.is_empty(), "the next stage starts");
    assert!(running.iter().all(|stage| stage.stage_id == "review"));
    assert!(detail(&h, &item.id).await.waiting_reason.is_none());
}

#[tokio::test]
async fn a_running_items_next_stage_names_its_retired_pin() {
    let h = Harness::new().await;
    let item = h.enqueue("retire the next stage's pin").await;
    h.dispatch().await;
    assert_eq!(h.running().await.len(), 1);
    serve_catalog_without(&h, common::REVIEW_MODEL).await;
    h.settle_running(passing).await;
    h.dispatch().await;
    assert!(
        h.running().await.is_empty(),
        "the held stage creates no agent"
    );
    let wait = reason(&h, &item.id, Code::Blocked).await;
    assert!(wait.message.contains("valid model"), "{}", wait.message);
    assert!(
        wait.message.contains(common::REVIEW_MODEL),
        "{}",
        wait.message
    );
    assert!(
        wait.next_action.contains("update_item"),
        "{}",
        wait.next_action
    );
}

#[tokio::test]
async fn a_pin_retired_while_its_create_is_prepared_is_never_sent() {
    let h = Harness::new().await;
    let mut plan = common::plan();
    plan.0.insert(
        "implement".into(),
        vec![common::choice(HAIKU, archon_core::model::Effort::Medium)],
    );
    let item = h
        .enqueue_with("retire a pin mid-create", plan)
        .await
        .unwrap();
    let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    serve_until_retired(&h, retired.clone());
    let ctx = h.ctx.clone();
    h.backend.on_host_context(2, move || {
        retired.store(true, std::sync::atomic::Ordering::SeqCst);
        ctx.store
            .try_lock()
            .expect("no store lock is held while a create is prepared")
            .invalidate_model_catalog("catalog changed".into());
    });
    h.dispatch().await;
    assert!(
        h.backend.requests().is_empty(),
        "no create under a retired pin"
    );
    let stages = h.live_stages().await;
    assert_eq!(stages.len(), 1, "the stage stays reserved for a new check");
    assert_eq!(stages[0].state, archon_core::run::StageState::Spawning);
    let state = h.item(&item.id).await.state;
    assert!(matches!(state, QueueState::Running { .. }), "{state:?}");
}

#[tokio::test]
async fn a_pin_retired_while_its_run_node_is_prepared_holds_the_stage() {
    // The node's two host-context lookups come before its create is sent.
    for lookup in [0, 1] {
        let h = Harness::new().await;
        let mut plan = common::plan();
        plan.0.insert(
            "implement".into(),
            vec![common::choice(HAIKU, archon_core::model::Effort::Medium)],
        );
        let item = h.enqueue_with("retire a pin mid-node", plan).await.unwrap();
        let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        serve_until_retired(&h, retired.clone());
        let ctx = h.ctx.clone();
        h.backend.on_host_context(lookup, move || {
            retired.store(true, std::sync::atomic::Ordering::SeqCst);
            ctx.store
                .try_lock()
                .expect("no store lock is held while a node is prepared")
                .invalidate_model_catalog("catalog changed".into());
        });
        h.dispatch().await;
        assert!(h.backend.node_requests().is_empty(), "no node on that pin");
        assert!(h.backend.requests().is_empty(), "no create on that pin");
        let stages = h.live_stages().await;
        assert_eq!(stages.len(), 1, "the stage stays reserved for a new check");
        assert_eq!(stages[0].state, archon_core::run::StageState::Spawning);
        let state = h.item(&item.id).await.state;
        assert!(matches!(state, QueueState::Running { .. }), "{state:?}");
    }
}

/// The run's node entry as a daemon from before generations were labelled
/// left it: generation two pending on `model`, its create never sent.
async fn legacy_pending_node(h: &Harness, run: &archon_core::id::PipelineRunId, model: &str) {
    let key = format!("run_node:{run}");
    let mut store = h.ctx.store.lock().await;
    let raw = store.kv_get(&key).expect("kv").expect("a stored node");
    let mut node: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(node["agent"].is_string(), "generation one was recorded");
    node["agent"] = serde_json::Value::Null;
    node["generation"] = serde_json::json!(2);
    let model: archon_core::model::ModelRef = model.parse().expect("a model id");
    let request = node["request"].as_object_mut().expect("a stored request");
    request["idempotency_key"] = serde_json::json!(format!("{run}:node:2"));
    request["model"] = serde_json::to_value(model).expect("json");
    let labels = request["labels"].as_object_mut().expect("labels");
    assert!(labels.remove("archon.node_generation").is_some());
    store.kv_set(&key, &node.to_string()).expect("kv set");
}

#[tokio::test]
async fn a_legacy_pending_node_on_a_retired_model_is_told_from_the_runs_earlier_node() {
    let h = Harness::new().await;
    let item = h.enqueue("replace a legacy node").await;
    h.dispatch().await;
    let first = h.backend.node_ids()[0].clone();
    legacy_pending_node(&h, &h.run(&item.id).await.id, HAIKU).await;
    serve_catalog_without(&h, HAIKU).await;
    h.settle_running(passing).await;
    h.dispatch().await;
    assert_eq!(
        h.backend.node_lookups(),
        vec![vec![first]],
        "the lookup leaves out the node generation one recorded"
    );
    let nodes = h.backend.node_requests();
    assert_eq!(nodes.len(), 2, "the retired generation is never sent");
    assert!(nodes[1].idempotency_key.ends_with(":node:3"), "{nodes:?}");
    assert_eq!(nodes[1].model.to_string(), common::REVIEW_MODEL);
    let running = h.running().await;
    assert!(!running.is_empty(), "the next stage starts under the node");
    assert!(running.iter().all(|stage| stage.stage_id == "review"));
}

/// Lists the default catalog, without `HAIKU` once `retired` is set.
fn serve_until_retired(h: &Harness, retired: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let rows = |without: Option<&str>| {
        let models = common::catalog::default_models();
        let listed: Vec<serde_json::Value> = models
            .models()
            .iter()
            .filter(|entry| Some(entry.model.to_string().as_str()) != without)
            .map(|entry| serde_json::json!({"model": entry.model, "configured": true}))
            .collect();
        archon_aether::fake::Scripted::reply("models", serde_json::json!(listed))
    };
    let (all, without) = (rows(None), rows(Some(HAIKU)));
    h.catalog_server.set_responder(
        "list_models",
        std::sync::Arc::new(
            move |_| match retired.load(std::sync::atomic::Ordering::SeqCst) {
                true => without.clone(),
                false => all.clone(),
            },
        ),
    );
}

/// A stage left Spawning whose stored request names no network, as a
/// daemon from before networks were frozen stored it.
async fn legacy_unsent_stage(h: &Harness) -> QueueItem {
    h.backend
        .fail_next(ArchonError::Unavailable("aetherd busy".into()));
    let item = h.enqueue("a create from before networks were stored").await;
    h.dispatch().await;
    let stages = h.live_stages().await;
    assert_eq!(stages.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(
        h.backend.checked().len(),
        1,
        "the failed create was checked"
    );
    let key = format!("spawn_request:{}", stages[0].id);
    let mut stored: serde_json::Value = serde_json::from_str(&h.kv(&key).await.unwrap()).unwrap();
    stored["request"]
        .as_object_mut()
        .expect("a stored request")
        .remove("network")
        .expect("a network was stored");
    h.ctx
        .store
        .lock()
        .await
        .kv_set(&key, &stored.to_string())
        .unwrap();
    item
}

/// A create frozen before its network was stored is sent as stored, with
/// Aether's default read before the catalog check, so only the create follows.
#[tokio::test]
async fn a_legacy_create_without_a_network_reads_the_default_before_its_check() {
    let h = Harness::new().await;
    legacy_unsent_stage(&h).await;
    h.backend
        .set_network_default(archon_core::ports::NetworkPolicy::Denied);

    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 2, "the create is issued again");
    assert_eq!(requests[1].network, None, "sent as stored");
    let checked = h.backend.checked();
    assert_eq!(checked.len(), 2, "the failed create, then this one");
    assert_eq!(
        checked.last().map(|on| on.network_default),
        Some(archon_core::ports::NetworkPolicy::Denied),
        "Aether's default of the moment"
    );
}

/// A catalog change while the default is read is seen by the check after it.
#[tokio::test]
async fn a_catalog_change_while_a_legacy_default_is_read_sends_nothing() {
    let h = Harness::new().await;
    legacy_unsent_stage(&h).await;
    let ctx = h.ctx.clone();
    h.backend.on_next_network_default(move || {
        ctx.store
            .try_lock()
            .expect("no store lock is held while the default is read")
            .invalidate_model_catalog("catalog changed".into());
    });
    h.dispatch().await;
    assert_eq!(
        h.backend.checked().len(),
        1,
        "no create after a stale check, beyond the failed one"
    );
    assert_eq!(
        h.backend.requests().len(),
        1,
        "only the first, failed create"
    );
}

/// A reconnect after the default was read sends nothing, though the new
/// connection lists the same catalog: its default may differ.
#[tokio::test]
async fn a_reconnect_after_a_legacy_default_is_read_sends_nothing() {
    let h = Harness::new().await;
    legacy_unsent_stage(&h).await;
    let ctx = h.ctx.clone();
    h.backend
        .on_host_context(0, move || reconnect_with_the_same_catalog(&ctx));
    h.dispatch().await;
    assert_eq!(
        h.backend.checked().len(),
        1,
        "no create on another connection, beyond the failed one"
    );
    assert_eq!(
        h.backend.requests().len(),
        1,
        "only the first, failed create"
    );
}

/// Reconnect to Aether, which lists the catalog read before.
fn reconnect_with_the_same_catalog(ctx: &archon_daemon::context::DaemonContext) {
    let client = ctx.aether.client().expect("connected");
    ctx.aether.replace(Some(client)).expect("reconnected");
    let connection = ctx.aether.generation().expect("a generation");
    let store = ctx.store.try_lock().expect("no store lock is held");
    let same = store.model_catalog();
    assert!(!same.is_empty(), "the catalog read before");
    store.record_model_catalog(same, archon_core::now(), Some(connection));
}

/// The stored request of the one reserved stage, which was never sent.
async fn unsent_request(h: &Harness) -> serde_json::Value {
    assert!(h.backend.requests().is_empty(), "no create was sent");
    let stages = h.live_stages().await;
    assert_eq!(stages.len(), 1, "the stage stays reserved");
    assert_eq!(stages[0].state, archon_core::run::StageState::Spawning);
    let key = format!("spawn_request:{}", stages[0].id);
    let stored: serde_json::Value = serde_json::from_str(&h.kv(&key).await.unwrap()).unwrap();
    stored["request"].clone()
}

/// A new stage's network is the default of the connection it is checked on:
/// a reconnect while that default is read freezes and sends nothing.
#[tokio::test]
async fn a_reconnect_while_a_new_stages_default_is_read_freezes_nothing() {
    use archon_core::ports::NetworkPolicy;
    let h = Harness::new().await;
    let ctx = h.ctx.clone();
    h.backend
        .on_next_network_default(move || reconnect_with_the_same_catalog(&ctx));
    h.enqueue("reconnect while a default is read").await;
    h.dispatch().await;
    let request = unsent_request(&h).await;
    assert!(request["network"].is_null(), "no network is frozen");
    assert!(request["parent"].is_null(), "no parent is frozen");

    h.backend.set_network_default(NetworkPolicy::Denied);
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(
        requests.len(),
        1,
        "created on the connection it is checked on"
    );
    assert_eq!(requests[0].network, Some(NetworkPolicy::Denied));
}

/// A reconnect after the default was read, before the binding is stored,
/// freezes nothing either: the default read belongs to the old connection.
#[tokio::test]
async fn a_reconnect_before_a_new_stage_is_bound_freezes_nothing() {
    use archon_core::ports::NetworkPolicy;
    let h = Harness::new().await;
    let ctx = h.ctx.clone();
    h.backend
        .on_host_context(1, move || reconnect_with_the_same_catalog(&ctx));
    h.enqueue("reconnect before a binding").await;
    h.dispatch().await;
    let request = unsent_request(&h).await;
    assert!(request["network"].is_null(), "no network is frozen");

    h.backend.set_network_default(NetworkPolicy::Denied);
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(
        requests.len(),
        1,
        "created on the connection it is checked on"
    );
    assert_eq!(requests[0].network, Some(NetworkPolicy::Denied));
}

/// A reserved create whose pin left the catalog is named like a pending one.
#[tokio::test]
async fn a_reserved_creates_retired_pin_is_named_with_a_re_plan_hint() {
    let h = Harness::new().await;
    let mut plan = common::plan();
    plan.0.insert(
        "implement".into(),
        vec![common::choice(HAIKU, archon_core::model::Effort::Medium)],
    );
    h.backend
        .fail_next(ArchonError::Unavailable("aetherd busy".into()));
    let item = h.enqueue_with("retire a reserved pin", plan).await.unwrap();
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1, "the create that failed");
    assert!(detail(&h, &item.id).await.waiting_reason.is_none());

    serve_catalog_without(&h, HAIKU).await;
    h.dispatch().await;
    assert_eq!(
        h.backend.requests().len(),
        1,
        "no create under a retired pin"
    );
    let stages = h.live_stages().await;
    assert_eq!(stages.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(stages[0].state, archon_core::run::StageState::Spawning);
    let wait = reason(&h, &item.id, Code::Blocked).await;
    assert!(wait.message.contains("valid model"), "{}", wait.message);
    assert!(wait.message.contains(HAIKU), "{}", wait.message);
    assert!(
        wait.next_action.contains("update_item"),
        "{}",
        wait.next_action
    );
}

const REVIEWED_THEN_POLISHED: &str = r#"[pipeline]
name = "reviewed-then-polished"
description = "A change whose review key no stage owes at this level, then a pass"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.review]
level = "none"
stage = "implement"

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
timeout_minutes = 60

[[stage]]
id = "polish"
kind = "work"
prompt = "implement.md"
timeout_minutes = 60
"#;

#[tokio::test]
async fn a_review_key_no_pending_stage_owes_holds_nothing() {
    let h = Harness::new().await;
    h.add_pipeline("reviewed-then-polished", REVIEWED_THEN_POLISHED);
    let medium = archon_core::model::Effort::Medium;
    let plan = archon_core::model::ModelPlan(std::collections::BTreeMap::from([
        (
            "implement".into(),
            vec![common::choice(common::MODEL, medium)],
        ),
        (
            "implement.review".into(),
            vec![common::choice(HAIKU, medium)],
        ),
        ("polish".into(), vec![common::choice(common::MODEL, medium)]),
    ]));
    let item = h
        .enqueue_in("reviewed-then-polished", "retire a finished reviewer", plan)
        .await
        .unwrap();
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{:?}", h.state(&item.id).await);
    assert_eq!(running[0].stage_id, "implement");
    assert!(
        !h.backend.requests()[0].instructions.contains(HAIKU),
        "no review is owed"
    );
    serve_catalog_without(&h, HAIKU).await;
    h.settle_running(passing).await;
    h.dispatch().await;
    let running = h.running().await;
    assert_eq!(
        running.len(),
        1,
        "the next stage starts: {:?}",
        h.state(&item.id).await
    );
    assert_eq!(running[0].stage_id, "polish");
    assert!(detail(&h, &item.id).await.waiting_reason.is_none());
}
