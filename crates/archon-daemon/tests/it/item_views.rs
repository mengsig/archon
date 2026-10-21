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
        error
            .to_string()
            .contains("is not a queue item id or its start"),
        "{error}"
    );
}

use archon_core::protocol::{
    ItemPatch, PauseTarget, RepoRef, WaitingReason, WaitingReasonCode as Code,
};
use archon_core::queue::{Caps, QueueState};

async fn connected_harness() -> (Harness, archon_aether::fake::FakeAether) {
    let h = Harness::new().await;
    let fake = archon_aether::fake::FakeAether::start(&h.dir.path().join("aether.sock"))
        .await
        .unwrap();
    let client = archon_aether::AetherClient::connect(&fake.endpoint())
        .await
        .unwrap();
    h.ctx
        .aether
        .replace(Some(std::sync::Arc::new(client)))
        .unwrap();
    (h, fake)
}

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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let (h, _fake) = connected_harness().await;
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
    let wait = reason(&h, &item.id, Code::AetherDisconnected).await;
    assert!(wait.message.contains("Aether disconnected"));
    assert_eq!(wait.next_action, "archon doctor");
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

#[tokio::test]
async fn delegated_descendant_cost_stays_with_its_run_after_retry_and_restart() {
    let mut h = Harness::new().await;
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
    let (h, _fake) = connected_harness().await;
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
