//! Aether's notification center: questions, blockers and outcomes are
//! posted to a fake aetherd that keeps notifications, answers given there
//! take the `/answer` path, and an aetherd without the center is left alone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use archon_aether::AetherClient;
use archon_aether::fake::Scripted;
use archon_core::config::{AetherMode, Config};
use archon_core::id::{PipelineRunId, QuestionId};
use archon_core::notify::{AnswerVia, FeedEntry, NotificationKind, Question};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{GateSubject, OperatorGate, RunState};
use archon_daemon::{feed, handlers, notification_center};
use archon_queue::store::keys;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::link_common::{Harness, running_stage};

/// The records a fake notification center holds, oldest first.
#[derive(Clone, Default)]
struct Center(Arc<Mutex<Vec<Value>>>);

impl Center {
    /// Serve `post`, `list` and `resolve` from these records, as Aether does.
    fn install(&self, h: &Harness) {
        h.fake.set_features(vec!["notifications".into()]);
        for cmd in [
            "post_notification",
            "list_notifications",
            "resolve_notification",
        ] {
            let center = self.clone();
            h.fake
                .set_responder(cmd, Arc::new(move |frame| center.respond(cmd, frame)));
        }
    }

    /// Fail the next `cmd` inside Aether, then serve it again.
    fn fail_next(&self, h: &Harness, cmd: &'static str) {
        let center = self.clone();
        let failed = Arc::new(AtomicBool::new(false));
        h.fake.set_responder(
            cmd,
            Arc::new(move |frame| match failed.swap(true, Ordering::SeqCst) {
                false => Scripted::refuse("internal", "the store is busy"),
                true => center.respond(cmd, frame),
            }),
        );
    }

    fn respond(&self, cmd: &str, frame: &Value) -> Scripted {
        match cmd {
            "post_notification" => self.post(&frame["source"], &frame["notification"]),
            "list_notifications" => {
                let newest_first: Vec<Value> = self.all().into_iter().rev().collect();
                Scripted::reply("notifications", Value::Array(newest_first))
            }
            "resolve_notification" => self.resolve(frame["key"].as_str().unwrap()),
            other => panic!("the center serves no {other}"),
        }
    }

    fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }

    fn by_key(&self, key: &str) -> Option<Value> {
        self.all().into_iter().find(|n| n["key"] == key)
    }

    fn post(&self, source: &Value, post: &Value) -> Scripted {
        assert_eq!(source, "archon", "Archon posts as itself");
        let mut records = self.0.lock().unwrap();
        let mut record = post.clone();
        record["source"] = source.clone();
        let existing = records.iter_mut().find(|n| n["key"] == post["key"]);
        let Some(existing) = existing else {
            record["id"] = json!(format!("ntf_{}", records.len() + 1));
            record["state"] = json!("unread");
            records.push(record.clone());
            return Scripted::reply("notification", record);
        };
        record["id"] = existing["id"].clone();
        record["state"] = existing["state"].clone();
        *existing = record.clone();
        Scripted::reply("notification", record)
    }

    fn resolve(&self, key: &str) -> Scripted {
        let mut records = self.0.lock().unwrap();
        let Some(record) = records.iter_mut().find(|n| n["key"] == key) else {
            return Scripted::refuse("not_found", "no such notification");
        };
        record["state"] = json!("resolved");
        Scripted::reply("notification", record.clone())
    }

    /// The operator's answer, recorded as Aether records it: resolved with
    /// the answer. Returns the record for a `notification.answered` event.
    fn answer(&self, key: &str, action: Option<&str>, text: Option<&str>, by: Value) -> Value {
        let mut records = self.0.lock().unwrap();
        let record = records
            .iter_mut()
            .find(|n| n["key"] == key)
            .expect("posted");
        record["state"] = json!("resolved");
        record["answer"] = json!({"action": action, "text": text, "by": by,
                                  "at": "2026-10-19T00:00:00Z"});
        record.clone()
    }
}

fn operator() -> Value {
    json!({"actor": "user"})
}

/// The `notification.answered` event Aether records for `record`.
fn answered_event(record: Value) -> Value {
    json!({"kind": {"type": "custom", "name": "notification.answered", "payload": record}})
}

/// The notification center bridge, running until the test ends.
struct Bridge(JoinHandle<()>);

impl Drop for Bridge {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Started once its feed cursor is set: news before that is not posted.
async fn bridge(h: &Harness, client: Arc<AetherClient>) -> Bridge {
    let ctx = h.ctx.clone();
    let bridge = Bridge(tokio::spawn(async move {
        notification_center::follow(&ctx, &client)
            .await
            .expect("the bridge never fails the session");
    }));
    h.eventually("the bridge follows the feed", || async {
        h.kv(keys::CURSOR_NOTIFICATION_CENTER).await.is_some()
    })
    .await;
    bridge
}

async fn ask(h: &Harness, text: &str, options: &[&str]) -> Question {
    let options = options.iter().map(|o| (*o).to_owned()).collect();
    feed::question(&h.ctx, text.into(), options, None)
        .await
        .expect("question recorded")
}

/// `archon answer`, as the CLI sends it.
async fn archon_answer(h: &Harness, id: &QuestionId, text: &str) {
    let request = Request::Answer {
        question: id.clone(),
        text: text.into(),
        chat: false,
    };
    let reply = handlers::handle(&h.ctx, ClientKind::ArchonCli, request).await;
    assert!(matches!(reply, Ok(Reply::Question(_))), "{reply:?}");
}

async fn question(h: &Harness, id: &QuestionId) -> Question {
    h.ctx.store.lock().await.question(id).expect("question")
}

async fn posted(h: &Harness, center: &Center, key: &str) -> Value {
    h.eventually("the notification is posted", || async {
        center.by_key(key).is_some()
    })
    .await;
    center.by_key(key).unwrap()
}

#[tokio::test]
async fn a_question_is_posted_with_its_options_and_resolved_when_answered_elsewhere() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    let asked = ask(&h, "Ship 1.5 now?\nCI is green.", &["yes", "wait"]).await;

    let record = posted(&h, &center, asked.id.as_str()).await;
    assert_eq!(record["kind"], "question");
    assert_eq!(record["title"], format!("Q{}: Ship 1.5 now?", asked.number));
    assert_eq!(
        record["actions"],
        json!([{"id": "1", "label": "yes"}, {"id": "2", "label": "wait"}])
    );
    assert_eq!(record["free_text"], true);
    let body = record["body"].as_str().unwrap();
    assert!(body.contains("Archon asks."), "{body}");
    assert!(
        body.contains(&format!("/answer {}", asked.number)),
        "{body}"
    );

    archon_answer(&h, &asked.id, "wait").await;
    h.eventually("the notification is resolved", || async {
        center.by_key(asked.id.as_str()).unwrap()["state"] == "resolved"
    })
    .await;
}

#[tokio::test]
async fn an_answer_in_the_notification_center_takes_the_answer_path() {
    let mut h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    let asked = ask(&h, "Which release?", &["1.5", "1.6"]).await;
    posted(&h, &center, asked.id.as_str()).await;

    let record = center.answer(
        asked.id.as_str(),
        Some("2"),
        Some("after the freeze"),
        operator(),
    );
    h.fake.push_event(answered_event(record));
    h.eventually("the question is answered", || async {
        !question(&h, &asked.id).await.is_open()
    })
    .await;
    let answered = question(&h, &asked.id).await;
    assert_eq!(answered.answer.as_deref(), Some("1.6 after the freeze"));
    let via = h.feed().await.into_iter().find_map(|entry| match entry {
        FeedEntry::Answer { question, via, .. } if question == asked.id => Some(via),
        _ => None,
    });
    assert_eq!(via, Some(AnswerVia::NotificationCenter));
    let routed = h.drain_triggers().await;
    assert!(
        routed
            .iter()
            .any(|t| format!("{t:?}").contains("1.6 after the freeze")),
        "the asker hears the answer: {routed:?}"
    );
}

#[tokio::test]
async fn on_connect_answers_given_meanwhile_apply_and_closed_questions_resolve() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let answered_away = ask(&h, "Merge #12?", &["yes", "no"]).await;
    let closed_away = ask(&h, "Rebase first?", &[]).await;
    let never_posted = ask(&h, "Pin the toolchain?", &[]).await;
    for (q, title) in [(&answered_away, "Q1"), (&closed_away, "Q2")] {
        center.post(
            &json!("archon"),
            &json!({"kind": "question", "title": title, "key": q.id}),
        );
    }
    center.answer(answered_away.id.as_str(), Some("1"), None, operator());
    archon_answer(&h, &closed_away.id, "no").await;

    let _bridge = bridge(&h, h.connect().await).await;
    h.eventually("the answer given meanwhile is applied", || async {
        !question(&h, &answered_away.id).await.is_open()
    })
    .await;
    assert_eq!(
        question(&h, &answered_away.id).await.answer.as_deref(),
        Some("yes")
    );
    h.eventually("the question closed meanwhile is resolved", || async {
        center.by_key(closed_away.id.as_str()).unwrap()["state"] == "resolved"
    })
    .await;
    let record = posted(&h, &center, never_posted.id.as_str()).await;
    assert_eq!(record["state"], "unread");
}

#[tokio::test]
async fn a_reconcile_that_failed_inside_aether_is_retried() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let asked = ask(&h, "Merge #12?", &["yes", "no"]).await;
    center.post(
        &json!("archon"),
        &json!({"kind": "question", "title": "Q1", "key": asked.id}),
    );
    center.answer(asked.id.as_str(), Some("1"), None, operator());
    center.fail_next(&h, "list_notifications");

    let _bridge = bridge(&h, h.connect().await).await;
    h.eventually("the failed list is sent", || async {
        h.fake
            .received()
            .iter()
            .any(|c| c["cmd"] == "list_notifications")
    })
    .await;
    assert!(
        question(&h, &asked.id).await.is_open(),
        "nothing applied yet"
    );
    feed::notify(&h.ctx, NotificationKind::Progress, "stage 2", None, None)
        .await
        .unwrap();
    h.eventually("the answer given meanwhile is applied", || async {
        !question(&h, &asked.id).await.is_open()
    })
    .await;
    assert_eq!(question(&h, &asked.id).await.answer.as_deref(), Some("yes"));
}

#[tokio::test]
async fn a_notice_aether_failed_to_take_is_posted_on_the_next_pass() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    center.fail_next(&h, "post_notification");
    feed::notify(&h.ctx, NotificationKind::Blocker, "stuck", None, None)
        .await
        .unwrap();
    h.eventually("the failed post is sent", || async {
        h.fake
            .received()
            .iter()
            .any(|c| c["cmd"] == "post_notification")
    })
    .await;
    assert!(center.all().is_empty(), "the first post failed");
    feed::notify(&h.ctx, NotificationKind::Outcome, "merged", None, None)
        .await
        .unwrap();
    h.eventually("both notices are posted", || async {
        center.all().len() == 2
    })
    .await;
    let titles: Vec<Value> = center.all().iter().map(|n| n["title"].clone()).collect();
    assert_eq!(titles, [json!("stuck"), json!("merged")], "in feed order");
}

#[tokio::test]
async fn blockers_and_outcomes_are_posted_without_actions() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    let blocker = feed::notify(
        &h.ctx,
        NotificationKind::Blocker,
        "qi_1 needs a token",
        None,
        None,
    )
    .await
    .unwrap();
    let outcome = feed::notify(
        &h.ctx,
        NotificationKind::Outcome,
        "PR #7 merged",
        None,
        None,
    )
    .await
    .unwrap();
    feed::notify(
        &h.ctx,
        NotificationKind::Progress,
        "stage 2 of 3",
        None,
        None,
    )
    .await
    .unwrap();

    let key = |item: &archon_core::notify::FeedItem| match &item.entry {
        FeedEntry::Notification(n) => n.id.to_string(),
        other => panic!("a notification, not {other:?}"),
    };
    let blocked = posted(&h, &center, &key(&blocker)).await;
    assert_eq!(blocked["kind"], "blocker");
    assert_eq!(blocked["title"], "qi_1 needs a token");
    assert!(blocked.get("actions").is_none(), "{blocked}");
    let finished = posted(&h, &center, &key(&outcome)).await;
    assert_eq!(finished["kind"], "outcome");
    assert!(finished.get("actions").is_none(), "{finished}");
    assert_eq!(center.all().len(), 2, "progress stays out of the center");
}

#[tokio::test]
async fn in_shared_mode_questions_are_shown_but_not_answered_there() {
    let mut config = Config::default();
    config.aether.mode = AetherMode::Shared;
    let h = Harness::with(config, None).await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    let asked = ask(&h, "Ship it?", &["yes", "no"]).await;

    let record = posted(&h, &center, asked.id.as_str()).await;
    assert!(record.get("actions").is_none(), "{record}");
    assert!(record.get("free_text").is_none(), "{record}");
    let record = center.answer(asked.id.as_str(), Some("1"), None, operator());
    h.fake.push_event(answered_event(record));
    h.eventually("the refusal is told", || async {
        h.system_notes()
            .await
            .iter()
            .any(|n| n.contains("was not applied"))
    })
    .await;
    assert!(question(&h, &asked.id).await.is_open());
}

#[tokio::test]
async fn an_answer_no_person_gave_is_refused() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    let asked = ask(&h, "Ship it?", &["yes", "no"]).await;
    posted(&h, &center, asked.id.as_str()).await;

    let agent = json!({"actor": "agent", "id": "agt_01M5AEC0BDS8JVA3Z9E30R1Q1V"});
    let record = center.answer(asked.id.as_str(), Some("1"), None, agent);
    h.fake.push_event(answered_event(record));
    h.eventually("the refusal is told", || async {
        h.system_notes()
            .await
            .iter()
            .any(|n| n.contains("only a person"))
    })
    .await;
    assert!(question(&h, &asked.id).await.is_open());
}

#[tokio::test]
async fn an_aetherd_without_the_center_is_left_alone() {
    let h = Harness::new().await;
    let client = h.connect().await;
    ask(&h, "Ship it?", &["yes"]).await;
    feed::notify(&h.ctx, NotificationKind::Blocker, "stuck", None, None)
        .await
        .unwrap();
    tokio::time::timeout(
        crate::link_common::WAIT,
        notification_center::follow(&h.ctx, &client),
    )
    .await
    .expect("returns at once")
    .expect("never fails");
    let sent: Vec<Value> = h.fake.received();
    assert!(
        sent.iter()
            .all(|cmd| !cmd["cmd"].as_str().unwrap().contains("notification")),
        "{sent:?}"
    );
}

/// A proposal gate of a running item, asked as `gates::announce` asks it.
async fn open_gate(h: &Harness) -> (Question, PipelineRunId) {
    let stage = running_stage(&h.ctx, h.dir.path(), "agt_1", "tsk_1").await;
    let mut store = h.ctx.store.lock().await;
    let run = store.run(&stage.run).unwrap();
    let asked = Question {
        id: QuestionId::new(),
        number: 0,
        text: "Approve the proposal of link test item?".into(),
        options: vec!["yes".into(), "reject".into()],
        item: Some(run.item.clone()),
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let asked = store.insert_question(&asked).unwrap();
    let gate = OperatorGate {
        run: run.id.clone(),
        item: run.item.clone(),
        stage_id: "implement".into(),
        attempt: 1,
        subject: GateSubject::Proposal,
        summary: "Approach: a flag.".into(),
        page: None,
        location: None,
        report: None,
        question: None,
        decision: None,
        withdrawn: None,
        opened_at: archon_core::now(),
        decided_at: None,
    };
    let mut gate = store.open_gate(&gate).unwrap();
    gate.question = Some(asked.id.clone());
    store.update_gate(&gate).unwrap();
    let item = store
        .append_feed(FeedEntry::Question(asked.clone()), archon_core::now())
        .unwrap();
    feed::publish(&h.ctx, vec![item]);
    (asked, run.id)
}

#[tokio::test]
async fn a_gate_is_posted_as_a_question_and_resolved_when_its_run_ends() {
    let h = Harness::new().await;
    let center = Center::default();
    center.install(&h);
    let _bridge = bridge(&h, h.connect().await).await;
    let (asked, run) = open_gate(&h).await;

    let record = posted(&h, &center, asked.id.as_str()).await;
    let body = record["body"].as_str().unwrap();
    assert!(
        body.contains("Gate: the direction waits for your decision."),
        "{body}"
    );
    assert!(body.contains("link test item"), "the item is named: {body}");
    assert_eq!(
        record["actions"],
        json!([{"id": "1", "label": "yes"}, {"id": "2", "label": "reject"}])
    );

    let finished = {
        let mut store = h.ctx.store.lock().await;
        let mut run = store.run(&run).unwrap();
        let error = "cancelled by the operator".to_owned();
        let failed = RunState::Failed {
            stage_id: "implement".into(),
            error: error.clone(),
        };
        run.finish(failed, archon_core::now()).unwrap();
        let state = QueueState::Failed {
            run: Some(run.id.clone()),
            error,
        };
        store.finish_run(&run, state, archon_core::now()).unwrap()
    };
    assert_eq!(
        finished.answers.len(),
        1,
        "the gate's question is withdrawn"
    );
    feed::publish(&h.ctx, finished.answers);
    h.eventually("the withdrawn gate's notification is resolved", || async {
        center.by_key(asked.id.as_str()).unwrap()["state"] == "resolved"
    })
    .await;
}
