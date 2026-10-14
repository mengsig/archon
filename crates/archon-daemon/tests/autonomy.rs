//! Autonomy (ARCHITECTURE §18) against a scripted backend: operator gates
//! answered in chat or on the Lavish page, across a restart and a cancel;
//! the ceiling refusing an enqueue; and the tool exposure `tools` gives.

mod common;

use std::path::Path;
use std::time::Duration;

use archon_core::autonomy::{AutonomyPatch, MergeMode, ProposalMode, ReportMode, ToolsMode};
use archon_core::config::{Config, LavishConfig, Toggle};
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, QueueItemId};
use archon_core::notify::{FeedEntry, Question};
use archon_core::protocol::{EnqueueRequest, Reply, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{GateDecision, GateSubject, OperatorGate, RunState, StageOutcome, StageRun};
use archon_daemon::executor;
use archon_daemon::reports;
use archon_daemon::triggers::TriggerEvent;
use archon_lavish::Lavish;
use common::{Harness, PIPELINE, REPO_NAME, completed, plan, verdict};

const PROPOSAL: &str = "Approach: add a greeting flag.\nFiles: src/lib.rs.";

fn proposal() -> AutonomyPatch {
    AutonomyPatch {
        proposal: Some(ProposalMode::Lavish),
        ..AutonomyPatch::default()
    }
}

async fn enqueue_knobs(
    h: &Harness,
    title: &str,
    autonomy: AutonomyPatch,
) -> Result<QueueItem, ArchonError> {
    enqueue_into(h, PIPELINE, title, autonomy).await
}

async fn enqueue_into(
    h: &Harness,
    pipeline: &str,
    title: &str,
    autonomy: AutonomyPatch,
) -> Result<QueueItem, ArchonError> {
    let request = EnqueueRequest {
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy,
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

/// What a well-behaved agent of each stage reports; a gate stage also
/// writes the page its prompt asks for.
fn passing(stage: &StageRun) -> StageOutcome {
    match stage.stage_id.as_str() {
        "propose" => completed(PROPOSAL),
        "report" => completed("Changed the greeting; risk: none."),
        "review" => verdict(true),
        "ship" => StageOutcome::Completed {
            summary: "opened a pull request".into(),
            structured: Some(serde_json::json!({"pr_url": "https://example.invalid/pr/1"})),
        },
        _ => completed("implemented"),
    }
}

/// Settle the running stages, writing each gate stage's page first, then
/// spawn what follows.
async fn settle(h: &Harness) -> Vec<StageRun> {
    for stage in h.running().await {
        let page = match stage.stage_id.as_str() {
            "propose" => ".archon/reports/proposal/index.html",
            "report" => ".archon/reports/change/index.html",
            _ => continue,
        };
        let run = h.run(&stage_item(h, &stage).await).await;
        let path = run.workspace.join(page);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "<html><body>page</body></html>").unwrap();
    }
    let settled = h.settle_running(passing).await;
    h.dispatch().await;
    settled
}

async fn stage_item(h: &Harness, stage: &StageRun) -> QueueItemId {
    h.ctx.store.lock().await.run(&stage.run).expect("run").item
}

async fn gates(h: &Harness, item: &QueueItemId) -> Vec<OperatorGate> {
    h.ctx
        .store
        .lock()
        .await
        .gates_for_item(item)
        .expect("gates")
}

async fn open_questions(h: &Harness) -> Vec<Question> {
    h.ctx.store.lock().await.questions(true).expect("questions")
}

/// A conventional gate page and its question, after checking the run waits.
async fn waiting_gate(
    h: &Harness,
    item: &QueueItemId,
    subject: GateSubject,
) -> (OperatorGate, Question) {
    let (gate, question) = waiting_gate_question(h, item, subject).await;
    assert!(
        gate.page
            .as_deref()
            .is_some_and(|p| p.ends_with("index.html")),
        "{gate:?}"
    );
    (gate, question)
}

/// The item's one open gate and its question, after checking the run waits.
async fn waiting_gate_question(
    h: &Harness,
    item: &QueueItemId,
    subject: GateSubject,
) -> (OperatorGate, Question) {
    let open: Vec<OperatorGate> = gates(h, item)
        .await
        .into_iter()
        .filter(OperatorGate::is_open)
        .collect();
    assert_eq!(open.len(), 1, "one open gate: {open:?}");
    let gate = open.into_iter().next().unwrap();
    assert_eq!(gate.subject, subject);
    let run = h.run(item).await;
    let waiting = RunState::AwaitingOperator {
        stage_id: gate.stage_id.clone(),
        attempt: gate.attempt,
        subject,
    };
    assert_eq!(run.state, waiting);
    assert!(
        h.running().await.is_empty(),
        "nothing runs while the gate waits"
    );
    let asked = gate.question.clone().expect("the gate was asked");
    let question = open_questions(h)
        .await
        .into_iter()
        .find(|q| q.id == asked)
        .expect("open question");
    assert_eq!(question.item.as_ref(), Some(item));
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem { item: item.clone() })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    assert_eq!(
        detail.waiting_reason.as_ref().unwrap().code,
        archon_core::protocol::WaitingReasonCode::OperatorGate
    );
    let wait = detail.waiting_on.as_ref().expect("derived gate wait");
    assert_eq!(wait.question.as_ref(), Some(&question.id));
    assert_eq!(wait.prompt, question.text);
    assert_eq!(
        wait.next_action,
        format!("archon answer {} approve", question.id)
    );
    assert_eq!(
        wait.subject,
        match subject {
            GateSubject::Proposal => "proposal",
            GateSubject::Report => "report",
        }
    );
    assert!(matches!(detail.item.state, QueueState::Running { .. }));
    let Reply::ItemSummaries(rows) = h
        .cli(Request::ListItemSummaries(Default::default()))
        .await
        .unwrap()
    else {
        panic!("summaries")
    };
    assert_eq!(
        rows.iter()
            .find(|r| &r.item.id == item)
            .unwrap()
            .waiting_on
            .as_ref(),
        Some(wait)
    );
    (gate, question)
}

async fn answer(h: &Harness, question: &Question, text: &str) {
    let request = Request::Answer {
        question: question.id.clone(),
        text: text.into(),
    };
    h.cli(request).await.expect("answer");
    h.dispatch().await;
}

/// Enqueue with `autonomy`, dispatch, and settle the first stage.
async fn to_first_gate(h: &Harness, title: &str) -> QueueItem {
    let item = enqueue_knobs(h, title, proposal()).await.expect("enqueue");
    assert_eq!(item.autonomy.proposal, ProposalMode::Lavish);
    assert_eq!(
        item.model_plan.choices_for("propose"),
        item.model_plan.choices_for("implement")
    );
    h.dispatch().await;
    let settled = settle(h).await;
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].stage_id, "propose", "the knob put propose first");
    item
}

#[tokio::test]
async fn an_approved_proposal_is_passed_to_implement() {
    let h = Harness::new().await;
    let item = to_first_gate(&h, "Add a greeting flag").await;
    let (gate, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    assert!(
        question
            .text
            .starts_with(&format!("Approve the direction for {}", item.id)),
        "{}",
        question.text
    );
    assert!(
        question.text.contains("Page: ") && question.text.contains(PROPOSAL),
        "{}",
        question.text
    );
    assert_eq!(question.options, ["yes", "reject"]);

    answer(&h, &question, "yes, keep it small").await;
    let decided = gates(&h, &item.id).await.remove(0);
    let note = Some("keep it small".to_owned());
    assert_eq!(decided.decision, Some(GateDecision::Approve { note }));
    assert!(decided.decided_at >= Some(gate.opened_at));
    let running = h.running().await;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].stage_id, "implement");
    let objective = h.backend.requests().last().unwrap().objective.clone();
    assert!(
        objective.contains("The operator approved this proposal"),
        "{objective}"
    );
    assert!(
        objective.contains(PROPOSAL) && objective.contains("keep it small"),
        "{objective}"
    );

    let repeat = Request::Answer {
        question: question.id.clone(),
        text: "reject".into(),
    };
    let refused = h.cli(repeat).await.expect_err("the question is closed");
    assert!(
        matches!(refused, ArchonError::InvalidTransition(_)),
        "{refused}"
    );
    for _ in 0..3 {
        settle(&h).await;
    }
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
}

#[tokio::test]
async fn revise_redoes_the_proposal_and_reject_cancels_the_item() {
    let h = Harness::new().await;
    let item = to_first_gate(&h, "Pick a cache").await;
    let (_, first) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    answer(&h, &first, "use an LRU instead").await;
    let running = h.running().await;
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("propose", 2)
    );
    let objective = h.backend.requests().last().unwrap().objective.clone();
    assert!(
        objective.contains("asked for changes") && objective.contains("use an LRU instead"),
        "{objective}"
    );

    settle(&h).await;
    let (second, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    assert_eq!(second.attempt, 2);
    assert_ne!(question.id, first.id, "each attempt asks anew");
    assert_eq!(
        (first.number, question.number),
        (1, 2),
        "each gate question gets the next number"
    );
    answer(&h, &question, "reject too risky").await;
    let state = h.state(&item.id).await;
    assert!(
        matches!(&state, QueueState::Cancelled { reason: Some(r) } if r.contains("too risky")),
        "{state:?}"
    );
    assert!(open_questions(&h).await.is_empty());
    assert_eq!(h.backend.requests().len(), 2, "no implementer ever ran");
}

#[tokio::test]
async fn an_open_gate_survives_a_restart_without_asking_twice() {
    let mut h = Harness::new().await;
    let item = to_first_gate(&h, "Survive a restart").await;
    let (gate, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    let (after, asked) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    assert_eq!((after, asked.id.clone()), (gate, question.id.clone()));
    assert_eq!(open_questions(&h).await.len(), 1, "not asked again");

    answer(&h, &question, "approve").await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "the rebuilt driver acts on the answer");
    assert_eq!(running[0].stage_id, "implement");
}

#[tokio::test]
async fn cancelling_withdraws_the_gate_and_closes_its_question() {
    let h = Harness::new().await;
    let item = to_first_gate(&h, "Cancel while waiting").await;
    let (_, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some("not now".into()),
    };
    h.cli(cancel).await.expect("cancel");

    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Cancelled { .. }
    ));
    let withdrawn = gates(&h, &item.id).await.remove(0);
    assert_eq!(
        withdrawn.withdrawn.as_deref(),
        Some("the run was cancelled")
    );
    assert!(withdrawn.decision.is_none() && withdrawn.decided_at.is_some());
    assert!(
        open_questions(&h).await.is_empty(),
        "chat stops offering it"
    );
    let late = Request::Answer {
        question: question.id.clone(),
        text: "yes".into(),
    };
    assert!(
        h.cli(late).await.is_err(),
        "a withdrawn gate takes no answer"
    );
}

/// The `Answer` feed entries of `question`.
async fn feed_answers(h: &Harness, question: &QuestionId) -> Vec<String> {
    let feed = h
        .ctx
        .store
        .lock()
        .await
        .feed_after(0, 10_000)
        .expect("feed");
    feed.into_iter()
        .filter_map(|item| match item.entry {
            FeedEntry::Answer {
                question: q, text, ..
            } if &q == question => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_restart_closes_a_question_left_open_on_a_withdrawn_gate() {
    let mut h = Harness::new().await;
    let item = to_first_gate(&h, "Crash after the finish").await;
    let (_, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    let withdrawn = vec!["withdrawn: the run was cancelled".to_owned()];
    assert_eq!(feed_answers(&h, &question.id).await, withdrawn);

    // A store written before the finish closed gate questions: the gate
    // withdrawn, its question still open.
    let stale = Question {
        id: QuestionId::new(),
        number: 0,
        answer: None,
        answered_at: None,
        ..question
    };
    let stale = {
        let mut store = h.ctx.store.lock().await;
        let stale = store.insert_question(&stale).expect("question");
        let mut gate = store.gates_for_item(&item.id).expect("gates").remove(0);
        gate.question = Some(stale.id.clone());
        store.update_gate(&gate).expect("relink");
        stale
    };
    assert_eq!(open_questions(&h).await, vec![stale.clone()]);

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    assert!(
        open_questions(&h).await.is_empty(),
        "chat stops offering it"
    );
    assert_eq!(feed_answers(&h, &stale.id).await, withdrawn);
}

#[tokio::test]
async fn a_report_before_the_pr_waits_and_revise_goes_back_to_implement() {
    let h = Harness::new().await;
    let knobs = AutonomyPatch {
        report: Some(ReportMode::BeforePr),
        ..AutonomyPatch::default()
    };
    let item = enqueue_knobs(&h, "Report first", knobs)
        .await
        .expect("enqueue");
    h.dispatch().await;
    for expected in [&["implement"][..], &["review", "review"], &["report"]] {
        let settled: Vec<String> = settle(&h).await.into_iter().map(|s| s.stage_id).collect();
        assert_eq!(settled, expected);
    }
    let (_, question) = waiting_gate(&h, &item.id, GateSubject::Report).await;
    assert!(
        question
            .text
            .starts_with("Approve the change report before the PR for"),
        "{}",
        question.text
    );

    answer(&h, &question, "cover the error path too").await;
    let running = h.running().await;
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("implement", 2)
    );
    for _ in 0..3 {
        settle(&h).await;
    }
    let (_, again) = waiting_gate(&h, &item.id, GateSubject::Report).await;
    answer(&h, &again, "lgtm").await;
    let ship = h.backend.requests().last().unwrap().clone();
    assert_eq!(ship.labels["archon.stage"], "ship");
    assert!(
        ship.objective.contains("## Change report"),
        "{}",
        ship.objective
    );
}

#[tokio::test]
async fn a_report_with_the_pr_never_waits() {
    let h = Harness::new().await;
    let knobs = AutonomyPatch {
        report: Some(ReportMode::WithPr),
        ..AutonomyPatch::default()
    };
    let item = enqueue_knobs(&h, "Report along", knobs)
        .await
        .expect("enqueue");
    h.dispatch().await;
    for _ in 0..4 {
        settle(&h).await;
    }
    assert!(matches!(
        h.state(&item.id).await,
        QueueState::Succeeded { .. }
    ));
    assert!(
        gates(&h, &item.id).await.is_empty(),
        "with-pr opens no gate"
    );
    let stages: Vec<String> = h
        .stages(&item.id)
        .await
        .into_iter()
        .map(|s| s.stage_id)
        .collect();
    assert_eq!(stages, ["implement", "review", "review", "report", "ship"]);
}

#[tokio::test]
async fn the_ceiling_refuses_until_the_operator_raises_it() {
    let h = Harness::new().await;
    let auto = AutonomyPatch {
        merge: Some(MergeMode::Auto),
        ..AutonomyPatch::default()
    };
    let refused = enqueue_knobs(&h, "Merge it", auto)
        .await
        .expect_err("above the ceiling");
    let ArchonError::Forbidden(message) = &refused else {
        panic!("a ceiling refusal is Forbidden: {refused:?}");
    };
    assert!(
        message.contains("merge=auto (allowed: manual)"),
        "{message}"
    );
    assert!(
        message.contains(REPO_NAME),
        "names where the ceiling applies: {message}"
    );

    let raise = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch {
            autonomy_max: Some(auto),
            ..RepoPatch::default()
        },
    };
    h.cli(raise).await.expect("the operator raises the ceiling");
    let allowed = enqueue_knobs(&h, "Merge it", auto)
        .await
        .expect("within the raised ceiling");
    assert_eq!(allowed.autonomy.merge, MergeMode::Auto);

    h.repo_config("[autonomy.max]\nmerge = \"manual\"\n");
    let lowered = enqueue_knobs(&h, "Merge it again", auto)
        .await
        .expect_err("the repo file lowers it");
    assert!(matches!(lowered, ArchonError::Forbidden(_)), "{lowered:?}");
}

/// `archon` tool names as a stage agent has them hidden: all but the one
/// that reaches the orchestrator, and Aether's scheduling tools.
fn stage_hidden(archon: Vec<String>) -> Vec<String> {
    let messaging = archon_core::mcp_tools::qualified("message_orchestrator");
    let schedules = ["create_schedule", "update_schedule", "list_schedules"];
    archon
        .into_iter()
        .filter(|name| name != &messaging)
        .chain(schedules.map(str::to_owned))
        .collect()
}

const DELEGATION_HEADING: &str = "\n## Delegation\n";

#[tokio::test]
async fn yolo_stage_agents_may_delegate_and_read_archon() {
    let h = Harness::new().await;
    let guarded = enqueue_knobs(&h, "Guarded", AutonomyPatch::default())
        .await
        .unwrap();
    let yolo = AutonomyPatch {
        tools: Some(ToolsMode::Yolo),
        ..AutonomyPatch::default()
    };
    let yolo = enqueue_knobs(&h, "Yolo", yolo).await.unwrap();
    assert_eq!(
        (guarded.autonomy.tools, yolo.autonomy.tools),
        (ToolsMode::Guarded, ToolsMode::Yolo)
    );
    h.dispatch().await;
    let requests = h.backend.requests();
    let spawned = |title: &str| {
        let request = requests.iter().find(|r| r.objective.contains(title));
        request.expect("spawned")
    };
    let guarded = spawned("brief for Guarded");
    assert_eq!(
        guarded.hidden_tools,
        stage_hidden(archon_core::mcp_tools::qualified_names())
    );
    assert!(
        guarded.leaf,
        "the built-in implement stage does not delegate"
    );
    assert_eq!(guarded.instructions.matches(DELEGATION_HEADING).count(), 0);

    let yolo = spawned("brief for Yolo");
    assert_eq!(
        yolo.hidden_tools,
        stage_hidden(archon_core::mcp_tools::mutating_names())
    );
    assert!(
        !yolo.hidden_tools.iter().any(|n| n.ends_with("get_item")),
        "reads are exposed"
    );
    assert!(!yolo.leaf, "yolo stage agents may delegate");
    // The stage does not set `allow_delegation`: yolo alone brings the rules.
    let rules = &yolo.instructions[yolo.instructions.find(DELEGATION_HEADING).unwrap()..];
    assert!(
        rules.contains("A child may delegate once more when its part splits further"),
        "{rules}"
    );
    assert!(rules.contains("policy for this item: any model Aether lists. Archon cancels"));
}

/// The test pipeline's stages, each allowing delegation.
const DELEGATING_PIPELINE: &str = r#"[pipeline]
name = "delegating"
description = "allows delegation"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"
allow_delegation = true

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 2
gate = "all_pass"
allow_delegation = true

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
allow_delegation = true
"#;

/// Stage delegation and yolo each grant children without changing tool exposure.
#[tokio::test]
async fn stage_delegation_allows_guarded_and_yolo_children() {
    let h = Harness::new().await;
    h.add_pipeline("delegating", DELEGATING_PIPELINE);
    let yolo = AutonomyPatch {
        tools: Some(ToolsMode::Yolo),
        ..AutonomyPatch::default()
    };
    for (title, autonomy) in [("Guarded", AutonomyPatch::default()), ("Yolo", yolo)] {
        enqueue_into(&h, "delegating", title, autonomy)
            .await
            .expect("enqueue");
    }
    h.dispatch().await;
    let requests = h.backend.requests();
    let spawned = |title: &str| {
        let brief = format!("brief for {title}");
        let request = requests.iter().find(|r| r.objective.contains(&brief));
        request.expect("spawned")
    };
    let (guarded, yolo) = (spawned("Guarded"), spawned("Yolo"));
    assert!(
        !guarded.leaf,
        "the stage explicitly permits guarded delegation"
    );
    assert!(!yolo.leaf, "a yolo agent may delegate");
    for request in [guarded, yolo] {
        assert_eq!(request.instructions.matches(DELEGATION_HEADING).count(), 1);
    }
    assert_eq!(
        guarded.hidden_tools,
        stage_hidden(archon_core::mcp_tools::qualified_names())
    );
    assert_eq!(
        yolo.hidden_tools,
        stage_hidden(archon_core::mcp_tools::mutating_names())
    );
}

const LAVISH_URL: &str = "http://127.0.0.1:4455/session/gate";

/// The fake `lavish-axi` from `archon-lavish/tests/fixtures`: every page
/// opens at [`LAVISH_URL`], and poll `n` returns what [`FakeAxi::prompt`] wrote.
struct FakeAxi {
    dir: tempfile::TempDir,
}

impl FakeAxi {
    fn new() -> (Self, Lavish) {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../archon-lavish/tests/fixtures/lavish-axi");
        let binary = dir.path().join("lavish-axi");
        std::os::unix::fs::symlink(fixture, &binary).unwrap();
        let config = LavishConfig {
            enabled: Toggle::On,
            binary,
        };
        let lavish = Lavish::detect(&config).unwrap().expect("the fake is found");
        let axi = Self { dir };
        let opened = format!("session:\n  url: \"{LAVISH_URL}\"\n  status: opened\n");
        axi.reply("open", &opened);
        (axi, lavish)
    }

    /// Written then renamed so a waiting poll never reads half a reply.
    fn reply(&self, name: &str, body: &str) {
        let staged = self.dir.path().join(format!(".{name}"));
        std::fs::write(&staged, body).unwrap();
        std::fs::rename(staged, self.dir.path().join(name)).unwrap();
    }

    /// The operator types `prompt` on the page; poll `poll` returns it.
    fn prompt(&self, poll: u32, prompt: &str) {
        let body = format!(
            "session:\n  status: feedback\nprompts[1]{{uid,prompt,selector,tag,text}}:\n  u1,\"{prompt}\",body,body,\n"
        );
        self.reply(&format!("poll.{poll}"), &body);
    }
}

/// Poll `check` until it holds, for at most ten seconds.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Answer the proposal gate of a fresh item on its Lavish page with
/// `prompt`; returns the item, the decided gate and the closed question.
async fn decide_on_the_page(
    h: &mut Harness,
    axi: &FakeAxi,
    prompt: &str,
) -> (QueueItem, OperatorGate, Question) {
    let item = to_first_gate(h, "Add a greeting flag").await;
    let (gate, question) = waiting_gate(h, &item.id, GateSubject::Proposal).await;
    assert_eq!(gate.location.as_deref(), Some(LAVISH_URL));
    assert!(gate.report.is_some(), "the page was published");
    assert!(
        question.text.contains(&format!("Page: {LAVISH_URL}")),
        "{}",
        question.text
    );
    let supervisor = tokio::spawn(reports::supervise(h.ctx.clone()));
    axi.prompt(1, prompt);
    eventually("the page decides the gate", || async {
        !gates(h, &item.id).await[0].is_open()
    })
    .await;
    supervisor.abort();
    let _ = supervisor.await;
    h.dispatch().await;
    let decided = gates(h, &item.id).await.remove(0);
    assert_eq!(decided.decided_at.is_some(), decided.decision.is_some());
    assert!(
        open_questions(h).await.iter().all(|q| q.id != question.id),
        "the chat question closes with the page's answer"
    );
    let held = archon_daemon::triggers::held(&h.ctx).await.unwrap();
    let forwarded: Vec<_> = held
        .into_iter()
        .filter(|event| matches!(event, TriggerEvent::LavishFeedback { .. }))
        .collect();
    assert!(
        forwarded.is_empty(),
        "a gate answer is not news for the orchestrator: {forwarded:?}"
    );
    (item, decided, question)
}

#[tokio::test]
async fn a_yes_on_the_lavish_page_approves_the_proposal() {
    let (axi, lavish) = FakeAxi::new();
    let mut h = Harness::with_lavish(Config::default(), lavish).await;
    let (item, decided, _) = decide_on_the_page(&mut h, &axi, "yes").await;
    assert_eq!(decided.decision, Some(GateDecision::Approve { note: None }));
    let running = h.running().await;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].stage_id, "implement");
    assert_eq!(h.run(&item.id).await.state.live_stage(), Some("implement"));
}

#[tokio::test]
async fn feedback_on_the_lavish_page_redoes_the_proposal() {
    let (axi, lavish) = FakeAxi::new();
    let mut h = Harness::with_lavish(Config::default(), lavish).await;
    let feedback = "Use one flag, not two";
    let (_, decided, _) = decide_on_the_page(&mut h, &axi, feedback).await;
    let revise = GateDecision::Revise {
        feedback: feedback.into(),
    };
    assert_eq!(decided.decision, Some(revise));
    let running = h.running().await;
    assert_eq!(running.len(), 1);
    assert_eq!(
        (running[0].stage_id.as_str(), running[0].attempt),
        ("propose", 2)
    );
    let objective = h.backend.requests().last().unwrap().objective.clone();
    assert!(objective.contains(feedback), "{objective}");
}

/// A pin with an empty prompt on the gate's page is no answer. It
/// neither decides the gate nor stops the report polls, so the operator's
/// next prompt still does.
#[tokio::test]
async fn a_blank_prompt_on_the_lavish_page_leaves_the_gate_to_the_next_answer() {
    let (axi, lavish) = FakeAxi::new();
    let h = Harness::with_lavish(Config::default(), lavish).await;
    let item = to_first_gate(&h, "Add a greeting flag").await;
    let supervisor = tokio::spawn(reports::supervise(h.ctx.clone()));
    axi.prompt(1, "");
    let polls = axi.dir.path().join("poll.count");
    eventually("the page is polled again", || async {
        std::fs::read_to_string(&polls).is_ok_and(|n| n.trim() == "2")
    })
    .await;
    assert!(
        !supervisor.is_finished(),
        "a blank prompt stopped the polls"
    );
    assert!(gates(&h, &item.id).await[0].is_open(), "blank is no answer");

    axi.prompt(2, "yes");
    eventually("the next answer decides the gate", || async {
        !gates(&h, &item.id).await[0].is_open()
    })
    .await;
    assert!(!supervisor.is_finished());
    supervisor.abort();
    let decided = gates(&h, &item.id).await.remove(0);
    assert_eq!(decided.decision, Some(GateDecision::Approve { note: None }));
}

/// The gate's page is published and its window closes: the gate question
/// and one "Reopen it?" question are open. Returns the item, the gate
/// question and the reopen question.
async fn gate_page_closed(h: &Harness, axi: &FakeAxi) -> (QueueItem, Question, Question) {
    let item = to_first_gate(h, "Close the window").await;
    let (_, asked) = waiting_gate(h, &item.id, GateSubject::Proposal).await;
    let supervisor = tokio::spawn(reports::supervise(h.ctx.clone()));
    axi.reply("poll.1", "session:\n  status: browser_disconnected\n");
    eventually("the operator is asked to reopen", || async {
        open_questions(h).await.len() == 2
    })
    .await;
    supervisor.abort();
    let _ = supervisor.await;
    let reopen = open_questions(h)
        .await
        .into_iter()
        .find(|q| q.id != asked.id)
        .expect("a reopen question");
    assert_eq!(reopen.options[0], "reopen", "{reopen:?}");
    assert_eq!(reopen.item.as_ref(), Some(&item.id));
    (item, asked, reopen)
}

const NO_GATE_WAITS: &str = "withdrawn: no gate waits on the report any more";

/// Deciding the gate closes the reopen question of its page.
#[tokio::test]
async fn deciding_the_gate_closes_the_reopen_question_of_its_page() {
    let (axi, lavish) = FakeAxi::new();
    let h = Harness::with_lavish(Config::default(), lavish).await;
    let (item, asked, reopen) = gate_page_closed(&h, &axi).await;
    answer(&h, &asked, "yes").await;
    assert!(!gates(&h, &item.id).await[0].is_open(), "decided");
    assert!(open_questions(&h).await.is_empty(), "both closed");
    assert_eq!(feed_answers(&h, &reopen.id).await, vec![NO_GATE_WAITS]);
}

/// A run that finishes while its gate waits closes the reopen
/// question of the gate's page with the gate's own question.
#[tokio::test]
async fn cancelling_the_run_closes_the_reopen_question_of_its_gate_page() {
    let (axi, lavish) = FakeAxi::new();
    let h = Harness::with_lavish(Config::default(), lavish).await;
    let (item, asked, reopen) = gate_page_closed(&h, &axi).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    assert!(open_questions(&h).await.is_empty(), "both closed");
    assert_eq!(
        feed_answers(&h, &asked.id).await,
        vec!["withdrawn: the run was cancelled"]
    );
    assert_eq!(feed_answers(&h, &reopen.id).await, vec![NO_GATE_WAITS]);
}

/// A restart closes the reopen questions no gate waits on, such as
/// the ones older versions asked about every informational page.
#[tokio::test]
async fn a_restart_closes_reopen_questions_no_gate_waits_on() {
    let mut h = Harness::new().await;
    let question = archon_daemon::feed::question(
        &h.ctx,
        "The review window for \"Merged\" was closed. Reopen it?".into(),
        vec!["reopen".into(), "leave it closed".into()],
        None,
    )
    .await
    .expect("question");
    let key = format!(
        "{}{}",
        reports::KEY_PREFIX,
        archon_core::id::ReportId::new()
    );
    let record = serde_json::json!({
        "html": h.dir.path().join("merged.html"),
        "title": "Merged",
        "item": null,
        "run": null,
        "state": "disconnected",
        "reopen_question": question.id,
    });
    let mut store = h.ctx.store.lock().await;
    store.kv_set(&key, &record.to_string()).expect("kv");
    drop(store);

    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    assert!(
        open_questions(&h).await.is_empty(),
        "chat stops offering it"
    );
    assert_eq!(feed_answers(&h, &question.id).await, vec![NO_GATE_WAITS]);
    let kept: serde_json::Value = serde_json::from_str(&h.kv(&key).await.unwrap()).unwrap();
    assert_eq!(kept["state"], "disconnected", "still resumable");
    assert_eq!(kept["reopen_question"], serde_json::Value::Null);
}

#[tokio::test]
async fn gate_questions_name_every_refused_report_even_when_another_page_is_shown() {
    for subject in [GateSubject::Proposal, GateSubject::Report] {
        let conventional = match subject {
            GateSubject::Proposal => ".archon/reports/proposal/index.html",
            GateSubject::Report => ".archon/reports/change/index.html",
        };
        for valid_path in [None, Some("custom-valid.html"), Some(conventional)] {
            let h = Harness::new().await;
            let knobs = match subject {
                GateSubject::Proposal => proposal(),
                GateSubject::Report => AutonomyPatch {
                    report: Some(ReportMode::BeforePr),
                    ..AutonomyPatch::default()
                },
            };
            let item = enqueue_knobs(&h, "refused gate reports", knobs)
                .await
                .unwrap();
            h.dispatch().await;
            if subject == GateSubject::Report {
                for _ in 0..2 {
                    settle(&h).await;
                }
            }
            let run = h.run(&item.id).await;
            let outside = h.dir.path().join("outside.html");
            std::fs::write(&outside, "outside the workspace").unwrap();
            let escaped = run.workspace.join("custom-proposal.html");
            std::os::unix::fs::symlink(&outside, &escaped).unwrap();
            let mut listed = vec!["custom-proposal.html", "../other-report.html"];
            let good = valid_path.map(|path| run.workspace.join(path));
            if let Some(good) = &good {
                std::fs::create_dir_all(good.parent().unwrap()).unwrap();
                std::fs::write(good, "valid gate page").unwrap();
                if valid_path != Some(conventional) {
                    listed.push("custom-valid.html");
                }
            }
            std::os::unix::fs::symlink(&outside, run.workspace.join("last-refused.html")).unwrap();
            listed.push("last-refused.html");
            h.settle_running(|_| StageOutcome::Completed {
                summary: "gate summary".into(),
                structured: Some(serde_json::json!({"reports": listed})),
            })
            .await;
            let (gate, question) = waiting_gate_question(&h, &item.id, subject).await;
            let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
            for (path, reason) in [
                ("custom-proposal.html", "resolves outside"),
                ("../other-report.html", "not a path inside"),
                ("last-refused.html", "resolves outside"),
            ] {
                assert!(
                    feed.iter().any(|f| matches!(&f.entry, FeedEntry::System { text }
                        if text.contains(path) && text.contains(reason) && text.contains("not published"))),
                    "{feed:?}"
                );
                assert!(question.text.contains(path), "{}", question.text);
                assert!(question.text.contains(reason), "{}", question.text);
            }
            assert!(
                !question.text.contains("no page was written"),
                "{}",
                question.text
            );
            let shown = reports::list(&h.ctx).await.unwrap();
            assert_eq!(shown.len(), usize::from(good.is_some()));
            assert_eq!(gate.report.is_some(), good.is_some());
            if let Some(good) = &good {
                assert_eq!(gate.page.as_deref(), good.to_str());
                assert_eq!(
                    std::fs::read_to_string(&shown[0].path).unwrap(),
                    "valid gate page"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_gate_names_a_listed_page_that_is_missing_or_not_a_regular_file() {
    for directory in [false, true] {
        let h = Harness::new().await;
        let item = enqueue_knobs(&h, "invalid gate file", proposal())
            .await
            .unwrap();
        h.dispatch().await;
        let run = h.run(&item.id).await;
        let page = run.workspace.join("listed.html");
        if directory {
            std::fs::create_dir(&page).unwrap();
        }
        h.settle_running(|_| StageOutcome::Completed {
            summary: "gate summary".into(),
            structured: Some(serde_json::json!({"reports": ["listed.html"]})),
        })
        .await;
        let (gate, question) = waiting_gate_question(&h, &item.id, GateSubject::Proposal).await;
        assert!(gate.report.is_none());
        assert!(question.text.contains("listed.html"), "{}", question.text);
        assert!(question.text.contains("not published"), "{}", question.text);
        assert!(
            !question.text.contains("no page was written"),
            "{}",
            question.text
        );
        let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
        assert!(
            feed.iter()
                .any(|f| matches!(&f.entry, FeedEntry::System { text }
            if text.contains("listed.html") && text.contains("not published"))),
            "{feed:?}"
        );
        assert!(reports::list(&h.ctx).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_gate_with_a_copy_refusal_shows_the_original_and_explains_its_retention() {
    let h = Harness::new().await;
    let item = enqueue_knobs(&h, "non durable proposal", proposal())
        .await
        .unwrap();
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let dir = run.workspace.join(".archon/reports/proposal");
    std::fs::create_dir_all(&dir).unwrap();
    let html = dir.join("index.html");
    std::fs::write(&html, "proposal").unwrap();
    std::os::unix::fs::symlink(&html, dir.join("linked-asset")).unwrap();
    h.settle_running(passing).await;
    let (gate, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    assert!(gate.report.is_some());
    assert!(question.text.contains(&html.display().to_string()));
    assert!(question.text.contains("could not be copied"));
    assert!(question.text.contains("not durable"));
    assert!(question.text.contains("workspace is collected"));
    let shown = reports::list(&h.ctx).await.unwrap().remove(0);
    assert_eq!(shown.path, html);
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    assert!(
        feed.iter()
            .any(|f| matches!(&f.entry, FeedEntry::System { text }
        if text.contains("not durable") && text.contains("Proposal:"))),
        "{feed:?}"
    );
}

#[tokio::test]
async fn an_answered_gate_does_not_request_another_approval_while_the_run_advances() {
    let h = Harness::new().await;
    let item = to_first_gate(&h, "pending continuation").await;
    let (mut gate, question) = waiting_gate(&h, &item.id, GateSubject::Proposal).await;
    let now = archon_core::now();
    {
        let mut store = h.ctx.store.lock().await;
        store.answer_question(&question.id, "approve", now).unwrap();
    }
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
        "an answered question needs no further operator action"
    );
    gate.decision = Some(GateDecision::Approve { note: None });
    gate.decided_at = Some(now);
    h.ctx.store.lock().await.update_gate(&gate).unwrap();
    let Reply::ItemSummaries(rows) = h
        .cli(Request::ListItemSummaries(Default::default()))
        .await
        .unwrap()
    else {
        panic!("summaries")
    };
    assert!(
        rows.iter()
            .find(|r| r.item.id == item.id)
            .unwrap()
            .waiting_on
            .is_none()
    );
    assert!(matches!(
        h.run(&item.id).await.state,
        RunState::AwaitingOperator { .. }
    ));
}

/// Enqueue into `adaptive`, with a model for each of its two stages.
async fn enqueue_adaptive(
    h: &Harness,
    title: &str,
    autonomy: AutonomyPatch,
) -> Result<QueueItem, ArchonError> {
    let stages = ["build", "ship"].map(|stage| (stage.to_owned(), plan().0["implement"].clone()));
    let request = EnqueueRequest {
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("adaptive".into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: archon_core::model::ModelPlan(stages.into()),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy,
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

/// `adaptive` declares the review its lead holds, so it runs at
/// `standard`: the knob, the stored item and the ceiling all see that.
#[tokio::test]
async fn adaptive_runs_at_the_standard_review_it_declares() {
    use archon_core::autonomy::ReviewLevel;

    let h = Harness::new().await;
    let asking = |review| AutonomyPatch {
        review: Some(review),
        ..AutonomyPatch::default()
    };
    for other in [ReviewLevel::Strict, ReviewLevel::None] {
        let error = enqueue_adaptive(&h, "another review", asking(other))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("runs review=standard"), "{error}");
        assert!(error.contains(other.pipeline()), "{error}");
    }
    assert!(h.backend.requests().is_empty());

    for patch in [AutonomyPatch::default(), asking(ReviewLevel::Standard)] {
        let item = enqueue_adaptive(&h, "an ordinary change", patch)
            .await
            .expect("standard is what adaptive runs");
        assert_eq!(item.pipeline, "adaptive");
        assert_eq!(item.autonomy.review, ReviewLevel::Standard);
    }
}

/// A ceiling that demands gated review refuses `adaptive`, whose review is
/// its lead's own; one that allows `standard` lets it through.
#[tokio::test]
async fn a_ceiling_stricter_than_standard_refuses_adaptive() {
    let h = Harness::new().await;
    h.repo_config("[autonomy.max]\nreview = \"strict\"\n");
    let refused = enqueue_adaptive(&h, "under strict", AutonomyPatch::default())
        .await
        .expect_err("standard is above a strict ceiling");
    let ArchonError::Forbidden(message) = &refused else {
        panic!("a ceiling refusal is Forbidden: {refused:?}");
    };
    assert!(
        message.contains("review=standard (allowed: strict)"),
        "{message}"
    );
    assert!(h.backend.requests().is_empty());

    h.repo_config("[autonomy.max]\nreview = \"standard\"\n");
    let allowed = enqueue_adaptive(&h, "under standard", AutonomyPatch::default())
        .await
        .expect("a standard ceiling allows the review adaptive declares");
    assert_eq!(
        allowed.autonomy.review,
        archon_core::autonomy::ReviewLevel::Standard
    );
}
