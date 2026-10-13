//! The initiative, steering and lesson requests: who may make them, and
//! what each answers until archond carries it out.

mod common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::{InitiativeId, LessonId, QueueItemId};
use archon_core::initiative::{EnqueueInitiative, InitiativeMember};
use archon_core::lesson::{LessonDraft, LessonKind, LessonPatch, LessonQuery};
use archon_core::protocol::{ClientKind, ErrorCode, ErrorReply, Reply, RepoRef, Request};
use archon_core::steer::Steer;
use archon_daemon::handlers;
use common::{Harness, REPO_NAME};

/// An agent Archon's records do not know as the orchestrator.
const HELPER: &str = "agt_helper";

#[track_caller]
fn assert_not_implemented(answer: Result<Reply, ArchonError>, command: &str) {
    let Err(error) = answer else {
        panic!("{command} is carried out now; drop it from this test: {answer:?}");
    };
    assert_eq!(ErrorReply::from(&error).code, ErrorCode::Unavailable);
    let expected = format!("unavailable: {command} is not implemented yet");
    assert_eq!(error.to_string(), expected);
}

async fn as_helper(h: &Harness, request: Request) -> Result<Reply, ArchonError> {
    let forwarded = Request::ForAgent {
        caller: AetherAgentId::parse(HELPER).unwrap(),
        request: Box::new(request),
    };
    handlers::handle(&h.ctx, ClientKind::ArchonMcp, forwarded).await
}

#[track_caller]
fn assert_orchestrators_alone(answer: Result<Reply, ArchonError>) {
    match answer {
        Err(ArchonError::Forbidden(why)) => assert!(why.contains("only the orchestrator")),
        other => panic!("a helper's change is refused: {other:?}"),
    }
}

fn enqueue_initiative() -> Request {
    Request::EnqueueInitiative(EnqueueInitiative {
        title: "Rename the account field".into(),
        contract: "The field is `account_id`.".into(),
        atomic: false,
        members: vec![InitiativeMember {
            repo: RepoRef(REPO_NAME.into()),
            brief: "Rename it.".into(),
            title: None,
            pipeline: None,
            model_plan: None,
            after: Vec::new(),
        }],
        idempotency_key: None,
    })
}

#[tokio::test]
async fn initiatives_are_the_orchestrators_to_change_and_not_built_yet() {
    let h = Harness::new().await;
    let initiative = InitiativeId::new();
    let update = Request::UpdateInitiative {
        initiative: initiative.clone(),
        contract_append: "Also rename the column.".into(),
    };
    assert_orchestrators_alone(as_helper(&h, enqueue_initiative()).await);
    assert_orchestrators_alone(as_helper(&h, update.clone()).await);
    assert_not_implemented(
        h.orchestrator(enqueue_initiative()).await,
        "enqueue_initiative",
    );
    assert_not_implemented(h.orchestrator(update).await, "update_initiative");
    let get = Request::GetInitiative { initiative };
    assert_not_implemented(as_helper(&h, get).await, "get_initiative");
}

#[tokio::test]
async fn steering_is_the_orchestrators_and_not_built_yet() {
    let h = Harness::new().await;
    let steer = Request::SteerItem {
        item: QueueItemId::new(),
        steer: Steer {
            note: Some("Use the v2 client.".into()),
            ..Steer::default()
        },
    };
    assert_orchestrators_alone(as_helper(&h, steer.clone()).await);
    assert_not_implemented(h.orchestrator(steer.clone()).await, "steer_item");
    assert_not_implemented(h.cli(steer).await, "steer_item");
}

#[tokio::test]
async fn lessons_are_read_by_anyone_edited_by_the_operator_and_not_built_yet() {
    let h = Harness::new().await;
    let record = Request::RecordLesson(LessonDraft {
        kind: LessonKind::Pitfall,
        text: "Run `make lint` before shipping.".into(),
        repo: None,
        pipeline: None,
        item: None,
    });
    assert_orchestrators_alone(as_helper(&h, record.clone()).await);
    assert_not_implemented(h.orchestrator(record.clone()).await, "record_lesson");
    assert_not_implemented(h.cli(record).await, "record_lesson");

    let edit = Request::UpdateLesson {
        lesson: LessonId::new(),
        patch: LessonPatch {
            pinned: Some(true),
            ..LessonPatch::default()
        },
    };
    let refused = h
        .orchestrator(edit.clone())
        .await
        .expect_err("operator only");
    assert!(matches!(refused, ArchonError::Forbidden(_)), "{refused:?}");
    assert_not_implemented(h.cli(edit).await, "update_lesson");

    let item = QueueItemId::new();
    let lookups = [
        (Request::Lessons(LessonQuery::default()), "lessons"),
        (Request::LessonsWhy { item }, "lessons_why"),
        (Request::LessonStats { repo: None }, "lesson_stats"),
    ];
    for (request, command) in lookups {
        assert_not_implemented(as_helper(&h, request).await, command);
    }
}
