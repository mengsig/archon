//! `--json` on every command: one JSON value on stdout, or for the commands
//! that cannot print one, a refusal on stderr before anything starts.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};

use archon_core::aether_ids::AetherAgentId;
use archon_core::id::{QuestionId, QueueItemId};
use archon_core::model::{Effort, ModelRef};
use archon_core::notify::Question;
use archon_core::protocol::{
    ClientFrame, ErrorCode, ErrorReply, ItemDetail, OrchestratorInfo, PROTOCOL_VERSION, Reply,
    Request, ServerFrame, Welcome,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::steer::{SteerRecord, SteeredBy};
use common::{Env, git_repo, start_daemon, wait_for_exit};
use serde_json::Value;

/// `archon --json <args>`, which must succeed and print one JSON value.
fn json(env: &Env, args: &[&str]) -> Value {
    let argv = [&["--json"], args].concat();
    let out = env.run(&argv);
    parse(&argv, &out.stdout)
}

fn parse(argv: &[&str], stdout: &[u8]) -> Value {
    serde_json::from_slice(stdout).unwrap_or_else(|e| {
        let text = String::from_utf8_lossy(stdout);
        panic!("archon {argv:?} printed no JSON ({e}):\n{text}")
    })
}

#[test]
fn commands_without_json_output_refuse_the_flag() {
    let env = Env::new();
    let refused: [&[&str]; 7] = [
        &["--json"],
        &["--json", "chat"],
        &["--json", "tui"],
        &["--json", "aether", "agents"],
        &["--json", "mcp"],
        &["--json", "daemon", "run"],
        &["daemon", "run", "--json"],
    ];
    for args in refused {
        let out = env.command(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "archon {args:?}: {stderr}");
        let envelope = parse(args, &out.stdout);
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--json is not supported here")
        );
        assert_eq!(envelope["error"]["code"], "invalid_argument");
        assert!(stderr.is_empty(), "{stderr}");
    }
    assert!(
        !env.path("archond.sock").exists(),
        "a refused command started nothing"
    );
}

#[test]
fn setup_print_json_carries_the_block() {
    let env = Env::new();
    let printed = json(&env, &["setup", "--print"]);
    let block = printed["block"].as_str().expect("the block as a string");
    let table: toml::Table = toml::from_str(block).unwrap();
    assert_eq!(table["mcp"]["servers"][0]["name"].as_str(), Some("archon"));
}

#[test]
fn status_talk_and_daemon_commands_print_json() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    assert!(json(&env, &["status"])["caps"].is_object());
    assert_eq!(json(&env, &["questions"]), serde_json::json!([]));
    assert!(json(&env, &["log", "--limit", "5"]).is_array());
    assert_eq!(json(&env, &["orchestrator", "status"]), Value::Null);
    assert_eq!(json(&env, &["daemon", "status"])["running"], true);
    assert_eq!(json(&env, &["daemon", "start"])["running"], true);
    for args in [&["pause"][..], &["resume"], &["pause", "--pipeline", "ask"]] {
        assert_eq!(
            json(&env, args),
            serde_json::json!({"ok": true}),
            "{args:?}"
        );
    }
    assert!(json(&env, &["gc"])["removed"].is_array());
    let evidence = json(
        &env,
        &["model-evidence", "--since", "2w", "--stage", "review"],
    );
    assert_eq!(evidence["rows"].as_array().map(Vec::len), Some(0));
    assert_eq!(evidence["omitted"], 0);
    let cost = json(&env, &["cost", "--by", "repo", "--since", "2w"]);
    assert_eq!(
        (cost["by"].as_str(), cost["rows"].as_array().map(Vec::len)),
        (Some("repo"), Some(0)),
        "{cost}"
    );
    let registered = json(&env, &["setup"]);
    assert_eq!(registered["registration"], "added", "{registered}");
    // Aether is down, so doctor fails, but its report is still JSON.
    let out = env.command(&["--json", "doctor"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(parse(&["--json", "doctor"], &out.stdout).is_array());
    assert_eq!(json(&env, &["daemon", "stop"])["stopped"], true);
    wait_for_exit(daemon);
}

#[test]
fn repo_queue_pipeline_and_autonomy_commands_print_json() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let repo = env.path("demo");
    git_repo(&repo);
    let repo = repo.to_str().unwrap();
    assert_eq!(json(&env, &["repo", "add", repo])["name"], "demo");
    assert_eq!(
        json(&env, &["repo", "list"]).as_array().map(Vec::len),
        Some(1)
    );
    let set = json(&env, &["repo", "set", "demo", "--ship", "local"]);
    assert_eq!(set["name"], "demo", "{set}");

    assert_eq!(json(&env, &["queue", "list"]), serde_json::json!([]));
    let caps = json(&env, &["queue", "caps", "--max-per-repo", "2"]);
    assert_eq!(caps["max_per_repo"], 2, "{caps}");
    assert!(json(&env, &["pipelines", "list", "--repo", repo])["pipelines"].is_array());
    assert!(json(&env, &["pipelines", "check"])["valid"].is_u64());

    assert!(json(&env, &["autonomy", "--repo", "demo"])["policy"].is_object());
    let directive = json(&env, &["autonomy", "direct", "keep", "it", "small"]);
    assert_eq!(directive["text"], "keep it small", "{directive}");
    assert_eq!(json(&env, &["autonomy", "clear", "--all"])["cleared"], 1);
    let removed = json(&env, &["repo", "remove", "demo"]);
    assert_eq!(removed, serde_json::json!({"ok": true}));
    common::stop_daemon(&env, daemon);
}

/// Canned replies for the requests a real archond answers only with a live
/// orchestrator or an existing queue item.
struct Canned {
    item: QueueItem,
    question: Question,
    waiting_on: Option<archon_core::protocol::WaitingOn>,
    waiting_reason: Option<archon_core::protocol::WaitingReason>,
    status: Option<archon_core::protocol::DaemonStatus>,
}

impl Canned {
    fn new() -> Self {
        let now = archon_core::now();
        let item = QueueItem {
            onto: None,
            id: QueueItemId::new(),
            repo: None,
            pipeline: "ask".into(),
            title: "t".into(),
            brief: String::new(),
            priority: archon_core::queue::Priority::Normal,
            dependencies: vec![],
            tags: vec![],
            model_plan: archon_core::model::ModelPlan::default(),
            model_reasons: None,
            budget_usd: None,
            autonomy: archon_core::autonomy::Autonomy::default(),
            origin: None,
            stage_timeouts: Default::default(),
            fresh_retries: 0,
            routed_provider: None,
            provider_switch: None,
            initiative: None,
            state: QueueState::Queued,
            created_at: now,
            updated_at: now,
        };
        let question = Question {
            id: QuestionId::new(),
            number: 1,
            text: "Ship it?".into(),
            options: vec!["yes".into(), "no".into()],
            item: None,
            aether: None,
            asked_by: None,
            asked_at: now,
            answer: None,
            answered_at: None,
        };
        Self {
            item,
            question,
            waiting_on: None,
            waiting_reason: None,
            status: None,
        }
    }

    fn reply(&self, request: &Request) -> Result<Reply, ErrorReply> {
        Ok(match request {
            Request::Hello { .. } => Reply::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "fake".into(),
                pid: std::process::id(),
            }),
            Request::RepoList => Reply::Repos(vec![]),
            Request::Status(_) if self.status.is_some() => {
                Reply::Status(Box::new(self.status.clone().unwrap()))
            }
            Request::ListItemSummaries(_) => {
                Reply::ItemSummaries(vec![archon_core::protocol::ItemSummary {
                    item: self.item.clone(),
                    stage: self.waiting_on.as_ref().map(|w| w.stage.clone()),
                    spent_usd: 0.0,
                    pr_url: None,
                    waiting_on: self.waiting_on.clone(),
                    waiting_reason: self.waiting_reason.clone(),
                    switched_to: None,
                }])
            }
            Request::Say { .. } | Request::Answer { .. } => Reply::Ok,
            Request::ListQuestions { .. } => Reply::Questions(vec![self.question.clone()]),
            Request::GetItem { .. } => Reply::ItemDetail(Box::new(ItemDetail {
                default_efforts: Default::default(),
                steers: vec![SteerRecord {
                    at: self.item.created_at,
                    by: SteeredBy::Operator,
                    what: "amended the brief".into(),
                    note: Some("Use the v2 client.".into()),
                }],
                waiting_on: self.waiting_on.clone(),
                waiting_reason: self.waiting_reason.clone(),
                item: self.item.clone(),
                runs: vec![],
                stages: vec![],
                spent_usd: 0.0,
                gates: vec![],
                delegations: Default::default(),
                switched_to: None,
                merges: Vec::new(),
            })),
            Request::CancelItem { .. } | Request::RetryItem { .. } | Request::SteerItem { .. } => {
                Reply::Item(self.item.clone())
            }
            Request::OrchestratorRestart => Reply::Orchestrator(OrchestratorInfo {
                agent: AetherAgentId::parse("agt_fake").unwrap(),
                model: ModelRef::new("anthropic", "claude-test"),
                effort: Effort::High,
                status: "ready".into(),
                restarts: 1,
                generation: 2,
                freshness: None,
            }),
            other => {
                return Err(ErrorReply {
                    code: ErrorCode::Internal,
                    message: format!("the fake archond does not serve {other:?}"),
                });
            }
        })
    }

    /// Answer every frame on one connection until the client hangs up.
    fn serve(&self, stream: UnixStream) {
        let mut writer = stream.try_clone().unwrap();
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            let frame: ClientFrame = serde_json::from_str(&line).unwrap();
            let reply = ServerFrame::Reply {
                id: frame.id,
                result: self.reply(&frame.request),
            };
            let text = serde_json::to_string(&reply).unwrap();
            if writeln!(writer, "{text}").is_err() {
                return;
            }
        }
    }
}

#[test]
fn orchestrator_and_item_commands_print_json() {
    let env = Env::new();
    let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
    let canned = Canned::new();
    let (item, question) = (canned.item.id.to_string(), canned.question.id.to_string());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            canned.serve(stream.unwrap());
        }
    });
    assert_eq!(
        json(&env, &["say", "hello"]),
        serde_json::json!({"ok": true})
    );
    let answered = json(&env, &["answer", "Q1", "2"]);
    let expected = serde_json::json!({"question": question, "number": 1, "answer": "no"});
    assert_eq!(answered, expected);
    assert_eq!(json(&env, &["queue", "show", &item])["item"]["id"], item);
    for verb in ["cancel", "retry"] {
        assert_eq!(json(&env, &["queue", verb, &item])["id"], item, "{verb}");
    }
    let steer = ["queue", "steer", &item, "--note", "Use the v2 client."];
    assert_eq!(json(&env, &steer)["id"], item);
    let said = String::from_utf8(env.run(&steer).stdout).unwrap();
    assert_eq!(said, format!("Steered {item}: amended the brief.\n"));
    let shown = String::from_utf8(env.run(&["queue", "show", &item]).stdout).unwrap();
    assert!(
        shown.contains(" by operator: amended the brief\n  note: Use the v2 client.\n"),
        "{shown}"
    );
    let empty = env.command(&["queue", "steer", &item]).output().unwrap();
    assert_eq!(empty.status.code(), Some(1), "a steer changes something");
    let restarted = json(&env, &["orchestrator", "restart"]);
    assert_eq!(restarted["agent"], "agt_fake", "{restarted}");
}

#[test]
fn json_errors_keep_codes_messages_and_exit_status() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    for (argument, code) in [
        ("nonexistent", "invalid_argument"),
        ("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "not_found"),
    ] {
        let out = env
            .command(&["--json", "queue", "show", argument])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let value = parse(&["queue", "show"], &out.stdout);
        assert_eq!(value["error"]["code"], code, "{value}");
        assert!(!value["error"]["message"].as_str().unwrap().is_empty());
        assert!(out.stderr.is_empty(), "{:?}", out.stderr);
    }
    let out = env
        .command(&["queue", "show", "nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("archon: "));
    common::stop_daemon(&env, daemon);
}

#[test]
fn a_command_line_that_does_not_parse_is_an_envelope_under_json() {
    let env = Env::new();
    let cases: [&[&str]; 4] = [
        &["--json", "queue", "steer", "qi_x", "--budget-usd", "-1"],
        &["queue", "steer", "qi_x", "--budget-usd", "-1", "--json"],
        &["--json", "no-such-command"],
        &["--json", "initiatives", "update", "ini_x"],
    ];
    for args in cases {
        let out = env.command(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let value = parse(args, &out.stdout);
        assert_eq!(value["error"]["code"], "invalid_argument", "{value}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            !message.is_empty() && !message.starts_with("error:"),
            "{message}"
        );
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
    }
    let plain = env
        .command(&["queue", "steer", "qi_x", "--budget-usd", "-1"])
        .output()
        .unwrap();
    assert_eq!(plain.status.code(), Some(2));
    assert!(plain.stdout.is_empty(), "{plain:?}");
    assert!(String::from_utf8_lossy(&plain.stderr).starts_with("error: "));
    let help = env.command(&["--json", "--help"]).output().unwrap();
    assert!(help.status.success(), "help is still help: {help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));
}

#[test]
fn caller_errors_have_typed_json_categories() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let cases: &[(&[&str], &str)] = &[
        (
            &["queue", "caps", "--max-parallel-agents", "0"],
            "invalid_argument",
        ),
        (
            &["queue", "caps", "--budget-daily", "NaN"],
            "invalid_argument",
        ),
        (
            &["queue", "caps", "--budget-daily", "inf"],
            "invalid_argument",
        ),
        (&["queue", "caps", "--budget-daily=-1"], "invalid_argument"),
        (&["answer", "999", "approve"], "invalid_transition"),
        (&["answer", "bad-reference", "approve"], "invalid_argument"),
        (&["answer", "1"], "invalid_argument"),
        (&["say"], "invalid_argument"),
        (&["log", "--limit", "0"], "invalid_argument"),
        (&["reports", "reopen", "bad-id"], "invalid_argument"),
        (&["autonomy", "clear", "bad-id"], "invalid_argument"),
        (&["github-watcher", "add", "bad-repo"], "invalid_argument"),
        (
            &["github-watcher", "allow", "o/r", "bad_login"],
            "invalid_argument",
        ),
        (&["queue", "list", "--state", "typo"], "invalid_argument"),
        (
            &["pause", "--hard", "--pipeline", "ask"],
            "invalid_argument",
        ),
        (&["repo", "set", "missing"], "not_found"),
        (&["pipelines", "list", "--repo", "missing"], "not_found"),
        (&["setup", "--resync"], "invalid_argument"),
        (&["daemon", "stop", "--aether"], "invalid_argument"),
    ];
    for &(args, code) in cases {
        let argv = [&["--json"], args].concat();
        let out = env.command(&argv).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{argv:?}: {out:?}");
        let value = parse(&argv, &out.stdout);
        assert_eq!(value["error"]["code"], code, "{argv:?}: {value}");
        assert!(!value["error"]["message"].as_str().unwrap().is_empty());
        assert!(out.stderr.is_empty(), "{argv:?}: {out:?}");
    }
    common::stop_daemon(&env, daemon);
}

#[test]
fn queue_and_status_views_put_the_current_operator_wait_first() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let status = json(&env, &["status"]);
    common::stop_daemon(&env, daemon);
    let mut canned = Canned::new();
    canned.item.state = QueueState::Running {
        run: archon_core::id::PipelineRunId::new(),
    };
    canned.waiting_on = Some(archon_core::protocol::WaitingOn {
        subject: "report".into(),
        stage: "report".into(),
        question: Some(canned.question.id.clone()),
        prompt: format!(
            "Approve this change report? {}",
            "Full proposal evidence.".repeat(100)
        ),
        next_action: format!("archon answer {} approve", canned.question.id),
    });
    let mut status: archon_core::protocol::DaemonStatus = serde_json::from_value(status).unwrap();
    status.open_gates = 1;
    status.open_questions = 1;
    status.waiting_items = vec![archon_core::protocol::ItemSummary {
        item: canned.item.clone(),
        stage: Some("report".into()),
        spent_usd: 0.0,
        pr_url: None,
        waiting_on: canned.waiting_on.clone(),
        waiting_reason: None,
        switched_to: None,
    }];
    canned.status = Some(status);
    let id = canned.item.id.to_string();
    let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            canned.serve(stream.unwrap());
        }
    });
    let show = String::from_utf8(env.run(&["queue", "show", &id]).stdout).unwrap();
    assert_eq!(
        show.lines().nth(1),
        Some("state: awaiting approval (report)")
    );
    assert!(
        show.lines()
            .nth(2)
            .unwrap()
            .contains("Full proposal evidence.")
    );
    assert!(
        show.lines()
            .nth(3)
            .unwrap()
            .starts_with("next: archon answer q_")
    );
    for args in [&["queue", "list"][..], &["status", "--no-start"]] {
        let text = String::from_utf8(env.run(args).stdout).unwrap();
        assert!(
            text.lines().next().unwrap().contains("awaiting approval"),
            "{text}"
        );
        let row = text.lines().next().unwrap();
        assert!(row.contains("report — next: archon answer q_"), "{row}");
        assert!(!row.contains("Full proposal evidence."), "{row}");
        assert!(row.len() < 300, "{row}");
    }
    let show = json(&env, &["queue", "show", &id]);
    let list = json(&env, &["queue", "list"]);
    assert_eq!(show["item"]["state"]["state"], "running");
    assert_eq!(list[0]["state"]["state"], "running");
    assert_eq!(show["waiting_on"], list[0]["waiting_on"]);
}

#[test]
fn answered_questions_and_empty_options_have_typed_json_errors() {
    for (answered, expected) in [(true, "invalid_transition"), (false, "invalid_argument")] {
        let env = Env::new();
        let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
        let mut canned = Canned::new();
        canned.question.options = vec![String::new()];
        if answered {
            canned.question.answer = Some("approved earlier".into());
            canned.question.answered_at = Some(archon_core::now());
        }
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            canned.serve(stream);
        });
        let argv = ["--json", "answer", "1", "1"];
        let out = env.command(&argv).output().unwrap();
        server.join().unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(parse(&argv, &out.stdout)["error"]["code"], expected);
        assert!(out.stderr.is_empty(), "{out:?}");
    }
}

#[test]
fn queued_waiting_reason_is_visible_in_queue_and_status_text_and_json() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let status = json(&env, &["status"]);
    common::stop_daemon(&env, daemon);
    let mut canned = Canned::new();
    canned.item.state = QueueState::Queued;
    canned.waiting_reason = Some(archon_core::protocol::WaitingReason {
        code: archon_core::protocol::WaitingReasonCode::TotalBudget,
        message: "Total budget reached ($5.00 / $5.00)".into(),
        next_action: "archon queue caps --budget-total 6.00".into(),
    });
    let mut status: archon_core::protocol::DaemonStatus = serde_json::from_value(status).unwrap();
    status.waiting_items = vec![archon_core::protocol::ItemSummary {
        item: canned.item.clone(),
        stage: None,
        spent_usd: 0.0,
        pr_url: None,
        waiting_on: None,
        waiting_reason: canned.waiting_reason.clone(),
        switched_to: None,
    }];
    canned.status = Some(status);
    let id = canned.item.id.to_string();
    let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            canned.serve(stream.unwrap());
        }
    });
    for args in [
        &["queue", "list"][..],
        &["queue", "show", &id],
        &["status", "--no-start"],
    ] {
        let output = env.run(args);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Total budget reached"), "{text}");
        assert!(
            text.contains("archon queue caps --budget-total 6.00"),
            "{text}"
        );
        assert!(!text.contains("awaiting approval"), "{text}");
    }
    let show = json(&env, &["queue", "show", &id]);
    let list = json(&env, &["queue", "list"]);
    let status = json(&env, &["status", "--no-start"]);
    assert_eq!(show["waiting_reason"]["code"], "total_budget");
    assert_eq!(show["waiting_reason"], list[0]["waiting_reason"]);
    assert_eq!(
        show["waiting_reason"],
        status["waiting_items"][0]["waiting_reason"]
    );
    assert_eq!(show["item"]["state"]["state"], "queued");
}
