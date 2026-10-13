use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use archon_aether::fake::{self, FakeAether, Scripted};
use archon_aether::wire::{ApprovalPolicy, NetworkPolicy, TaskResult, TaskStatus};
use archon_core::aether_ids::AetherAgentId;
use archon_core::id::{FeedItemId, NotificationId, PipelineRunId, QueueItemId, RepoId};
use archon_core::model::ModelPlan;
use archon_core::notify::{FeedEntry, FeedItem, Notification, NotificationKind};
use archon_core::protocol::ItemDetail;
use archon_core::queue::{Priority, QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
use serde_json::json;

use super::archon::{
    Daemon, Observation, SETTLE_WINDOW, Settle, answer_text, operator_messages, result_branches,
};
use super::*;
use crate::task::BenchTask;
use crate::workspace::TrialLayout;
use crate::workspace::tests::scratch;

pub(crate) fn settings() -> ArmSettings {
    ArmSettings {
        claude_bin: "claude".into(),
        codex_bin: "codex".into(),
        archon_bin: "archon".into(),
        aether_socket: Some("/run/aether.sock".into()),
        archon_model: "claude-code/claude-opus-5-5".parse().unwrap(),
        aether_model: "claude-code/claude-opus-5-5".parse().unwrap(),
        claude_model: "claude-opus-5-5".into(),
        codex_model: "gpt-5.6-sol".into(),
        effort: Effort::High,
        prices: vec!["gpt-5.6-sol=1,0.1,10".parse().unwrap()],
        env: EnvScrub::default(),
    }
}

fn bundled(id: &str) -> BenchTask {
    BenchTask::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks").join(id)).unwrap()
}

fn args(spec: &CommandSpec) -> Vec<&str> {
    spec.args.iter().map(String::as_str).collect()
}

#[test]
fn claude_command_pins_model_effort_and_permissions() {
    let cwd = Path::new("/w/run/ws/fixture");
    let spec = claude_command(&settings(), "Fix it.", cwd, Path::new("/w/run/ws"));
    assert_eq!(
        args(&spec),
        [
            "-p",
            "Fix it.",
            "--output-format",
            "json",
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high",
            "--dangerously-skip-permissions",
            "--no-session-persistence",
        ]
    );
    assert_eq!(spec.cwd, cwd);
    assert!(spec.env_remove.contains(&"CLAUDECODE".to_owned()));
    assert_eq!(
        spec.env,
        [("GIT_CEILING_DIRECTORIES".to_owned(), "/w/run/ws".to_owned())]
    );
}

#[test]
fn codex_command_runs_exec_json_in_the_workspace() {
    let mut s = settings();
    s.effort = Effort::Max;
    let cwd = Path::new("/w/run/ws");
    let spec = codex_command(&s, "Do it.", cwd, cwd);
    assert_eq!(
        args(&spec),
        [
            "exec",
            "--json",
            "--dangerously-bypass-approvals-and-sandbox",
            "--skip-git-repo-check",
            "--ephemeral",
            "-m",
            "gpt-5.6-sol",
            "-c",
            "model_reasoning_effort=\"xhigh\"",
            "-C",
            "/w/run/ws",
            "Do it.",
        ]
    );
    assert_eq!(
        spec.env[0].1, "/w/run/ws",
        "a multi-repo cwd stops at the trial workspace"
    );
}

#[test]
fn agent_commands_drop_variables_pointing_at_the_checkout() {
    let mut s = settings();
    s.env = EnvScrub {
        remove: vec!["CARGO_MANIFEST_DIR".into(), "OLDPWD".into()],
        set: vec![("PATH".into(), "/usr/bin".into())],
    };
    let ws = Path::new("/w/run/ws");
    for spec in [
        claude_command(&s, "Fix it.", &ws.join("r"), ws),
        codex_command(&s, "Fix it.", ws, ws),
    ] {
        assert!(spec.env_remove.contains(&"CARGO_MANIFEST_DIR".to_owned()));
        assert!(spec.env_remove.contains(&"CLAUDECODE".to_owned()));
        let env: BTreeMap<_, _> = spec.env.iter().cloned().collect();
        assert_eq!(env["PATH"], "/usr/bin");
        assert_eq!(env["GIT_CEILING_DIRECTORIES"], "/w/run/ws");
    }
    let daemon = archon_daemon_command(&s, &ws.join(".archon"), Path::new("/s.sock"));
    assert!(daemon.env_remove.contains(&"CARGO_MANIFEST_DIR".to_owned()));
}

#[test]
#[should_panic(expected = "inside its trial workspace")]
fn agents_never_start_outside_their_workspace() {
    claude_command(
        &settings(),
        "Fix it.",
        Path::new("/w/other"),
        Path::new("/w/run/ws"),
    );
}

#[test]
fn effort_maps_onto_each_cli() {
    assert_eq!(claude_effort(Effort::XHigh), "xhigh");
    assert_eq!(claude_effort(Effort::Max), "max");
    assert_eq!(codex_effort(Effort::Low), "low");
    assert_eq!(codex_effort(Effort::Max), "xhigh");
}

#[test]
#[should_panic(expected = "flag")]
fn prompts_that_look_like_flags_are_refused() {
    claude_command(&settings(), "--help", Path::new("/w"), Path::new("/w"));
}

#[test]
fn model_labels_and_price_estimates() {
    let s = settings();
    assert_eq!(s.model_label(Arm::Claude), "claude-code/claude-opus-5-5");
    assert_eq!(s.model_label(Arm::Codex), "codex/gpt-5.6-sol");
    let usage = Usage {
        input_tokens: 2_000_000,
        cached_input_tokens: 1_000_000,
        output_tokens: 100_000,
        ..Usage::default()
    };
    let priced = s.price(Arm::Codex, usage);
    let estimate = priced.estimated_cost_usd.unwrap();
    assert!((estimate - (1.0 + 0.1 + 1.0)).abs() < 1e-9, "{estimate}");
    assert_eq!(s.price(Arm::Claude, usage).estimated_cost_usd, None);
    let billed = Usage {
        cost_usd: Some(0.5),
        ..Usage::default()
    };
    assert_eq!(s.price(Arm::Codex, billed), billed);
}

#[test]
fn aether_request_is_one_leaf_agent_without_archon_tools() {
    let task = bundled("fix-off-by-one");
    let layout = TrialLayout::new("/w/ws".into(), "/w/g".into(), "/r/logs".into());
    let s = settings();
    let input = TrialInput {
        task: &task,
        layout: &layout,
        settings: &s,
        timeout: Duration::from_secs(60),
        key: "k1",
    };
    let request = aether_request(&input, archon_core::now());
    assert_eq!(
        request.objective.as_deref(),
        Some(task.file.prompt.as_str())
    );
    assert_eq!(request.idempotency_key.as_deref(), Some("archon-bench-k1"));
    assert!(request.parent.is_none() && request.start);
    let scope = request.scope.as_ref().unwrap();
    assert_eq!(
        scope.network,
        NetworkPolicy::Allowed,
        "same reach as the other arms"
    );
    assert_eq!(request.approval, Some(ApprovalPolicy::none()));
    let config = request.config.unwrap();
    assert!(config.leaf && !config.long_lived);
    assert_eq!(config.effort, Some(Effort::High));
    let ToolExposure::Except { names } = config.tools else {
        panic!("archon tools must be hidden");
    };
    assert!(names.iter().all(|n| n.starts_with("mcp__archon__")) && !names.is_empty());
    let scope = request.scope.unwrap();
    assert_eq!(scope.workspace_root, Some(layout.agent_cwd(&task)));
}

#[test]
fn archon_config_pins_the_model_and_round_trips() {
    let s = settings();
    let text = archon_config_toml(&s).unwrap();
    let parsed: Config = toml::from_str(&text).unwrap();
    assert_eq!(parsed, archon_config(&s));
    assert_eq!(parsed.models.allow, std::slice::from_ref(&s.archon_model));
    assert_eq!(parsed.models.max_effort, Some(Effort::High));
    assert_eq!(parsed.lavish.enabled, Toggle::Off);
    assert!(parsed.worktrees.keep_on_success);
}

#[test]
fn archon_daemon_gets_a_private_config_and_data_dir() {
    let spec = archon_daemon_command(
        &settings(),
        Path::new("/w/t/archon/1/archon"),
        Path::new("/s.sock"),
    );
    assert_eq!(args(&spec), ["daemon", "run"]);
    let env: BTreeMap<_, _> = spec.env.iter().cloned().collect();
    assert_eq!(env["ARCHON_CONFIG_DIR"], "/w/t/archon/1/archon/config");
    assert_eq!(env["ARCHON_DATA_DIR"], "/w/t/archon/1/archon/data");
    assert_eq!(env["ARCHON_SOCKET"], "/s.sock");
}

fn item(state: QueueState, repo: Option<RepoId>, minute: i64) -> QueueItem {
    let at = archon_core::now() + chrono::Duration::minutes(minute);
    QueueItem {
        id: QueueItemId::new(),
        repo,
        pipeline: "default-pr-review".into(),
        title: "t".into(),
        brief: "b".into(),
        priority: Priority::Normal,
        dependencies: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default(),
        model_reasons: None,
        budget_usd: None,
        state,
        autonomy: Default::default(),
        origin: None,
        fresh_retries: 0,
        created_at: at,
        updated_at: at,
    }
}

fn succeeded(repo: &RepoId, branch: &str, minute: i64) -> ItemDetail {
    let run = PipelineRunId::new();
    let summary = format!("merged {branch}");
    let item = item(
        QueueState::Succeeded {
            run: run.clone(),
            summary: summary.clone(),
        },
        Some(repo.clone()),
        minute,
    );
    let pipeline_run = PipelineRun {
        id: run,
        item: item.id.clone(),
        pipeline: item.pipeline.clone(),
        shipped: false,
        aether_run: None,
        workspace: "/w".into(),
        team_root: None,
        branch: branch.into(),
        state: RunState::Succeeded { summary },
        cost_usd: 0.0,
        started_at: item.created_at,
        finished_at: Some(item.updated_at),
    };
    ItemDetail {
        waiting_on: None,
        waiting_reason: None,
        item,
        runs: vec![pipeline_run],
        stages: Vec::new(),
        spent_usd: 0.0,
        gates: Vec::new(),
        delegations: Default::default(),
    }
}

fn feed(seq: u64, entry: FeedEntry) -> FeedItem {
    FeedItem {
        id: FeedItemId::new(),
        seq,
        at: archon_core::now(),
        entry,
    }
}

fn notification(kind: NotificationKind, text: &str) -> FeedEntry {
    FeedEntry::Notification(Notification {
        id: NotificationId::new(),
        kind,
        text: text.into(),
        item: None,
        run: None,
        at: archon_core::now(),
    })
}

#[test]
fn settle_needs_quiet_for_a_whole_window() {
    let running = item(
        QueueState::Running {
            run: PipelineRunId::new(),
        },
        None,
        0,
    );
    let blocked = item(
        QueueState::Blocked {
            reason: "dep".into(),
        },
        None,
        0,
    );
    let busy = Observation {
        replied: true,
        orchestrator_busy: false,
        items: vec![running],
    };
    let quiet = Observation {
        replied: false,
        orchestrator_busy: false,
        items: vec![blocked],
    };
    let nothing_yet = Observation {
        replied: false,
        orchestrator_busy: false,
        items: Vec::new(),
    };
    assert!(!busy.quiet() && quiet.quiet() && !nothing_yet.quiet());
    let t0 = Instant::now();
    let mut settle = Settle::default();
    assert!(!settle.update(&quiet, t0));
    assert!(
        !settle.update(&busy, t0 + SETTLE_WINDOW),
        "activity restarts the window"
    );
    assert!(!settle.update(&quiet, t0 + SETTLE_WINDOW * 2));
    assert!(settle.update(&quiet, t0 + SETTLE_WINDOW * 3));
    assert_eq!(
        settle.quiet_since(),
        Some(t0 + SETTLE_WINDOW * 2),
        "wall time ends where the quiet began, not after the window"
    );
}

#[test]
fn operator_messages_count_what_the_quiet_chat_shows() {
    let items = [
        feed(
            1,
            FeedEntry::Operator {
                text: "do it".into(),
            },
        ),
        feed(
            2,
            FeedEntry::Orchestrator {
                text: "Queued two items.".into(),
            },
        ),
        feed(3, notification(NotificationKind::Progress, "50%")),
        feed(4, notification(NotificationKind::Outcome, "item done")),
    ];
    assert_eq!(operator_messages(&items), 2);
    let repo = RepoId::new();
    let text = answer_text(&items, &[succeeded(&repo, "archon/x", 0)]);
    assert_eq!(text, "Queued two items.\n\nitem done\n\nmerged archon/x");
}

#[test]
fn result_branches_follow_completion_order_per_repo() {
    let (api, client) = (RepoId::new(), RepoId::new());
    let repos: BTreeMap<RepoId, String> = [
        (api.clone(), "api".into()),
        (client.clone(), "client".into()),
    ]
    .into();
    let failed = ItemDetail {
        waiting_on: None,
        waiting_reason: None,
        item: item(
            QueueState::Failed {
                run: None,
                error: "e".into(),
            },
            Some(api.clone()),
            0,
        ),
        runs: Vec::new(),
        stages: Vec::new(),
        spent_usd: 0.0,
        gates: Vec::new(),
        delegations: Default::default(),
    };
    let details = [
        succeeded(&api, "archon/late", 5),
        failed,
        succeeded(&api, "archon/early", 1),
        succeeded(&client, "archon/c", 2),
    ];
    let branches = result_branches(&details, &repos);
    assert_eq!(branches["api"], ["archon/early", "archon/late"]);
    assert_eq!(branches["client"], ["archon/c"]);
}

/// An `ask` run answers without a branch; there is nothing to merge.
#[test]
fn result_branches_skip_answer_only_runs() {
    let repo = RepoId::new();
    let repos: BTreeMap<RepoId, String> = [(repo.clone(), "lib".into())].into();
    let details = [succeeded(&repo, "", 1), succeeded(&repo, "archon/fix", 2)];
    let branches = result_branches(&details, &repos);
    assert_eq!(branches["lib"], ["archon/fix"]);
    let answer_only = result_branches(&details[..1], &repos);
    assert!(answer_only.is_empty(), "{answer_only:?}");
}

#[test]
fn settle_text_blames_the_harness_only_for_unexplained_cancels() {
    let messages = vec!["All tests pass.".to_owned()];
    let done = TaskStatus::Completed {
        result: TaskResult {
            summary: "Fixed window.".into(),
            artifacts: Vec::new(),
            structured: None,
        },
    };
    let (answer, error) = aether::settle_text(&done, &messages, false);
    assert_eq!(answer, "Fixed window.\n\nAll tests pass.");
    assert!(error.is_none());
    let cancelled = TaskStatus::Cancelled {
        reason: Some("operator".into()),
    };
    assert!(aether::settle_text(&cancelled, &messages, true).1.is_none());
    assert!(
        aether::settle_text(&cancelled, &messages, false)
            .1
            .is_some()
    );
    assert!(
        aether::settle_text(&TaskStatus::Active, &[], false)
            .1
            .is_some()
    );
}

#[test]
fn assistant_messages_keep_only_the_agents_own_text() {
    let agent = AetherAgentId::parse("agt_root").unwrap();
    let events = [
        json!({"seq": 1, "agent_id": "agt_root", "kind": {"type": "agent.message", "role": "assistant", "content": "hi"}}),
        json!({"seq": 2, "agent_id": "agt_child", "kind": {"type": "agent.message", "role": "assistant", "content": "child"}}),
        json!({"seq": 3, "agent_id": "agt_root", "kind": {"type": "agent.message", "role": "user", "content": "task"}}),
        json!({"seq": 4, "agent_id": "agt_root", "kind": {"type": "agent.message", "role": "assistant",
            "content": [{"type": "text", "text": "done"}, {"type": "tool_use", "name": "x"}]}}),
        json!({"seq": 5, "agent_id": "agt_root", "kind": {"type": "agent.message", "role": "assistant", "content": " "}}),
    ];
    assert_eq!(aether::assistant_messages(&events, &agent), ["hi", "done"]);
}

#[tokio::test]
async fn aether_arm_reads_usage_and_answer_from_the_daemon() {
    let sockets = tempfile::tempdir().unwrap();
    let daemon = FakeAether::start(&sockets.path().join("aether.sock"))
        .await
        .unwrap();
    daemon.script(
        "create_agent",
        Scripted::reply("agent", fake::agent("agt_b1", Some("tsk_b1"))),
    );
    let completed = json!({"state": "completed", "result": {"summary": "Fixed the window."}});
    daemon.script(
        "get_task",
        Scripted::reply(
            "task",
            fake::task("tsk_b1", Some("agt_b1"), "run_1", completed),
        ),
    );
    daemon.script("get_agent", Scripted::reply("agent", agent_in("completed")));
    let usage = json!({"type": "agent.usage_updated", "total": {"input_tokens": 10, "output_tokens": 50,
        "cache_read_tokens": 1000, "cache_write_tokens": 90, "cost_usd": 0.0, "cost_complete": false,
        "estimated_cost_usd": 0.12}});
    let message = json!({"type": "agent.message", "role": "assistant", "content": "Tests pass."});
    let events = json!([usage_event(7, usage), usage_event(9, message),]);
    daemon.script("get_events", Scripted::reply("events", events));

    let tmp = scratch();
    let task = bundled("fix-off-by-one");
    let layout = TrialLayout::under(&tmp.path().join("t"));
    crate::workspace::prepare(&task, &layout).unwrap();
    let mut s = settings();
    s.aether_socket = Some(daemon.socket_path().to_path_buf());
    let input = TrialInput {
        task: &task,
        layout: &layout,
        settings: &s,
        timeout: Duration::from_secs(60),
        key: "k",
    };
    let outcome = aether::run_aether(&input).await;

    assert_eq!(outcome.error, None);
    assert_eq!(outcome.answer, "Fixed the window.\n\nTests pass.");
    assert_eq!(outcome.operator_messages, 1);
    let usage = outcome.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        ),
        (1100, 1000, 50)
    );
    assert_eq!(usage.estimated_cost_usd, Some(0.12));
    let created = daemon.received_named("create_agent");
    assert_eq!(created[0]["idempotency_key"], "archon-bench-k");
    assert_eq!(created[0]["config"]["leaf"], true);
}

/// The final message lands after `complete_task`, while the agent's turn
/// still runs; events are read only once that turn has ended.
#[tokio::test]
async fn aether_arm_reads_events_after_the_final_turn_ends() {
    let sockets = tempfile::tempdir().unwrap();
    let daemon = FakeAether::start(&sockets.path().join("aether.sock"))
        .await
        .unwrap();
    daemon.script(
        "create_agent",
        Scripted::reply("agent", fake::agent("agt_b1", Some("tsk_b1"))),
    );
    let completed = json!({"state": "completed", "result": {"summary": "Fixed."}});
    daemon.script(
        "get_task",
        Scripted::reply(
            "task",
            fake::task("tsk_b1", Some("agt_b1"), "run_1", completed),
        ),
    );
    for state in ["running", "running", "completed"] {
        daemon.script("get_agent", Scripted::reply("agent", agent_in(state)));
    }
    let message = json!({"type": "agent.message", "role": "assistant", "content": "Details."});
    daemon.script(
        "get_events",
        Scripted::reply("events", json!([usage_event(3, message)])),
    );

    let tmp = scratch();
    let task = bundled("fix-off-by-one");
    let layout = TrialLayout::under(&tmp.path().join("t"));
    crate::workspace::prepare(&task, &layout).unwrap();
    let mut s = settings();
    s.aether_socket = Some(daemon.socket_path().to_path_buf());
    let input = TrialInput {
        task: &task,
        layout: &layout,
        settings: &s,
        timeout: Duration::from_secs(60),
        key: "k",
    };
    let outcome = aether::run_aether(&input).await;

    assert_eq!(outcome.error, None);
    assert_eq!(outcome.answer, "Fixed.\n\nDetails.");
    let order: Vec<String> = daemon
        .received()
        .iter()
        .filter_map(|r| r["cmd"].as_str().map(str::to_owned))
        .filter(|c| c == "get_agent" || c == "get_events")
        .collect();
    assert_eq!(order, ["get_agent", "get_agent", "get_agent", "get_events"]);
}

fn agent_in(state: &str) -> serde_json::Value {
    let mut agent = fake::agent("agt_b1", Some("tsk_b1"));
    agent["status"] = json!({ "state": state });
    agent
}

fn usage_event(seq: u64, kind: serde_json::Value) -> serde_json::Value {
    let mut event = fake::event(Some("agt_b1"), Some("tsk_b1"), kind);
    event["seq"] = json!(seq);
    event
}

#[tokio::test]
async fn archon_teardown_cancels_the_orchestrator_only_after_archond_exits() {
    let sockets = tempfile::tempdir().unwrap();
    let aether = FakeAether::start(&sockets.path().join("aether.sock"))
        .await
        .unwrap();
    let exited = sockets.path().join("archond-exited");
    let cancelled_after_exit = Arc::new(std::sync::Mutex::new(None));
    let seen = Arc::clone(&cancelled_after_exit);
    let marker = exited.clone();
    aether.set_responder(
        "cancel",
        Arc::new(move |_| {
            *seen.lock().unwrap() = Some(marker.exists());
            Scripted::ok()
        }),
    );
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("sleep 0.3; touch '{}'", exited.display()))
        .spawn()
        .unwrap();
    let mut archond = Daemon { child };
    let orchestrator = AetherAgentId::parse("agt_orch").unwrap();

    archond
        .exit_then_cancel(Some(&orchestrator), Some(aether.socket_path()), Vec::new())
        .await
        .unwrap();

    assert_eq!(*cancelled_after_exit.lock().unwrap(), Some(true));
    let cancels = aether.received_named("cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["agent"], "agt_orch");
}
