//! Report publishing and feedback routing with the fake lavish-axi from
//! `archon-lavish/tests/fixtures`, and the plain-path fallback without it.

use crate::link_common;

use std::path::{Path, PathBuf};

use archon_core::config::{Config, LavishConfig, Toggle};
use archon_core::error::ArchonError;
use archon_core::id::ReportId;
use archon_core::notify::{AnswerVia, FeedEntry, Question};
use archon_core::protocol::{ClientKind, Reply, ReportState, Request};
use archon_core::run::{GateDecision, GateSubject, OperatorGate};
use archon_daemon::triggers::TriggerEvent;
use archon_daemon::{executor, handlers};
use archon_daemon::{reports, supervisor};
use archon_lavish::Lavish;
use link_common::Harness;
use serde_json::Value;
use tempfile::TempDir;

/// A scratch dir holding the fake binary, its canned replies and a report.
struct FakeAxi {
    dir: TempDir,
    html: PathBuf,
}

impl FakeAxi {
    fn new() -> (Self, Lavish) {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../archon-lavish/tests/fixtures/lavish-axi");
        let binary = dir.path().join("lavish-axi");
        std::os::unix::fs::symlink(fixture, &binary).unwrap();
        let pages = dir.path().join("pages");
        std::fs::create_dir(&pages).unwrap();
        let html = pages.join("report.html");
        std::fs::write(&html, "<h1>findings</h1>").unwrap();
        let config = LavishConfig {
            enabled: Toggle::On,
            binary,
        };
        let lavish = Lavish::detect(&config).unwrap().unwrap();
        let html = std::fs::canonicalize(html).unwrap();
        (Self { dir, html }, lavish)
    }

    /// Written then renamed so a waiting poll never reads half a reply.
    fn reply(&self, name: &str, body: &str) {
        let staged = self.dir.path().join(format!(".{name}"));
        std::fs::write(&staged, body).unwrap();
        std::fs::rename(staged, self.dir.path().join(name)).unwrap();
    }

    fn opens_at(&self, url: &str) {
        self.reply(
            "open",
            &format!("session:\n  url: \"{url}\"\n  status: opened\n"),
        );
    }

    fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default();
        calls.lines().map(str::to_owned).collect()
    }
}

const URL: &str = "http://127.0.0.1:4455/session/r1";

/// The state of the one report recorded, from its own `kv` entry.
async fn report_state(harness: &Harness) -> Option<String> {
    let store = harness.ctx.store.lock().await;
    let records = store.kv_prefix(reports::KEY_PREFIX).unwrap();
    assert!(records.len() <= 1, "{records:?}");
    let (_, raw) = records.first()?;
    let record: Value = serde_json::from_str(raw).unwrap();
    record["state"].as_str().map(str::to_owned)
}

async fn report_html(harness: &Harness) -> PathBuf {
    reports::list(&harness.ctx).await.unwrap().remove(0).path
}

const DISCONNECTED: &str = "session:\n  status: browser_disconnected\n";

/// An open proposal gate of a live run showing `report`, as announcing a
/// gate leaves it.
async fn gate_on(harness: &Harness, report: &ReportId) -> OperatorGate {
    let stage =
        link_common::running_stage(&harness.ctx, harness.dir.path(), "agt_0301", "tsk_0301").await;
    let mut store = harness.ctx.store.lock().await;
    let run = store.run(&stage.run).unwrap();
    let gate = OperatorGate {
        run: run.id,
        item: run.item,
        stage_id: "propose".into(),
        attempt: 1,
        subject: GateSubject::Proposal,
        summary: "the plan".into(),
        page: None,
        location: Some(URL.into()),
        report: Some(report.clone()),
        question: None,
        decision: None,
        withdrawn: None,
        opened_at: archon_core::now(),
        decided_at: None,
    };
    let gate = store.open_gate(&gate).unwrap();
    assert!(gate.is_open());
    gate
}

/// The open questions, oldest first.
async fn open_questions(harness: &Harness) -> Vec<Question> {
    harness.ctx.store.lock().await.questions(true).unwrap()
}

/// Wait for the window's close to be recorded; poll `poll` returns it.
async fn window_closes(harness: &Harness, axi: &FakeAxi, poll: u32) {
    axi.reply(&format!("poll.{poll}"), DISCONNECTED);
    harness
        .eventually("the window's close is recorded", || async {
            report_state(harness).await.as_deref() == Some("disconnected")
        })
        .await;
}

/// Publish a page an open gate waits on, with the poll supervisor running,
/// then have its window close; returns the gate and the one reopen question.
async fn disconnected_gated_report(harness: &Harness, axi: &FakeAxi) -> (OperatorGate, Question) {
    let published = reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let gate = gate_on(harness, &published.report).await;
    window_closes(harness, axi, 1).await;
    let open = open_questions(harness).await;
    assert_eq!(open.len(), 1, "{open:?}");
    let feed = harness.feed().await;
    assert!(
        matches!(feed.last(), Some(FeedEntry::Question(q)) if *q == open[0]),
        "announced: {feed:?}"
    );
    (gate, open[0].clone())
}

async fn cli(harness: &Harness, request: Request) -> Reply {
    handlers::handle(&harness.ctx, ClientKind::ArchonCli, request)
        .await
        .unwrap()
}

async fn answer(harness: &Harness, question: &Question, text: &str) {
    let request = Request::Answer {
        chat: false,
        question: question.id.clone(),
        text: text.into(),
    };
    cli(harness, request).await;
}

/// The `Answer` feed entries that closed `question`.
async fn answers_to(harness: &Harness, question: &Question) -> Vec<String> {
    let feed = harness.feed().await;
    feed.into_iter()
        .filter_map(|entry| match entry {
            FeedEntry::Answer {
                question: q, text, ..
            } if q == question.id => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn without_lavish_a_report_degrades_to_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let absent = LavishConfig {
        enabled: Toggle::Auto,
        binary: dir.path().join("no-such-lavish-axi"),
    };
    let lavish = Lavish::detect(&absent).unwrap();
    assert!(lavish.is_none(), "auto mode tolerates a missing binary");
    let harness = Harness::with(Config::default(), lavish).await;
    let html = dir.path().join("r.html");
    std::fs::write(&html, "<p>x</p>").unwrap();

    let published = reports::publish(&harness.ctx, " Findings ", &html, None)
        .await
        .unwrap();
    assert!(!published.published);
    assert_eq!(
        published.location,
        report_html(&harness).await.display().to_string()
    );
    let feed = harness.feed().await;
    assert!(
        matches!(&feed[..], [FeedEntry::Report { title, location, .. }] if title == "Findings" && *location == published.location),
        "{feed:?}"
    );
    assert_eq!(report_state(&harness).await.as_deref(), Some("unpublished"));
}

#[tokio::test]
async fn a_lavish_failure_falls_back_to_the_path_and_says_why() {
    let (axi, lavish) = FakeAxi::new();
    std::fs::write(axi.dir.path().join("open.exit"), "3").unwrap();
    axi.reply("open", "error: no browser\n");
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let published = reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    assert!(!published.published);
    assert_eq!(
        published.location,
        report_html(&harness).await.display().to_string()
    );
    let notes = harness.system_notes().await;
    assert!(
        notes.len() == 1 && notes[0].contains("no browser"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn bad_report_paths_are_refused_before_lavish_runs() {
    let (axi, lavish) = FakeAxi::new();
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let text = axi.dir.path().join("notes.txt");
    std::fs::write(&text, "x").unwrap();
    for (path, why) in [
        (PathBuf::from("report.html"), "relative"),
        (text, "not html"),
        (axi.dir.path().join("gone.html"), "missing"),
    ] {
        let result = reports::publish(&harness.ctx, "R", &path, None).await;
        assert!(result.is_err(), "{why} path was accepted");
    }
    assert!(
        reports::publish(&harness.ctx, "  ", &axi.html, None)
            .await
            .is_err()
    );
    assert!(axi.calls().is_empty(), "{:?}", axi.calls());
}

#[tokio::test]
async fn an_open_report_feeds_prompts_to_the_orchestrator_until_ended() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let published = reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    assert!(published.published);
    assert_eq!(published.location, URL);
    // Started after publishing: the poll comes from the stored record.
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));

    axi.reply(
        "poll.1",
        "session:\n  status: feedback\nprompts[1]{uid,prompt,selector,tag,text}:\n  u1,\"Split the table, please\",table,table,\n",
    );
    let TriggerEvent::LavishFeedback {
        report, prompts, ..
    } = harness.next_trigger().await
    else {
        panic!("expected lavish feedback");
    };
    assert_eq!(report, published.report);
    assert_eq!(prompts, vec!["Split the table, please"]);

    axi.reply("poll.2", "session:\n  status: ended\n  ended_by: user\n");
    let TriggerEvent::LavishFeedback { text, .. } = harness.next_trigger().await else {
        panic!("expected the end to be reported");
    };
    assert!(text.contains("ended"));
    harness
        .eventually("the report is marked ended", || async {
            report_state(&harness).await.as_deref() == Some("ended")
        })
        .await;
    supervisor.abort();
    let polls = axi.calls().iter().filter(|c| c.starts_with("poll")).count();
    assert_eq!(polls, 2, "an ended session is not polled again");
}

#[tokio::test]
async fn a_closed_review_window_a_gate_waits_on_asks_before_reopening() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    // Started before publishing: the poll is started from the feed entry.
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (gate, question) = disconnected_gated_report(&harness, &axi).await;
    assert_eq!(question.options, vec!["reopen", "leave it closed"]);
    assert_eq!(
        question.item,
        Some(gate.item),
        "asked about the gate's item"
    );
    assert!(
        question.aether.is_none(),
        "answered through an Archon trigger"
    );
    assert_eq!(
        report_state(&harness).await.as_deref(),
        Some("disconnected")
    );

    answer(&harness, &question, "leave it closed").await;
    supervisor.abort();
    assert_eq!(
        report_state(&harness).await.as_deref(),
        Some("disconnected")
    );
    assert!(
        harness.drain_triggers().await.is_empty(),
        "the answer is Archon's, not news for the orchestrator"
    );
    let calls = axi.calls();
    assert_eq!(calls.iter().filter(|c| !c.starts_with("poll")).count(), 1);
    assert!(
        !calls.iter().any(|c| c.starts_with("end")),
        "never ended uninvited"
    );
}

#[tokio::test]
async fn answering_reopen_reopens_the_report_and_polls_it_again() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (_, question) = disconnected_gated_report(&harness, &axi).await;

    answer(&harness, &question, "Reopen").await;
    let polls = axi.dir.path().join("poll.count");
    harness
        .eventually("the reopened window is polled", || async {
            std::fs::read_to_string(&polls).is_ok_and(|n| n.trim() == "2")
        })
        .await;
    supervisor.abort();
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
    assert!(open_questions(&harness).await.is_empty(), "answered");
    let recorded = vec!["Reopen".to_owned()];
    assert_eq!(answers_to(&harness, &question).await, recorded, "once");
    let opens = axi
        .calls()
        .iter()
        .filter(|c| !c.starts_with("poll"))
        .count();
    assert_eq!(opens, 2, "published, then reopened once");
    let reports: Vec<_> = harness
        .feed()
        .await
        .into_iter()
        .filter(|e| matches!(e, FeedEntry::Report { .. }))
        .collect();
    assert_eq!(reports.len(), 2, "the reopened window is announced");
    assert!(
        harness.drain_triggers().await.is_empty(),
        "the answer is Archon's, not news for the orchestrator"
    );
}

/// An informational page (no gate waits on it) whose window closes
/// asks nothing, is listed as disconnected, and reopens on demand.
#[tokio::test]
async fn a_closed_window_no_gate_waits_on_asks_nothing_and_reopens_on_demand() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let published = reports::publish(&harness.ctx, "Merged PR", &axi.html, None)
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    window_closes(&harness, &axi, 1).await;
    assert!(
        open_questions(&harness).await.is_empty(),
        "nothing is asked"
    );
    assert!(
        !harness
            .feed()
            .await
            .iter()
            .any(|e| matches!(e, FeedEntry::Question(_))),
        "nothing reaches the feed"
    );

    let Reply::Reports(listed) = cli(&harness, Request::ListReports).await else {
        panic!("expected the reports");
    };
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].report, published.report);
    assert_eq!(listed[0].state, ReportState::Disconnected);
    assert_eq!(listed[0].location.as_deref(), Some(URL));
    assert_eq!(listed[0].reopen_question, None);

    let reopen = Request::ReopenReport {
        report: published.report.clone(),
    };
    let Reply::Report(reopened) = cli(&harness, reopen).await else {
        panic!("expected the reopened report");
    };
    assert_eq!(reopened.report, published.report, "the same report");
    assert!(reopened.published && reopened.location == URL);
    axi.reply(
        "poll.2",
        "session:\n  status: feedback\nprompts[1]{uid,prompt,selector,tag,text}:\n  u1,\"Thanks\",body,body,\n",
    );
    let TriggerEvent::LavishFeedback { report, .. } = harness.next_trigger().await else {
        panic!("expected feedback from the reopened window");
    };
    supervisor.abort();
    assert_eq!(report, published.report);
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
    let opens = axi
        .calls()
        .iter()
        .filter(|c| !c.starts_with("poll"))
        .count();
    assert_eq!(opens, 2, "published, then reopened on demand");

    let unknown = Request::ReopenReport {
        report: ReportId::new(),
    };
    let refused = handlers::handle(&harness.ctx, ClientKind::ArchonCli, unknown).await;
    assert!(
        matches!(refused, Err(ArchonError::NotFound(_))),
        "{refused:?}"
    );
}

/// Publishing a file again reuses its report and session, and closes
/// the reopen question it asked, so one file never has two open.
#[tokio::test]
async fn republishing_a_file_reuses_its_report_and_asks_at_most_once() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (gate, first) = disconnected_gated_report(&harness, &axi).await;

    let again = reports::publish(&harness.ctx, "Findings, again", &axi.html, None)
        .await
        .unwrap();
    assert_eq!(Some(again.report.clone()), gate.report, "the same report");
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
    assert!(
        open_questions(&harness).await.is_empty(),
        "the first closed"
    );
    let shown = vec!["withdrawn: the report was shown again".to_owned()];
    assert_eq!(answers_to(&harness, &first).await, shown);

    window_closes(&harness, &axi, 2).await;
    supervisor.abort();
    let open = open_questions(&harness).await;
    assert_eq!(open.len(), 1, "one question for the one file: {open:?}");
    assert_ne!(open[0].id, first.id, "asked anew for the new window");
    let opens = axi
        .calls()
        .iter()
        .filter(|c| !c.starts_with("poll"))
        .count();
    assert_eq!(opens, 2, "each publish resumes the file's one session");
}

/// A reopen question closes once no gate waits on its report, and one
/// whose gate still waits is kept.
#[tokio::test]
async fn a_reopen_question_closes_once_its_gate_closes() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (mut gate, question) = disconnected_gated_report(&harness, &axi).await;
    supervisor.abort();
    let closed = reports::close_stale_questions(&harness.ctx).await.unwrap();
    assert_eq!(closed, 0, "its gate still waits");
    assert_eq!(open_questions(&harness).await, vec![question.clone()]);

    gate.decision = Some(GateDecision::Approve { note: None });
    gate.decided_at = Some(archon_core::now());
    harness.ctx.store.lock().await.update_gate(&gate).unwrap();
    let closed = reports::close_stale_questions(&harness.ctx).await.unwrap();
    assert_eq!(closed, 1);
    assert!(open_questions(&harness).await.is_empty());
    let withdrawn = vec!["withdrawn: no gate waits on the report any more".to_owned()];
    assert_eq!(answers_to(&harness, &question).await, withdrawn);
    let Reply::Reports(listed) = cli(&harness, Request::ListReports).await else {
        panic!("expected the reports");
    };
    assert_eq!(
        listed[0].state,
        ReportState::Disconnected,
        "still resumable"
    );
    assert_eq!(listed[0].reopen_question, None);
    assert_eq!(
        reports::close_stale_questions(&harness.ctx).await.unwrap(),
        0,
        "closed once"
    );
}

/// Decide `gate` in the store alone, so no reopen question is swept yet.
async fn decide(harness: &Harness, gate: &mut OperatorGate) {
    gate.decision = Some(GateDecision::Approve { note: None });
    gate.decided_at = Some(archon_core::now());
    harness.ctx.store.lock().await.update_gate(gate).unwrap();
}

fn answer_request(question: &Question, text: &str) -> Request {
    Request::Answer {
        chat: false,
        question: question.id.clone(),
        text: text.into(),
    }
}

/// An answer routed after the sweep closed its question (read open,
/// then swept) is refused with the reason, reaches no one, and reopens
/// nothing; answered afresh, the same reason is given.
#[tokio::test]
async fn an_answer_the_sweep_beat_is_refused_and_reaches_no_one() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (mut gate, question) = disconnected_gated_report(&harness, &axi).await;
    supervisor.abort();
    decide(&harness, &mut gate).await;
    assert_eq!(
        reports::close_stale_questions(&harness.ctx).await.unwrap(),
        1
    );

    let routed =
        supervisor::route_answer(&harness.ctx, &question, "reopen", AnswerVia::Archon).await;
    let Err(ArchonError::InvalidTransition(why)) = routed else {
        panic!("expected a refusal: {routed:?}");
    };
    assert!(why.contains("its report's gate closed"), "{why}");
    let refused = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        answer_request(&question, "reopen"),
    )
    .await;
    let Err(ArchonError::InvalidTransition(again)) = refused else {
        panic!("expected a refusal: {refused:?}");
    };
    assert_eq!(again, why, "the same reason either way");
    assert!(
        harness.drain_triggers().await.is_empty(),
        "no stray operator_answer"
    );
    let withdrawn = vec!["withdrawn: no gate waits on the report any more".to_owned()];
    assert_eq!(answers_to(&harness, &question).await, withdrawn);
    assert_eq!(
        report_state(&harness).await.as_deref(),
        Some("disconnected")
    );
    let opens = axi
        .calls()
        .iter()
        .filter(|c| !c.starts_with("poll"))
        .count();
    assert_eq!(opens, 1, "nothing was reopened");
}

/// Once the operator's answer closed the question, the sweep finds
/// nothing to close and says nothing.
#[tokio::test]
async fn the_sweep_after_an_answer_stays_silent() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (mut gate, question) = disconnected_gated_report(&harness, &axi).await;
    supervisor.abort();

    let Reply::Question(answered) =
        cli(&harness, answer_request(&question, "leave it closed")).await
    else {
        panic!("expected the answered question");
    };
    assert_eq!(answered.answer.as_deref(), Some("leave it closed"));
    decide(&harness, &mut gate).await;
    let before = harness.feed().await.len();
    assert_eq!(
        reports::close_stale_questions(&harness.ctx).await.unwrap(),
        0
    );
    assert_eq!(harness.feed().await.len(), before, "nothing echoed");
    let recorded = vec!["leave it closed".to_owned()];
    assert_eq!(answers_to(&harness, &question).await, recorded, "once");
    assert!(harness.drain_triggers().await.is_empty());
}

/// An answer and the sweep racing close the question exactly once,
/// whichever wins, and never trigger the orchestrator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_racing_the_sweep_closes_the_question_once() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (mut gate, question) = disconnected_gated_report(&harness, &axi).await;
    supervisor.abort();
    decide(&harness, &mut gate).await;

    let (ctx, request) = (
        harness.ctx.clone(),
        answer_request(&question, "leave it closed"),
    );
    let answering =
        tokio::spawn(async move { handlers::handle(&ctx, ClientKind::ArchonCli, request).await });
    let swept = reports::close_stale_questions(&harness.ctx).await.unwrap();
    let answered = answering.await.unwrap();
    let answers = answers_to(&harness, &question).await;
    assert_eq!(answers.len(), 1, "closed once: {answers:?}");
    match answered {
        Ok(_) => assert_eq!((swept, answers[0].as_str()), (0, "leave it closed")),
        Err(ArchonError::InvalidTransition(why)) => {
            assert_eq!(swept, 1);
            assert!(why.contains("its report's gate closed"), "{why}");
        }
        Err(other) => panic!("unexpected failure: {other}"),
    }
    assert!(
        harness.drain_triggers().await.is_empty(),
        "no stray trigger"
    );
}

/// Mark the one report open again with no reopen question, as publishing
/// its file did in older builds, which left the question it asked open.
async fn republish_as_an_older_build(harness: &Harness) {
    let records = harness
        .ctx
        .store
        .lock()
        .await
        .kv_prefix(reports::KEY_PREFIX)
        .unwrap();
    let [(key, raw)] = records.as_slice() else {
        panic!("expected one report: {records:?}");
    };
    let mut record: Value = serde_json::from_str(raw).unwrap();
    assert!(record["reopen_question"].is_string(), "{record}");
    record["state"] = "open".into();
    record["reopen_question"] = Value::Null;
    harness.set_kv(key, &record.to_string()).await;
}

/// A gated page an older build asked about twice, published again in
/// between: returns its gate, the orphan no record asks any more, and the
/// question its report asks.
async fn orphaned_then_asked_again(
    harness: &Harness,
    axi: &FakeAxi,
) -> (OperatorGate, Question, Question) {
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (gate, orphan) = disconnected_gated_report(harness, axi).await;
    supervisor.abort();
    republish_as_an_older_build(harness).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    window_closes(harness, axi, 2).await;
    supervisor.abort();
    let open = open_questions(harness).await;
    assert_eq!(open.len(), 2, "two questions for one page: {open:?}");
    assert_eq!(open[0], orphan);
    assert_eq!(open[0].text, open[1].text, "both about the same page");
    (gate, orphan, open[1].clone())
}

const NO_GATE: &str = "withdrawn: no gate waits on the report any more";

/// The startup sweep closes both questions a republished page left,
/// the orphan included, once no gate waits on the page.
#[tokio::test]
async fn the_sweep_closes_every_reopen_question_a_republished_page_left() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let (mut gate, orphan, current) = orphaned_then_asked_again(&harness, &axi).await;
    decide(&harness, &mut gate).await;

    let swept = reports::close_stale_questions(&harness.ctx).await.unwrap();
    assert_eq!(swept, 2, "the orphan and the asked one");
    assert!(open_questions(&harness).await.is_empty());
    let withdrawn = vec![NO_GATE.to_owned()];
    assert_eq!(answers_to(&harness, &orphan).await, withdrawn);
    assert_eq!(answers_to(&harness, &current).await, withdrawn);
    assert_eq!(
        reports::close_stale_questions(&harness.ctx).await.unwrap(),
        0,
        "closed once"
    );
    assert!(harness.drain_triggers().await.is_empty());
}

/// While the gate waits, the sweep keeps the question the report asks
/// and closes the orphan, whose late answer is refused with the reason.
#[tokio::test]
async fn while_its_gate_waits_the_sweep_keeps_only_the_question_asked() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let (_, orphan, current) = orphaned_then_asked_again(&harness, &axi).await;

    let swept = reports::close_stale_questions(&harness.ctx).await.unwrap();
    assert_eq!(swept, 1, "only the orphan");
    assert_eq!(open_questions(&harness).await, vec![current]);
    let superseded = vec!["withdrawn: the report no longer asks it".to_owned()];
    assert_eq!(answers_to(&harness, &orphan).await, superseded);

    let refused = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        answer_request(&orphan, "reopen"),
    )
    .await;
    let Err(ArchonError::InvalidTransition(why)) = refused else {
        panic!("expected a refusal: {refused:?}");
    };
    assert!(why.contains("its report no longer asks it"), "{why}");
    assert!(harness.drain_triggers().await.is_empty());
}

/// Publishing the page again closes every question about it, the
/// orphan an older build left included.
#[tokio::test]
async fn republishing_closes_every_reopen_question_about_the_page() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let (gate, orphan, current) = orphaned_then_asked_again(&harness, &axi).await;

    let again = reports::publish(&harness.ctx, "Merged PR #1", &axi.html, None)
        .await
        .unwrap();
    assert_eq!(Some(again.report), gate.report, "the same report");
    assert!(open_questions(&harness).await.is_empty());
    let shown = vec!["withdrawn: the report was shown again".to_owned()];
    assert_eq!(answers_to(&harness, &orphan).await, shown);
    assert_eq!(answers_to(&harness, &current).await, shown);
}

/// An answer to an orphan is Archon's: it closes the orphan, reaches
/// no orchestrator and reopens nothing; asked to reopen, it says how.
#[tokio::test]
async fn an_answer_to_an_orphaned_reopen_question_never_reaches_the_orchestrator() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let (gate, orphan, current) = orphaned_then_asked_again(&harness, &axi).await;

    let Reply::Question(answered) = cli(&harness, answer_request(&orphan, "reopen")).await else {
        panic!("expected the answered question");
    };
    assert_eq!(answered.answer.as_deref(), Some("reopen"));
    assert_eq!(
        answers_to(&harness, &orphan).await,
        vec!["reopen".to_owned()]
    );
    assert_eq!(open_questions(&harness).await, vec![current], "kept");
    assert!(
        harness.drain_triggers().await.is_empty(),
        "no stray operator_answer"
    );
    let opens = axi
        .calls()
        .iter()
        .filter(|c| !c.starts_with("poll"))
        .count();
    assert_eq!(opens, 1, "nothing was reopened");
    let report = gate.report.unwrap();
    let notes = harness.system_notes().await;
    assert!(
        notes.len() == 1 && notes[0].contains(&format!("archon reports reopen {report}")),
        "{notes:?}"
    );
}

/// An orphan about a page no longer recorded is closed by its answer
/// too, and "leave it closed" adds no note.
#[tokio::test]
async fn an_answer_to_an_orphan_about_an_unrecorded_page_closes_it() {
    let mut harness = Harness::new().await;
    let orphan = Question {
        id: archon_core::id::QuestionId::new(),
        number: 0,
        text: "The review window for \"Gone\" was closed. Reopen it? (The report is at /gone/index.html.)".into(),
        options: vec!["reopen".into(), "leave it closed".into()],
        item: None,
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let orphan = harness
        .ctx
        .store
        .lock()
        .await
        .insert_question(&orphan)
        .unwrap();

    let Reply::Question(answered) = cli(&harness, answer_request(&orphan, "leave it closed")).await
    else {
        panic!("expected the answered question");
    };
    assert_eq!(answered.answer.as_deref(), Some("leave it closed"));
    assert!(open_questions(&harness).await.is_empty());
    assert!(harness.drain_triggers().await.is_empty());
    assert!(harness.system_notes().await.is_empty());
}

/// `archon reports reopen` shows an ended report again with
/// `--reopen`; a failed attempt leaves it as it was.
#[tokio::test]
async fn reopening_an_ended_report_asks_lavish_to_reopen_it() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let published = reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    axi.reply("poll.1", "session:\n  status: ended\n  ended_by: user\n");
    harness.next_trigger().await;
    harness
        .eventually("the report ends", || async {
            report_state(&harness).await.as_deref() == Some("ended")
        })
        .await;

    std::fs::write(axi.dir.path().join("open.2.exit"), "3").unwrap();
    axi.reply("open.2", "error: server down\n");
    let reopen = Request::ReopenReport {
        report: published.report.clone(),
    };
    let failed = handlers::handle(&harness.ctx, ClientKind::ArchonCli, reopen.clone()).await;
    let Err(ArchonError::Unavailable(why)) = failed else {
        panic!("expected the failure: {failed:?}");
    };
    assert!(
        why.contains("server down") && why.contains("still ended"),
        "{why}"
    );
    assert_eq!(
        report_state(&harness).await.as_deref(),
        Some("ended"),
        "unchanged"
    );

    let Reply::Report(reopened) = cli(&harness, reopen).await else {
        panic!("expected the reopened report");
    };
    assert!(
        reopened.published && reopened.location == URL,
        "{reopened:?}"
    );
    let reopen_call = format!("{} --reopen", report_html(&harness).await.display());
    let opens: Vec<_> = axi
        .calls()
        .into_iter()
        .filter(|c| !c.starts_with("poll"))
        .collect();
    assert_eq!(opens[1..], [reopen_call.clone(), reopen_call], "{opens:?}");
    axi.reply(
        "poll.2",
        "session:\n  status: feedback\nprompts[1]{uid,prompt,selector,tag,text}:\n  u1,\"More\",body,body,\n",
    );
    let TriggerEvent::LavishFeedback { report, .. } = harness.next_trigger().await else {
        panic!("expected feedback from the reopened window");
    };
    supervisor.abort();
    assert_eq!(report, published.report);
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
}

/// Answering "reopen" when Lavish cannot show the page records the
/// answer, keeps the report disconnected and tells the operator.
#[tokio::test]
async fn a_failed_reopen_answer_keeps_the_report_disconnected() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    tokio::task::yield_now().await;
    let (_, question) = disconnected_gated_report(&harness, &axi).await;
    supervisor.abort();
    std::fs::write(axi.dir.path().join("open.2.exit"), "3").unwrap();
    axi.reply("open.2", "error: no browser\n");

    answer(&harness, &question, "reopen").await;
    assert_eq!(
        answers_to(&harness, &question).await,
        vec!["reopen".to_owned()]
    );
    assert_eq!(
        report_state(&harness).await.as_deref(),
        Some("disconnected")
    );
    let notes = harness.system_notes().await;
    assert!(
        notes.len() == 1
            && notes[0].contains("no browser")
            && notes[0].contains("archon reports reopen"),
        "{notes:?}"
    );
}

/// A failing poll is contained to its report. It is restarted after a
/// backoff, its feedback then reaches the held news, and the supervisor
/// keeps running throughout.
#[tokio::test]
async fn a_failing_report_poll_is_retried_and_never_stops_the_supervisor() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    // The exit code must be in place before the reply releases the poll.
    std::fs::write(axi.dir.path().join("poll.1.exit"), "3").unwrap();
    axi.reply("poll.1", "lavish-axi crashed\n");
    axi.reply(
        "poll.2",
        "session:\n  status: feedback\nprompts[1]{uid,prompt,selector,tag,text}:\n  u1,\"Split it\",table,table,\n",
    );
    let TriggerEvent::LavishFeedback { prompts, .. } = harness.next_trigger().await else {
        panic!("expected the restarted poll's feedback");
    };
    assert_eq!(prompts, vec!["Split it"]);
    assert!(
        !supervisor.is_finished(),
        "one failing poll stopped them all"
    );

    axi.reply("poll.3", "session:\n  status: ended\n  ended_by: user\n");
    harness
        .eventually("the report ends", || async {
            report_state(&harness).await.as_deref() == Some("ended")
        })
        .await;
    assert!(!supervisor.is_finished());
    supervisor.abort();
    let polls = axi.calls().iter().filter(|c| c.starts_with("poll")).count();
    assert_eq!(
        polls, 3,
        "restarted once, and an ended report is left alone"
    );
}

/// The orchestrator gets the operator's prompts and the poll
/// output without the page's DOM snapshot, which the report file keeps.
#[tokio::test]
async fn feedback_carries_the_prompts_and_the_poll_without_its_snapshot() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    let snapshot = "<div>é</div>".repeat(1000);
    axi.reply(
        "poll.1",
        &format!(
            "session:\n  status: feedback\nprompts[1]{{uid,prompt,selector,tag,text}}:\n  u1,\"Split it\",table,table,\nnext_step: Apply\ndom_snapshot: \"{snapshot}\"\n"
        ),
    );
    let TriggerEvent::LavishFeedback { text, prompts, .. } = harness.next_trigger().await else {
        panic!("expected lavish feedback");
    };
    supervisor.abort();
    assert_eq!(prompts, ["Split it"]);
    let limit = archon_daemon::triggers::FEEDBACK_EXCERPT_BYTES;
    assert!(text.len() < limit, "{} bytes sent", text.len());
    assert!(text.starts_with("session:\n  status: feedback"), "{text}");
    assert!(text.contains("next_step: Apply\n"), "{text}");
    assert!(!text.contains("<div>"), "{text}");
    assert!(text.contains("[dom_snapshot left out: "), "{text}");
}

/// One prompt of `lavish-axi poll`'s expanded list form, with its target
/// and one image attachment.
fn listed_prompt(n: usize) -> String {
    format!(
        "  - uid: u{n}\n    prompt: \"Split table {n} by region, please\"\n    target:\n      selector: \"section#costs-{n} > table.breakdown\"\n      tag: table\n      text: \"Cost by stage and model for run {n}: plan, implement, review\"\n    attachments[1]:\n      - id: att_{n}\n        path: /home/op/.lavish/attachments/k/att_{n}.png\n        mime: image/png\n        width: 1280\n        height: 720\n"
    )
}

/// Several prompts with attachments outgrow the old 1 KB
/// bound, yet every row and path reaches the orchestrator; only lavish-axi's
/// `next_step`, which follows them, is cut.
#[tokio::test]
async fn feedback_keeps_every_prompt_row_and_attachment_path() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    let rows: Vec<String> = (1..=6).map(listed_prompt).collect();
    let table = format!("prompts[{}]:\n{}", rows.len(), rows.concat());
    let next_step = "Apply the requested changes, then poll again. ".repeat(80);
    axi.reply(
        "poll.1",
        &format!(
            "session:\n  status: feedback\n{table}next_step: \"{next_step}\"\ndom_snapshot: \"<p>page</p>\"\n"
        ),
    );
    let TriggerEvent::LavishFeedback { text, prompts, .. } = harness.next_trigger().await else {
        panic!("expected lavish feedback");
    };
    supervisor.abort();

    assert!(table.len() > 1024, "{} bytes of prompts", table.len());
    assert_eq!(prompts.len(), rows.len(), "{prompts:?}");
    assert!(text.contains(&table), "{text}");
    for n in 1..=rows.len() {
        assert!(text.contains(&format!("section#costs-{n} > table.breakdown")));
        assert!(text.contains(&format!(
            "path: /home/op/.lavish/attachments/k/att_{n}.png\n"
        )));
    }
    assert!(text.contains("next_step: \"Apply the requested"), "{text}");
    assert!(
        text.contains("more bytes of poll output left out"),
        "{text}"
    );
    let limit = archon_daemon::triggers::FEEDBACK_EXCERPT_BYTES;
    assert!(text.len() < limit + 64, "{} bytes sent", text.len());
}

/// With no prompt to parse, as for an artifact failure, the
/// detail reaches the orchestrator whole.
#[tokio::test]
async fn an_artifact_failure_keeps_its_detail_but_not_the_snapshot() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    let detail = "script error at line 7; ".repeat(100);
    let snapshot = "<div>é</div>".repeat(1000);
    axi.reply(
        "poll.1",
        &format!(
            "session:\n  status: feedback\nartifact_failures[1]:\n  - \"{detail}\"\nnext_step: Repair\ndom_snapshot: \"{snapshot}\"\n"
        ),
    );
    let TriggerEvent::LavishFeedback { text, prompts, .. } = harness.next_trigger().await else {
        panic!("expected lavish feedback");
    };
    supervisor.abort();
    assert!(prompts.is_empty());
    assert!(detail.len() > 1024, "longer than the old 1 KB bound");
    assert!(text.contains(&detail), "{text}");
    assert!(text.contains("next_step: Repair\n"), "{text}");
    assert!(!text.contains("<div>"), "{text}");
}

fn polls(axi: &FakeAxi) -> usize {
    axi.calls().iter().filter(|c| c.starts_with("poll")).count()
}

/// Once a report's item finishes, its poll process is stopped
/// and its Lavish session ended; the report file stays.
#[tokio::test]
async fn a_finished_items_report_session_is_ended_and_its_poll_stopped() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    axi.reply("end", "session:\n  status: ended\n");
    let mut harness = Harness::with(Config::default(), Some(lavish)).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    reports::publish(&harness.ctx, "Findings", &axi.html, Some(&item))
        .await
        .unwrap();
    harness
        .eventually("the report is polled", || async { polls(&axi) == 1 })
        .await;

    executor::cancel_item(&harness.ctx, &item, "done")
        .await
        .unwrap();
    harness
        .eventually("the session is ended", || async {
            report_state(&harness).await.as_deref() == Some("ended")
                && axi.calls().iter().any(|c| c.starts_with("end "))
        })
        .await;
    // Released now, a poll still running would return this feedback.
    axi.reply(
        "poll.1",
        "session:\n  status: feedback\nprompts[1]{uid,prompt}:\n  u1,late\n",
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!supervisor.is_finished());
    supervisor.abort();
    let end = format!("end {}", report_html(&harness).await.display());
    assert_eq!(axi.calls().iter().filter(|c| **c == end).count(), 1);
    assert_eq!(polls(&axi), 1, "the stopped poll is not restarted");
    let feedback = harness
        .drain_triggers()
        .await
        .into_iter()
        .filter(|t| matches!(t, TriggerEvent::LavishFeedback { .. }));
    assert_eq!(feedback.count(), 0, "the stopped poll delivers nothing");
    assert!(axi.html.is_file(), "the report file is kept");
    assert!(harness.system_notes().await.is_empty());
}

/// A session left open by an item that finished while no
/// supervisor ran is ended when the supervisor starts, not polled.
#[tokio::test]
async fn a_restart_ends_the_sessions_of_items_that_finished_meanwhile() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    axi.reply("end", "session:\n  status: ended\n");
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, Some(&item))
        .await
        .unwrap();
    executor::cancel_item(&harness.ctx, &item, "done")
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    harness
        .eventually("the session is ended", || async {
            report_state(&harness).await.as_deref() == Some("ended")
                && axi.calls().iter().any(|c| c.starts_with("end "))
        })
        .await;
    supervisor.abort();
    assert_eq!(polls(&axi), 0, "{:?}", axi.calls());
    assert!(harness.system_notes().await.is_empty());
}

/// A daemon stopped while `lavish-axi end` runs has not
/// recorded the report ended yet, so the next start ends its session.
#[tokio::test]
async fn a_session_whose_end_was_interrupted_is_ended_at_the_next_start() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    axi.reply("end", "session:\n  status: ended\n");
    axi.reply("end.1.hold", "");
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, Some(&item))
        .await
        .unwrap();
    executor::cancel_item(&harness.ctx, &item, "done")
        .await
        .unwrap();
    let first = tokio::spawn(reports::supervise(harness.ctx.clone()));
    harness
        .eventually("the first end runs", || async {
            axi.calls().iter().any(|c| c.starts_with("end "))
        })
        .await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));

    let second = tokio::spawn(reports::supervise(harness.ctx.clone()));
    harness
        .eventually("the session is ended again", || async {
            report_state(&harness).await.as_deref() == Some("ended")
        })
        .await;
    second.abort();
    let end = format!("end {}", report_html(&harness).await.display());
    assert_eq!(axi.calls().iter().filter(|c| **c == end).count(), 2);
    assert_eq!(polls(&axi), 0, "{:?}", axi.calls());
    assert!(harness.system_notes().await.is_empty());
}

/// A report shown about an item that had already finished was
/// meant for review then, so it stays open and polled.
#[tokio::test]
async fn a_report_shown_after_its_item_finished_stays_open() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let finished = link_common::cancelled_item(&harness.ctx, harness.dir.path()).await;
    reports::publish(&harness.ctx, "Findings", &axi.html, Some(&finished))
        .await
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    harness
        .eventually("the report is polled", || async { polls(&axi) == 1 })
        .await;
    supervisor.abort();
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
    assert!(!axi.calls().iter().any(|c| c.starts_with("end ")));
}

/// A page an open gate of another item's run still shows is
/// not ended when the item it was published for finishes.
#[tokio::test]
async fn a_report_an_open_gate_waits_on_stays_open() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let published = reports::publish(&harness.ctx, "Plan", &axi.html, None)
        .await
        .unwrap();
    let gate = gate_on(&harness, &published.report).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    assert_ne!(item, gate.item);
    let again = reports::publish(&harness.ctx, "Plan", &axi.html, Some(&item))
        .await
        .unwrap();
    assert_eq!(again.report, published.report, "one report per page");
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    harness
        .eventually("the report is polled", || async { polls(&axi) == 1 })
        .await;
    executor::cancel_item(&harness.ctx, &item, "done")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    supervisor.abort();
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
    assert!(
        !axi.calls().iter().any(|c| c.starts_with("end ")),
        "{:?}",
        axi.calls()
    );
}

#[tokio::test]
async fn republishing_a_legacy_report_switches_its_active_poll_to_the_copy() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let report = ReportId::new();
    let raw = serde_json::json!({
        "html": axi.html, "title": "Legacy", "item": null, "run": null, "state": "open"
    });
    harness
        .ctx
        .store
        .lock()
        .await
        .kv_set(&format!("report:{report}"), &raw.to_string())
        .unwrap();
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    let old_poll = format!("poll {}", axi.html.display());
    harness
        .eventually("legacy path is polled", || async {
            axi.calls().contains(&old_poll)
        })
        .await;
    let published = reports::publish(&harness.ctx, "Migrated", &axi.html, None)
        .await
        .unwrap();
    assert_eq!(published.report, report);
    let html = report_html(&harness).await;
    assert!(html.starts_with(&harness.ctx.paths.reports));
    let new_poll = format!("poll {}", html.display());
    harness
        .eventually("durable path is polled", || async {
            axi.calls().contains(&new_poll)
        })
        .await;
    supervisor.abort();
    assert!(supervisor.await.unwrap_err().is_cancelled());
    assert_eq!(polls(&axi), 2, "the source poll was replaced once");
}

#[tokio::test]
async fn pruning_waits_for_reopen_and_then_keeps_the_open_report_copy() {
    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let published = reports::publish(&harness.ctx, "Findings", &axi.html, None)
        .await
        .unwrap();
    let key = format!("report:{}", published.report);
    {
        let mut store = harness.ctx.store.lock().await;
        let mut raw: Value = serde_json::from_str(&store.kv_get(&key).unwrap().unwrap()).unwrap();
        raw["state"] = "unpublished".into();
        raw["updated_at"] =
            serde_json::to_value(archon_core::now() - chrono::Duration::days(8)).unwrap();
        store.kv_set(&key, &raw.to_string()).unwrap();
    }
    // Hold the review service after it starts opening the copy.
    let held = axi.dir.path().join("open.2.hold");
    std::fs::write(&held, "hold").unwrap();
    let ctx = harness.ctx.clone();
    let id = published.report.clone();
    let reopen = tokio::spawn(async move { reports::reopen(&ctx, &id).await });
    harness
        .eventually("reopen is held in Lavish", || async {
            axi.calls().iter().any(|call| call.ends_with(" --reopen"))
        })
        .await;
    assert!(
        harness.ctx.report_files.try_lock().is_err(),
        "reopen owns the snapshot"
    );
    let ctx = harness.ctx.clone();
    let pruning =
        tokio::spawn(
            async move { archon_daemon::prune::prune_once(&ctx, archon_core::now()).await },
        );
    std::fs::remove_file(held).unwrap();
    assert!(reopen.await.unwrap().unwrap().published);
    assert_eq!(pruning.await.unwrap().unwrap().reports, 0);
    assert!(report_html(&harness).await.exists());
    assert_eq!(report_state(&harness).await.as_deref(), Some("open"));
}

#[tokio::test]
async fn a_terminal_item_blocker_ends_its_report_session() {
    use archon_core::notify::NotificationKind;
    use archon_core::queue::QueueState;
    use archon_daemon::feed;

    let (axi, lavish) = FakeAxi::new();
    axi.opens_at(URL);
    axi.reply("end", "session:\n  status: ended\n");
    let harness = Harness::with(Config::default(), Some(lavish)).await;
    let item = link_common::queued_item(&harness.ctx, harness.dir.path()).await;
    let supervisor = tokio::spawn(reports::supervise(harness.ctx.clone()));
    reports::publish(&harness.ctx, "Findings", &axi.html, Some(&item))
        .await
        .unwrap();
    harness
        .eventually("the report is polled", || async { polls(&axi) == 1 })
        .await;
    harness
        .ctx
        .store
        .lock()
        .await
        .transition_item(&item, QueueState::Dispatching, archon_core::now())
        .unwrap();
    harness
        .ctx
        .store
        .lock()
        .await
        .transition_item(
            &item,
            QueueState::Failed {
                run: None,
                error: "startup refused".into(),
            },
            archon_core::now(),
        )
        .unwrap();
    feed::notify(
        &harness.ctx,
        NotificationKind::Blocker,
        "Failed: startup refused",
        Some(item),
        None,
    )
    .await
    .unwrap();
    harness
        .eventually("the blocker ends the session", || async {
            report_state(&harness).await.as_deref() == Some("ended")
                && axi.calls().iter().any(|c| c.starts_with("end "))
        })
        .await;
    supervisor.abort();
    assert_eq!(
        axi.calls().iter().filter(|c| c.starts_with("end ")).count(),
        1
    );
    assert!(axi.html.is_file());
}
