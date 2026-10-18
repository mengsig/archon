//! The operator's `/answer` chat command and the orchestrator's
//! `record_chat_answer` (ARCHITECTURE §5.2): who may pass a gate from the
//! orchestrator's Aether chat, and what every other sender is refused.

mod common;

use archon_aether::AetherEvent;
use archon_aether::events::parse_event;
use archon_core::aether_ids::{AetherAgentId, AetherQuestionRef};
use archon_core::autonomy::{AutonomyPatch, ProposalMode, ReportMode};
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, QueueItemId};
use archon_core::notify::{AnswerVia, FeedEntry, Question};
use archon_core::protocol::{ClientKind, EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{GateDecision, GateSubject, OperatorGate, StageOutcome, StageRun};
use archon_daemon::triggers::{self, TriggerEvent};
use archon_daemon::{handlers, pump};
use archon_queue::store::keys;
use common::{Harness, PIPELINE, REPO_NAME, completed, plan, verdict};
use serde_json::{Value, json};

const ORCH: &str = "agt_orch";
const PROPOSAL: &str = "Approach: add a greeting flag.";

async fn harness(mode: AetherMode) -> Harness {
    let mut config = Config::default();
    config.aether.mode = mode;
    let h = Harness::with_config(config).await;
    let mut store = h.ctx.store.lock().await;
    store.kv_set(keys::ORCHESTRATOR_AGENT, ORCH).expect("kv");
    drop(store);
    h
}

/// An `inbox.delivered` on the orchestrator's stream carrying `kind`.
fn delivered(seq: u64, kind: Value) -> AetherEvent {
    let event = json!({
        "id": format!("evt_{seq}"), "seq": seq, "at": "2026-10-12T10:00:00Z",
        "actor": {"type": "system"}, "agent_id": ORCH,
        "kind": {"type": "inbox.delivered", "item": {
            "id": format!("inb_{seq}"), "agent_id": ORCH, "seq": seq,
            "delivered_at": "2026-10-12T10:00:00Z", "kind": kind}}
    });
    parse_event(event).expect("a valid event")
}

/// A chat `message` from `actor` whose whole text is `text`.
fn message(actor: Value, text: &str) -> Value {
    json!({"kind": "message", "message_id": "msg_1", "from": actor,
           "content": [{"type": "text", "text": text}]})
}

fn operator(text: &str) -> Value {
    message(json!({"actor": "user"}), text)
}

async fn deliver(h: &Harness, seq: u64, kind: Value) {
    pump::handle_orchestrator_event(&h.ctx, &delivered(seq, kind))
        .await
        .expect("the pump handles the event");
}

async fn held(h: &Harness) -> Vec<TriggerEvent> {
    triggers::held(&h.ctx).await.expect("held news")
}

/// `(number, applied, text)` of every `chat_answer` held for the orchestrator.
async fn chat_news(h: &Harness) -> Vec<(Option<u64>, bool, String)> {
    let news = held(h).await.into_iter().filter_map(|event| match event {
        TriggerEvent::ChatAnswer {
            number,
            applied,
            text,
        } => Some((number, applied, text)),
        _ => None,
    });
    news.collect()
}

async fn feed(h: &Harness) -> Vec<FeedEntry> {
    let store = h.ctx.store.lock().await;
    let items = store.feed_after(0, 10_000).expect("feed");
    items.into_iter().map(|item| item.entry).collect()
}

/// `(text, via)` of the `Answer` feed entries of `question`.
async fn answers(h: &Harness, question: &QuestionId) -> Vec<(String, AnswerVia)> {
    let entries = feed(h).await.into_iter().filter_map(|entry| match entry {
        FeedEntry::Answer {
            question: q,
            text,
            via,
        } if &q == question => Some((text, via)),
        _ => None,
    });
    entries.collect()
}

async fn system_notes(h: &Harness) -> Vec<String> {
    let notes = feed(h).await.into_iter().filter_map(|entry| match entry {
        FeedEntry::System { text } => Some(text),
        _ => None,
    });
    notes.collect()
}

async fn question(h: &Harness, id: &QuestionId) -> Question {
    h.ctx.store.lock().await.question(id).expect("question")
}

/// `request` as `archon mcp` forwards it for `caller`, the agent Aether names.
fn for_agent(caller: &str, request: Request) -> Request {
    Request::ForAgent {
        caller: AetherAgentId::parse(caller).unwrap(),
        request: Box::new(request),
    }
}

/// The orchestrator's `ask_operator`, as Aether's caller metadata vouches.
async fn ask(h: &Harness, text: &str, options: &[&str]) -> Question {
    ask_as(h, ORCH, text, options).await
}

/// An `ask_operator` over MCP by `caller`, which must be the orchestrator
/// when it asks: no other agent may.
async fn ask_as(h: &Harness, caller: &str, text: &str, options: &[&str]) -> Question {
    let request = Request::AskOperator {
        text: text.into(),
        item: None,
        options: options.iter().map(|o| (*o).to_owned()).collect(),
        caller: Some(AetherAgentId::parse(caller).unwrap()),
    };
    let request = for_agent(caller, request);
    let reply = handlers::handle(&h.ctx, ClientKind::ArchonMcp, request).await;
    let Ok(Reply::Question(asked)) = reply else {
        panic!("ask replied {reply:?}")
    };
    let asker = asked.asked_by.as_ref().map(AetherAgentId::as_str);
    assert_eq!(asker, Some(caller));
    assert_eq!(question(h, &asked.id).await, asked, "stored as replied");
    asked
}

/// An open question with no recorded asker, as a store written before
/// questions recorded one still holds it.
async fn unvouched(h: &Harness) -> Question {
    let mut legacy = serde_json::to_value(ask(h, "Which branch?", &[]).await).unwrap();
    let fields = legacy.as_object_mut().expect("a question is an object");
    assert!(
        fields.remove("asked_by").is_some(),
        "the asker was recorded"
    );
    legacy["id"] = json!(QuestionId::new());
    legacy["number"] = json!(0);
    let legacy: Question = serde_json::from_value(legacy).unwrap();
    let legacy = h.ctx.store.lock().await.insert_question(&legacy).unwrap();
    assert_eq!(legacy.asked_by, None);
    legacy
}

async fn set_orchestrator(h: &Harness, agent: &str) {
    let mut store = h.ctx.store.lock().await;
    store.kv_set(keys::ORCHESTRATOR_AGENT, agent).expect("kv");
}

async fn enqueue(h: &Harness, title: &str, autonomy: AutonomyPatch) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy,
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "propose" => completed(PROPOSAL),
        "report" => completed("Changed the greeting; risk: none."),
        "review" => verdict(true),
        _ => completed("implemented"),
    }
}

/// Settle the running stages, writing each gate stage's page first, then
/// spawn what follows.
async fn settle(h: &Harness, outcome: impl Fn(&StageRun) -> StageOutcome) {
    for stage in h.running().await {
        let page = match stage.stage_id.as_str() {
            "propose" => ".archon/reports/proposal/index.html",
            "report" => ".archon/reports/change/index.html",
            _ => continue,
        };
        let item = h.ctx.store.lock().await.run(&stage.run).expect("run").item;
        let path = h.run(&item).await.workspace.join(page);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "<html><body>page</body></html>").unwrap();
    }
    h.settle_running(outcome).await;
    h.dispatch().await;
}

/// The item's one open gate and its open question.
async fn open_gate(h: &Harness, item: &QueueItemId) -> (OperatorGate, Question) {
    let gates = h
        .ctx
        .store
        .lock()
        .await
        .gates_for_item(item)
        .expect("gates");
    let mut open: Vec<OperatorGate> = gates.into_iter().filter(OperatorGate::is_open).collect();
    assert_eq!(open.len(), 1, "one open gate: {open:?}");
    let gate = open.remove(0);
    let asked = question(h, gate.question.as_ref().expect("the gate was asked")).await;
    assert!(asked.is_open(), "an open gate's question is open");
    (gate, asked)
}

async fn decision(h: &Harness, item: &QueueItemId, stage: &str) -> Option<GateDecision> {
    let gates = h
        .ctx
        .store
        .lock()
        .await
        .gates_for_item(item)
        .expect("gates");
    let gate = gates.into_iter().rfind(|gate| gate.stage_id == stage);
    gate.expect("the stage has a gate").decision
}

/// A harness whose item waits at its proposal gate.
async fn at_proposal_gate(mode: AetherMode) -> (Harness, QueueItem, Question) {
    let h = harness(mode).await;
    let knobs = AutonomyPatch {
        proposal: Some(ProposalMode::Lavish),
        ..AutonomyPatch::default()
    };
    let item = enqueue(&h, "Add a greeting flag", knobs).await;
    h.dispatch().await;
    settle(&h, passing).await;
    let (gate, asked) = open_gate(&h, &item.id).await;
    assert_eq!(gate.subject, GateSubject::Proposal);
    (h, item, asked)
}

/// A harness whose item waits at its report gate.
async fn at_report_gate() -> (Harness, QueueItem, Question) {
    let h = harness(AetherMode::Dedicated).await;
    let knobs = AutonomyPatch {
        report: Some(ReportMode::BeforePr),
        ..AutonomyPatch::default()
    };
    let item = enqueue(&h, "Report first", knobs).await;
    h.dispatch().await;
    for _ in 0..3 {
        settle(&h, passing).await;
    }
    let (gate, asked) = open_gate(&h, &item.id).await;
    assert_eq!(gate.subject, GateSubject::Report);
    (h, item, asked)
}

/// Nothing was answered and the daemon said nothing about a command.
async fn assert_untouched(h: &Harness, asked: &Question, case: &str) {
    assert!(question(h, &asked.id).await.is_open(), "{case}: still open");
    assert!(answers(h, &asked.id).await.is_empty(), "{case}: no answer");
    assert!(chat_news(h).await.is_empty(), "{case}: no chat_answer news");
    assert!(system_notes(h).await.is_empty(), "{case}: no feed note");
}

#[tokio::test]
async fn an_operator_command_answers_an_ask_operator_question() {
    let h = harness(AetherMode::Dedicated).await;
    let asked = ask(&h, "Ship to prod?", &["yes", "wait a day"]).await;
    let command = format!("  /answer Q{} 2\n", asked.number);
    deliver(&h, 1, operator(&command)).await;

    let closed = question(&h, &asked.id).await;
    assert_eq!(closed.answer.as_deref(), Some("wait a day"), "the option");
    let recorded = vec![("wait a day".to_owned(), AnswerVia::ChatCommand)];
    assert_eq!(answers(&h, &asked.id).await, recorded);
    let routed = TriggerEvent::OperatorAnswer {
        question: asked.id.clone(),
        number: Some(asked.number),
        question_text: asked.text.clone(),
        options: asked.options.clone(),
        item: asked.item.clone(),
        asked_at: Some(asked.asked_at),
        text: "wait a day".into(),
    };
    assert!(held(&h).await.contains(&routed), "the orchestrator gets it");
    let applied = vec![(Some(asked.number), true, "wait a day".to_owned())];
    assert_eq!(chat_news(&h).await, applied);
    assert!(system_notes(&h).await.is_empty(), "nothing was refused");
    let Reply::Questions(open) = h
        .cli(Request::ListQuestions { open_only: true })
        .await
        .unwrap()
    else {
        panic!("questions")
    };
    assert!(open.is_empty(), "it left `archon questions`");
}

#[tokio::test]
async fn an_operator_command_approves_a_proposal_gate_by_id() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    deliver(
        &h,
        1,
        operator(&format!("/answer {} approve keep it small", asked.id)),
    )
    .await;
    h.dispatch().await;

    let note = Some("keep it small".to_owned());
    let approved = Some(GateDecision::Approve { note });
    assert_eq!(decision(&h, &item.id, "propose").await, approved);
    let running = h.running().await;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].stage_id, "implement", "the run moved on");
    let recorded = vec![("approve keep it small".to_owned(), AnswerVia::ChatCommand)];
    assert_eq!(answers(&h, &asked.id).await, recorded);
    let applied = (Some(asked.number), true, "approve keep it small".to_owned());
    assert_eq!(chat_news(&h).await, vec![applied]);
}

#[tokio::test]
async fn operator_commands_revise_then_reject_a_report_gate() {
    let (h, item, first) = at_report_gate().await;
    let revise = format!("/answer {} cover the error path too", first.number);
    deliver(&h, 1, operator(&revise)).await;
    h.dispatch().await;
    let feedback = "cover the error path too".to_owned();
    let revised = Some(GateDecision::Revise { feedback });
    assert_eq!(decision(&h, &item.id, "report").await, revised);
    let running = h.running().await;
    let redone = (running[0].stage_id.as_str(), running[0].attempt);
    assert_eq!(redone, ("implement", 2), "the work went back");

    for _ in 0..3 {
        settle(&h, passing).await;
    }
    let (_, second) = open_gate(&h, &item.id).await;
    assert!(second.number > first.number, "a new gate, a new number");
    deliver(
        &h,
        2,
        operator(&format!("/answer {} reject", second.number)),
    )
    .await;
    h.dispatch().await;
    assert!(matches!(
        decision(&h, &item.id, "report").await,
        Some(GateDecision::Reject { .. })
    ));
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    let applied: Vec<bool> = chat_news(&h).await.into_iter().map(|news| news.1).collect();
    assert_eq!(applied, [true, true]);
}

#[tokio::test]
async fn archon_answer_applies_like_answer_for_the_operator() {
    let h = harness(AetherMode::Dedicated).await;
    let asked = ask(&h, "Ship to prod?", &["yes", "wait a day"]).await;
    let command = format!("/archon answer {} 2", asked.number);
    deliver(&h, 1, operator(&command)).await;
    assert_eq!(
        question(&h, &asked.id).await.answer.as_deref(),
        Some("wait a day")
    );
    let applied = vec![(Some(asked.number), true, "wait a day".to_owned())];
    assert_eq!(chat_news(&h).await, applied);
}

#[tokio::test]
async fn archon_approve_and_reject_decide_gates_like_the_bare_commands() {
    let (h, item, _) = at_proposal_gate(AetherMode::Dedicated).await;
    deliver(&h, 1, operator("/archon approve")).await;
    assert!(matches!(
        decision(&h, &item.id, "propose").await,
        Some(GateDecision::Approve { .. })
    ));
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    deliver(
        &h,
        1,
        operator(&format!("/ARCHON reject {} too big", asked.number)),
    )
    .await;
    assert!(matches!(
        decision(&h, &item.id, "propose").await,
        Some(GateDecision::Reject { .. })
    ));
}

#[tokio::test]
async fn archon_answer_from_anyone_but_the_operator_passes_no_gate() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let text = format!("/archon answer {} approve", asked.number);
    let sender = json!({"actor": "agent", "id": "agt_stage"});
    deliver(&h, 1, message(sender, &text)).await;
    h.dispatch().await;
    assert_untouched(&h, &asked, "an agent's /archon answer").await;
    assert_eq!(decision(&h, &item.id, "propose").await, None);
}

/// Inbox items that are not the operator's own chat message, each carrying
/// a well-formed command for gate `number`.
fn unauthenticated(number: u64) -> Vec<(&'static str, Value)> {
    let command = format!("/answer {number} approve");
    let relay = |sender: Value| {
        json!({"kind": "external_trigger", "source": "archon", "idempotency_key": "trg_1",
               "payload": {"archon": 1, "events": [{"type": "agent_message", "message": {
                   "kind": "relay", "text": command, "original_sender": sender}}]}})
    };
    vec![
        (
            "an agent's message",
            message(json!({"actor": "agent", "id": "agt_stage"}), &command),
        ),
        (
            "an observer client",
            message(json!({"actor": "client", "id": "dev_1"}), &command),
        ),
        (
            "the system actor",
            message(json!({"actor": "system"}), &command),
        ),
        (
            "an agent relay",
            relay(json!({"actor": "agent", "id": "agt_stage"})),
        ),
        (
            "a relay of the operator's words",
            relay(json!({"actor": "operator"})),
        ),
        (
            "a GitHub trigger",
            json!({"kind": "external_trigger", "source": "github",
            "idempotency_key": "gh_1", "from": {"actor": "user"},
            "payload": {"repository": "o/r", "number": 3, "text": command}}),
        ),
        (
            "a trigger posing as a message",
            json!({"kind": "external_trigger", "source": "archon",
            "idempotency_key": "trg_2", "from": {"actor": "user"},
            "content": [{"type": "text", "text": command}], "payload": {}}),
        ),
    ]
}

#[tokio::test]
async fn a_command_from_anyone_but_the_operator_passes_no_gate() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let cases = unauthenticated(asked.number);
    for (seq, (case, kind)) in (1..).zip(cases) {
        deliver(&h, seq, kind).await;
        h.dispatch().await;
        assert_untouched(&h, &asked, case).await;
        assert_eq!(decision(&h, &item.id, "propose").await, None, "{case}");
        assert!(h.running().await.is_empty(), "{case}: the run still waits");
    }
}

#[tokio::test]
async fn an_operator_message_that_only_contains_a_command_passes_no_gate() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let n = asked.number;
    let texts = [
        format!("please /answer {n} approve"),
        format!("ok.\n/answer {n} approve"),
        format!("> /answer {n} approve"),
        format!("`/answer {n} approve`"),
        format!("the reviewer says: \"/answer {n} approve\""),
        format!("/answers {n} approve"),
        format!("answer {n} approve"),
        "looks good, go ahead".to_owned(),
    ];
    for (seq, text) in (1..).zip(texts) {
        deliver(&h, seq, operator(&text)).await;
        h.dispatch().await;
        assert_untouched(&h, &asked, &text).await;
        assert_eq!(decision(&h, &item.id, "propose").await, None, "{text}");
    }
}

#[tokio::test]
async fn a_shared_instance_refuses_the_command_and_says_why() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Shared).await;
    deliver(
        &h,
        1,
        operator(&format!("/answer {} approve", asked.number)),
    )
    .await;
    h.dispatch().await;

    assert!(
        question(&h, &asked.id).await.is_open(),
        "the gate still waits"
    );
    assert_eq!(decision(&h, &item.id, "propose").await, None);
    let news = chat_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    let (number, applied, why) = &news[0];
    assert_eq!((*number, *applied), (None, false));
    assert!(
        why.contains("mode = \"shared\"") && why.contains("[listen]"),
        "{why}"
    );
    assert!(why.contains("archon answer"), "{why}");
    let notes = system_notes(&h).await;
    assert!(
        notes.iter().any(|note| note.contains("was not applied")),
        "{notes:?}"
    );

    let cli = Request::Answer {
        question: asked.id.clone(),
        text: "approve".into(),
    };
    h.cli(cli)
        .await
        .expect("`archon answer` still passes the gate");
    assert!(decision(&h, &item.id, "propose").await.is_some());
}

#[tokio::test]
async fn a_malformed_or_unknown_command_is_refused_with_its_reason() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let unknown = QuestionId::new();
    let cases = [
        ("/answer".to_owned(), "no question was named"),
        (format!("/answer {}", asked.number), "no answer was given"),
        ("/answer approve".to_owned(), "neither a question number"),
        ("/answer 0 approve".to_owned(), "not a question number"),
        (
            "/answer 999 approve".to_owned(),
            "no question Q999 is recorded",
        ),
        (
            format!("/answer {unknown} approve"),
            "is recorded; `archon questions`",
        ),
    ];
    for (seq, (text, reason)) in (1..).zip(&cases) {
        deliver(&h, seq, operator(text)).await;
        let news = chat_news(&h).await;
        let (number, applied, why) = news.last().expect("the refusal is reported");
        assert_eq!((*number, *applied), (None, false), "{text}");
        assert!(why.contains(reason), "{text}: {why}");
        let notes = system_notes(&h).await;
        assert!(
            notes.last().is_some_and(|note| note.contains(reason)),
            "{notes:?}"
        );
    }
    assert_eq!(chat_news(&h).await.len(), cases.len(), "one report each");
    assert!(question(&h, &asked.id).await.is_open());
    assert_eq!(decision(&h, &item.id, "propose").await, None);
}

#[tokio::test]
async fn an_answered_or_withdrawn_gate_is_not_answered_again() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let n = asked.number;
    deliver(&h, 1, operator(&format!("/answer {n} some feedback"))).await;
    deliver(&h, 2, operator(&format!("/answer {n} reject"))).await;
    let feedback = "some feedback".to_owned();
    let revised = Some(GateDecision::Revise { feedback });
    assert_eq!(
        decision(&h, &item.id, "propose").await,
        revised,
        "the first stands"
    );
    let news = chat_news(&h).await;
    assert_eq!((news[0].0, news[0].1), (Some(n), true));
    assert_eq!((news[1].0, news[1].1), (Some(n), false));
    assert!(news[1].2.contains("was already answered"), "{}", news[1].2);
    assert_eq!(answers(&h, &asked.id).await.len(), 1, "recorded once");

    h.dispatch().await;
    settle(&h, passing).await;
    let (_, again) = open_gate(&h, &item.id).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    deliver(
        &h,
        3,
        operator(&format!("/answer {} approve", again.number)),
    )
    .await;
    let news = chat_news(&h).await;
    assert_eq!(
        (news[2].0, news[2].1),
        (Some(again.number), false),
        "{news:?}"
    );
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    assert!(
        h.running().await.is_empty(),
        "a withdrawn gate starts nothing"
    );
}

#[tokio::test]
async fn a_replayed_event_applies_its_command_once() {
    let h = harness(AetherMode::Dedicated).await;
    let asked = ask(&h, "Ship to prod?", &[]).await;
    let command = operator(&format!("/answer {} yes", asked.number));
    deliver(&h, 4, command.clone()).await;
    deliver(&h, 4, command.clone()).await;
    deliver(&h, 3, command).await;
    assert_eq!(answers(&h, &asked.id).await.len(), 1);
    let news = chat_news(&h).await;
    assert_eq!(news, vec![(Some(asked.number), true, "yes".to_owned())]);
}

#[tokio::test]
async fn an_answer_that_cannot_be_delivered_is_reported_and_stays_open() {
    let h = harness(AetherMode::Dedicated).await;
    let mut blocking = ask(&h, "placeholder", &[]).await;
    (blocking.id, blocking.number) = (QuestionId::new(), 0);
    blocking.aether = Some(AetherQuestionRef {
        agent: AetherAgentId::parse("agt_stage").unwrap(),
        seq: 9,
    });
    let blocking = h.ctx.store.lock().await.insert_question(&blocking).unwrap();
    deliver(
        &h,
        1,
        operator(&format!("/answer {} use the cache", blocking.number)),
    )
    .await;

    let news = chat_news(&h).await;
    assert_eq!(news.len(), 1, "{news:?}");
    let (number, applied, why) = &news[0];
    assert_eq!((*number, *applied), (Some(blocking.number), false));
    assert!(
        why.ends_with(&format!("Q{} is still open", blocking.number)),
        "{why}"
    );
    assert!(
        question(&h, &blocking.id).await.is_open(),
        "a retry can answer it"
    );
    assert!(answers(&h, &blocking.id).await.is_empty());
}

#[tokio::test]
async fn an_opened_gate_is_news_with_its_number_and_how_to_answer_in_words() {
    let h = harness(AetherMode::Dedicated).await;
    let knobs = AutonomyPatch {
        proposal: Some(ProposalMode::Lavish),
        ..AutonomyPatch::default()
    };
    let item = enqueue(&h, &"A long title ".repeat(15), knobs).await;
    h.dispatch().await;
    settle(&h, |_| {
        completed(&"Approach: a long proposal. ".repeat(400))
    })
    .await;
    let (gate, asked) = open_gate(&h, &item.id).await;
    let opened: Vec<TriggerEvent> = held(&h).await;
    let [
        TriggerEvent::GateOpened {
            question,
            number,
            item: about,
            title,
            subject,
            asks,
            summary,
            page,
            reply,
        },
    ] = &opened[..]
    else {
        panic!("one gate_opened: {opened:?}")
    };
    assert_eq!(
        (question, *number, about),
        (&asked.id, asked.number, &item.id)
    );
    assert_eq!(*subject, GateSubject::Proposal);
    assert!(asks.starts_with("Approve the direction"), "{asks}");
    assert!(summary.starts_with("Approach: a long proposal.") && summary.ends_with('…'));
    assert_eq!(page, gate.location.as_deref().unwrap());
    for answer in ["approve", "reject", "<feedback>"] {
        let command = format!("`/answer {number} {answer}`");
        assert!(reply.contains(&command), "{reply}");
    }
    assert!(title.chars().count() <= 200 && summary.chars().count() <= 600);
    let encoded = serde_json::to_string(&opened[0]).unwrap();
    assert!(encoded.len() < 4_096, "bounded: {} bytes", encoded.len());
    assert_eq!(
        serde_json::to_value(&opened[0]).unwrap()["type"],
        "gate_opened"
    );
}

#[tokio::test]
async fn a_shared_instance_gate_names_the_cli_as_the_way_to_answer() {
    let (h, _, asked) = at_proposal_gate(AetherMode::Shared).await;
    let opened = held(&h).await;
    let [TriggerEvent::GateOpened { reply, .. }] = &opened[..] else {
        panic!("one gate_opened: {opened:?}")
    };
    assert!(
        reply.contains(&format!("`archon answer {} approve`", asked.number)),
        "{reply}"
    );
    assert!(!reply.contains("/answer"), "{reply}");
}

async fn record(
    h: &Harness,
    client: ClientKind,
    question: &QuestionId,
    caller: Option<&str>,
) -> Result<Reply, ArchonError> {
    let request = Request::RecordChatAnswer {
        question: question.clone(),
        answer: "Yes, ship it on Monday.".into(),
        caller: caller.map(|id| AetherAgentId::parse(id).unwrap()),
    };
    // Only `archon mcp` forwards a call for the agent Aether names.
    let request = match caller.filter(|_| client == ClientKind::ArchonMcp) {
        Some(caller) => for_agent(caller, request),
        None => request,
    };
    handlers::handle(&h.ctx, client, request).await
}

#[tokio::test]
async fn the_orchestrator_records_a_chat_reply_to_its_own_question() {
    let h = harness(AetherMode::Dedicated).await;
    let asked = ask(&h, "Ship to prod?", &[]).await;
    let reply = record(&h, ClientKind::ArchonMcp, &asked.id, Some(ORCH)).await;
    let Ok(Reply::Question(closed)) = reply else {
        panic!("recorded: {reply:?}")
    };
    assert_eq!(closed.answer.as_deref(), Some("Yes, ship it on Monday."));
    assert!(
        !question(&h, &asked.id).await.is_open(),
        "it left the open list"
    );
    let recorded = vec![("Yes, ship it on Monday.".to_owned(), AnswerVia::ChatReply)];
    assert_eq!(answers(&h, &asked.id).await, recorded);
    assert!(
        held(&h).await.is_empty(),
        "the orchestrator already has the answer"
    );

    let again = record(&h, ClientKind::ArchonMcp, &asked.id, Some(ORCH)).await;
    assert!(
        matches!(again, Err(ArchonError::InvalidTransition(_))),
        "{again:?}"
    );
    assert_eq!(answers(&h, &asked.id).await.len(), 1);
}

#[tokio::test]
async fn only_the_orchestrator_records_a_chat_reply() {
    let h = harness(AetherMode::Dedicated).await;
    let asked = ask(&h, "Ship to prod?", &[]).await;
    for caller in [None, Some("agt_stage")] {
        for client in [ClientKind::ArchonMcp, ClientKind::ArchonCli] {
            let refused = record(&h, client, &asked.id, caller).await;
            assert!(
                matches!(refused, Err(ArchonError::Forbidden(_))),
                "{caller:?}: {refused:?}"
            );
        }
    }
    h.ctx
        .store
        .lock()
        .await
        .kv_delete(keys::ORCHESTRATOR_AGENT)
        .unwrap();
    let nobody = record(&h, ClientKind::ArchonMcp, &asked.id, Some(ORCH)).await;
    assert!(
        matches!(nobody, Err(ArchonError::Forbidden(_))),
        "{nobody:?}"
    );
    assert!(question(&h, &asked.id).await.is_open());
    assert!(answers(&h, &asked.id).await.is_empty());
}

/// The orchestrator's attempt to close `asked`, which is not its own
/// `ask_operator` question, is refused and changes nothing.
async fn assert_only_the_operator_answers(h: &Harness, asked: &Question, asker: &str) {
    let refused = record(h, ClientKind::ArchonMcp, &asked.id, Some(ORCH)).await;
    let Err(ArchonError::Forbidden(why)) = refused else {
        panic!("{asker}: {refused:?}")
    };
    assert!(
        why.contains(asker) && why.contains("only the operator answers"),
        "{why}"
    );
    assert!(
        question(h, &asked.id).await.is_open(),
        "{asker}: still open"
    );
    assert!(answers(h, &asked.id).await.is_empty(), "{asker}: no answer");
    let operator_only = Request::Answer {
        question: asked.id.clone(),
        text: "approve".into(),
    };
    let forbidden = handlers::handle(&h.ctx, ClientKind::ArchonMcp, operator_only).await;
    assert!(
        matches!(forbidden, Err(ArchonError::Forbidden(_))),
        "{asker}: {forbidden:?}"
    );
}

#[tokio::test]
async fn the_orchestrator_cannot_close_a_question_another_agent_asked() {
    let h = harness(AetherMode::Dedicated).await;
    // The only other agent that can have asked: an orchestrator since replaced.
    set_orchestrator(&h, "agt_earlier").await;
    let theirs = ask_as(&h, "agt_earlier", "Which branch?", &[]).await;
    set_orchestrator(&h, ORCH).await;
    assert_only_the_operator_answers(&h, &theirs, "was asked by another agent (agt_earlier)").await;
    let asker = record(&h, ClientKind::ArchonMcp, &theirs.id, Some("agt_earlier")).await;
    assert!(
        matches!(asker, Err(ArchonError::Forbidden(_))),
        "not the orchestrator: {asker:?}"
    );
    assert!(question(&h, &theirs.id).await.is_open());
}

#[tokio::test]
async fn a_question_with_no_recorded_asker_is_never_the_orchestrators() {
    let h = harness(AetherMode::Dedicated).await;
    let legacy = unvouched(&h).await;
    assert_only_the_operator_answers(&h, &legacy, "has no recorded asker").await;
}

/// Asking is the orchestrator's alone: a call from another agent, or one
/// that names none while the orchestrator can delegate, asks nothing.
#[tokio::test]
async fn ask_operator_is_refused_from_an_unnamed_caller_and_from_any_other_agent() {
    let h = harness(AetherMode::Dedicated).await;
    let ask = |caller: Option<&str>| Request::AskOperator {
        text: "Which branch?".into(),
        item: None,
        options: vec![],
        caller: caller.map(|id| AetherAgentId::parse(id).unwrap()),
    };
    let unnamed = handlers::handle(&h.ctx, ClientKind::ArchonMcp, ask(None)).await;
    let Err(ArchonError::Forbidden(why)) = unnamed else {
        panic!("an unnamed caller asked: {unnamed:?}")
    };
    assert!(
        why.contains("Aether did not say which agent makes it"),
        "{why}"
    );
    assert!(why.contains("`archon daemon stop --aether`"), "{why}");
    let other = for_agent("agt_stage", ask(Some("agt_stage")));
    let other = handlers::handle(&h.ctx, ClientKind::ArchonMcp, other).await;
    assert!(matches!(other, Err(ArchonError::Forbidden(_))), "{other:?}");
    let open = h.ctx.store.lock().await.questions(true).unwrap();
    assert!(open.is_empty(), "nothing was asked: {open:?}");
}

/// The reviewer's case: a question asked before this orchestrator existed.
#[tokio::test]
async fn a_new_orchestrator_cannot_close_what_was_asked_before_it() {
    const NEXT: &str = "agt_new_orchestrator";
    let h = harness(AetherMode::Dedicated).await;
    let unvouched = unvouched(&h).await;
    let predecessors = ask(&h, "Ship to prod?", &[]).await;
    set_orchestrator(&h, NEXT).await;

    for (asked, asker) in [
        (&unvouched, "has no recorded asker"),
        (&predecessors, "was asked by another agent (agt_orch)"),
    ] {
        let refused = record(&h, ClientKind::ArchonMcp, &asked.id, Some(NEXT)).await;
        let Err(ArchonError::Forbidden(why)) = refused else {
            panic!("{asker}: {refused:?}")
        };
        assert!(why.contains(asker), "{why}");
        assert!(question(&h, &asked.id).await.is_open(), "{asker}");
    }
    let retired = record(&h, ClientKind::ArchonMcp, &predecessors.id, Some(ORCH)).await;
    assert!(
        matches!(retired, Err(ArchonError::Forbidden(_))),
        "a retired orchestrator records nothing: {retired:?}"
    );
    assert!(answers(&h, &predecessors.id).await.is_empty());

    let own = ask_as(&h, NEXT, "And the changelog?", &[]).await;
    let recorded = record(&h, ClientKind::ArchonMcp, &own.id, Some(NEXT)).await;
    assert!(matches!(recorded, Ok(Reply::Question(_))), "{recorded:?}");
    assert!(!question(&h, &own.id).await.is_open());
}

#[tokio::test]
async fn the_orchestrator_cannot_answer_a_proposal_or_report_gate() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    assert_only_the_operator_answers(&h, &asked, "an operator gate").await;
    assert_eq!(decision(&h, &item.id, "propose").await, None);
    assert!(h.running().await.is_empty(), "the run still waits");

    let (h, item, asked) = at_report_gate().await;
    assert_only_the_operator_answers(&h, &asked, "an operator gate").await;
    assert_eq!(decision(&h, &item.id, "report").await, None);
    assert!(h.running().await.is_empty(), "the run still waits");
}

#[tokio::test]
async fn the_orchestrator_cannot_answer_a_reopen_or_an_aether_question() {
    let h = harness(AetherMode::Dedicated).await;
    let mut other = ask(&h, "placeholder", &[]).await;
    (other.id, other.number) = (QuestionId::new(), 0);
    other.text = "The review window for \"E2E\" was closed. Reopen it? \
                  (The report is at /tmp/e2e.html.)"
        .into();
    other.options = vec!["reopen".into(), "leave it closed".into()];
    let reopen = h.ctx.store.lock().await.insert_question(&other).unwrap();
    assert_only_the_operator_answers(&h, &reopen, "\"Reopen it?\" question").await;

    (other.id, other.number) = (QuestionId::new(), 0);
    other.aether = Some(AetherQuestionRef {
        agent: AetherAgentId::parse("agt_stage").unwrap(),
        seq: 4,
    });
    let blocking = h.ctx.store.lock().await.insert_question(&other).unwrap();
    assert_only_the_operator_answers(&h, &blocking, "`ask_user` question").await;
}

#[tokio::test]
async fn the_retired_a_alias_answers_nothing_and_points_to_answer() {
    for mode in [AetherMode::Dedicated, AetherMode::Shared] {
        let (h, item, asked) = at_proposal_gate(mode).await;
        deliver(&h, 1, operator(&format!("/a {} yes", asked.number))).await;
        assert!(question(&h, &asked.id).await.is_open(), "{mode:?}");
        assert_eq!(decision(&h, &item.id, "propose").await, None);
        let notes = system_notes(&h).await;
        assert!(notes.iter().any(|n| n.contains("use /answer")), "{notes:?}");
        let news = chat_news(&h).await;
        assert_eq!(news.len(), 1, "{mode:?}: {news:?}");
        let (number, applied, hint) = &news[0];
        assert_eq!((*number, *applied), (None, false));
        assert!(hint.contains("use /answer"), "{hint}");
    }
}

#[tokio::test]
async fn approve_alias_passes_the_only_open_gate() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    deliver(&h, 1, operator("/approve")).await;
    assert!(!question(&h, &asked.id).await.is_open());
    assert!(matches!(
        decision(&h, &item.id, "propose").await,
        Some(GateDecision::Approve { .. })
    ));
}

#[tokio::test]
async fn github_operator_can_answer_in_shared_mode_but_payload_markers_cannot() {
    use archon_aether::{AetherClient, fake::FakeAether};
    use std::sync::Arc;
    let (h, _, asked) = at_proposal_gate(AetherMode::Shared).await;
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeAether::start(&dir.path().join("authority.sock"))
        .await
        .unwrap();
    let status = json!({"revision": 1, "repositories": [], "config": {
        "enabled": true, "mention_regex": "m", "label_regex": "l", "poll_interval_secs": 120,
        "repositories": [{"repository": "o/r", "enabled": true, "agent": ORCH,
            "dispatch": {"mode": "fixed"}, "allowed_users": null}]}});
    fake.set_default(
        "get_github_watcher",
        archon_aether::fake::Scripted::reply("github_watcher", status),
    );
    common::git(
        &h.repo,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
    h.ctx.aether.replace(Some(Arc::new(client))).unwrap();
    let command = format!("@archon /approve {}", asked.number);
    let mut trigger = json!({"kind":"external_trigger", "source":"github", "operator":false,
        "payload":{"repository":"o/r", "number":12,"requester":"octocat","operator":true,"text":command}});
    deliver(&h, 1, trigger.clone()).await;
    assert!(question(&h, &asked.id).await.is_open());
    trigger["operator"] = json!(true);
    deliver(&h, 2, trigger).await;
    assert!(!question(&h, &asked.id).await.is_open());
    assert!(feed(&h).await.into_iter().any(
        |entry| matches!(entry, FeedEntry::Authority { evidence, .. }
        if evidence.channel.is_github() && evidence.seq == 2)
    ));
}

#[tokio::test]
async fn a_cited_gate_answer_needs_its_answer_word_in_the_quote() {
    let (h, _item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let mut event = delivered(1, operator("what is the status of the queue?"));
    event.at = archon_core::now();
    pump::handle_orchestrator_event(&h.ctx, &event)
        .await
        .unwrap();
    let request = Request::RequestAuthority {
        action: Box::new(Request::Answer {
            question: asked.id.clone(),
            text: "approve".into(),
        }),
        utterance: 1,
        quote: "status of the queue".into(),
        reason: "operator approved this gate".into(),
        caller: Some(AetherAgentId::parse(ORCH).unwrap()),
    };
    assert!(
        h.orchestrator(request)
            .await
            .unwrap_err()
            .to_string()
            .contains("answer word")
    );
    assert!(question(&h, &asked.id).await.is_open());
}

#[tokio::test]
async fn a_cited_plain_language_gate_answer_must_postdate_the_gate() {
    let (h, item, asked) = at_proposal_gate(AetherMode::Dedicated).await;
    let request = |seq| Request::RequestAuthority {
        action: Box::new(Request::Answer {
            question: asked.id.clone(),
            text: "approve".into(),
        }),
        utterance: seq,
        quote: "please approve".into(),
        reason: "operator approved this gate".into(),
        caller: Some(AetherAgentId::parse(ORCH).unwrap()),
    };
    let mut earlier = delivered(1, operator("please approve"));
    earlier.at = asked.asked_at - chrono::Duration::seconds(1);
    pump::handle_orchestrator_event(&h.ctx, &earlier)
        .await
        .unwrap();
    assert!(
        h.orchestrator(request(1))
            .await
            .unwrap_err()
            .to_string()
            .contains("predates")
    );
    let mut event = delivered(2, operator("please approve"));
    event.at = archon_core::now();
    pump::handle_orchestrator_event(&h.ctx, &event)
        .await
        .unwrap();
    let result = h.orchestrator(request(2)).await.unwrap();
    assert!(matches!(result, Reply::Question(q) if !q.is_open()));
    assert!(matches!(
        decision(&h, &item.id, "propose").await,
        Some(GateDecision::Approve { .. })
    ));
}

#[tokio::test]
async fn github_cannot_answer_an_itemless_question_from_another_thread() {
    use archon_aether::{AetherClient, fake::FakeAether};
    let h = harness(AetherMode::Dedicated).await;
    let asked = ask(&h, "Which branch?", &[]).await;
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeAether::start(&dir.path().join("authority.sock"))
        .await
        .unwrap();
    let client = AetherClient::connect(&fake.endpoint()).await.unwrap();
    h.ctx
        .aether
        .replace(Some(std::sync::Arc::new(client)))
        .unwrap();
    let command = format!("@archon /answer {} main", asked.number);
    let trigger = json!({"kind":"external_trigger", "source":"github", "operator":true,
        "payload":{"repository":"other-org/unrelated", "number":3,"requester":"octocat","operator":true,"text":command}});
    deliver(&h, 1, trigger).await;
    assert!(question(&h, &asked.id).await.is_open());
}
