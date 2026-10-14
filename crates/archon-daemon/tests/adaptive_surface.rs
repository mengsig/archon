//! The initiative, steering and lesson requests: who may make them.

mod common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::initiative::{EnqueueInitiative, InitiativeMember};
use archon_core::lesson::{LessonDraft, LessonKind, LessonPatch, LessonQuery};
use archon_core::protocol::{ClientKind, Reply, RepoRef, Request};
use archon_core::steer::Steer;
use archon_daemon::handlers;
use common::{Harness, REPO_NAME};

/// An agent Archon's records do not know as the orchestrator.
const HELPER: &str = "agt_helper";

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
            autonomy: Default::default(),
            after: Vec::new(),
        }],
        idempotency_key: None,
    })
}

#[tokio::test]
async fn initiatives_are_the_orchestrators_to_change_and_anyones_to_read() {
    let h = Harness::new().await;
    assert_orchestrators_alone(as_helper(&h, enqueue_initiative()).await);
    let Reply::Initiative(created) = h.orchestrator(enqueue_initiative()).await.unwrap() else {
        panic!("initiative reply");
    };
    let initiative = created.initiative.id;
    let update = Request::UpdateInitiative {
        initiative: initiative.clone(),
        contract_append: "Also rename the column.".into(),
    };
    assert_orchestrators_alone(as_helper(&h, update.clone()).await);
    let Reply::Initiative(updated) = h.orchestrator(update).await.unwrap() else {
        panic!("initiative reply");
    };
    assert!(
        updated
            .initiative
            .contract
            .ends_with("Also rename the column.")
    );
    assert_eq!(updated.members.len(), 1);
    let get = Request::GetInitiative {
        initiative: initiative.clone(),
    };
    assert_eq!(
        as_helper(&h, get).await.unwrap(),
        Reply::Initiative(updated)
    );
    assert_orchestrators_alone(
        as_helper(
            &h,
            Request::CancelInitiative {
                initiative: initiative.clone(),
            },
        )
        .await,
    );
    assert_orchestrators_alone(as_helper(&h, Request::RetryInitiative { initiative }).await);
}

#[tokio::test]
async fn steering_is_the_orchestrators_and_the_operators() {
    let h = Harness::new().await;
    let steer = Request::SteerItem {
        item: QueueItemId::new(),
        steer: Steer {
            note: Some("Use the v2 client.".into()),
            ..Steer::default()
        },
    };
    assert_orchestrators_alone(as_helper(&h, steer.clone()).await);
    for answer in [h.orchestrator(steer.clone()).await, h.cli(steer).await] {
        assert!(
            matches!(answer, Err(ArchonError::NotFound(_))),
            "carried out, so the item is looked up: {answer:?}"
        );
    }
}

#[tokio::test]
async fn lessons_are_read_by_anyone_and_edited_by_the_operator() {
    let h = Harness::new().await;
    let item = h.enqueue("lesson source").await;
    let record = Request::RecordLesson(LessonDraft {
        kind: LessonKind::Pitfall,
        text: "Run make lint before shipping.".into(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: None,
        item: Some(item.id),
    });
    assert_orchestrators_alone(as_helper(&h, record.clone()).await);
    let Reply::Lesson(lesson) = h.orchestrator(record.clone()).await.unwrap() else {
        panic!("lesson");
    };
    let Reply::Lesson(operator) = h.cli(record).await.unwrap() else {
        panic!("lesson");
    };
    assert_eq!(lesson.id, operator.id);
    let edit = Request::UpdateLesson {
        lesson: lesson.id,
        patch: LessonPatch {
            pinned: Some(true),
            ..Default::default()
        },
    };
    assert!(matches!(
        h.orchestrator(edit.clone()).await,
        Err(ArchonError::Forbidden(_))
    ));
    assert!(matches!(h.cli(edit).await.unwrap(),Reply::Lesson(l) if l.pinned));
    assert!(
        matches!(as_helper(&h,Request::Lessons(LessonQuery::default())).await.unwrap(),Reply::Lessons(l) if l.len()==1)
    );
    assert!(matches!(
        as_helper(&h, Request::LessonStats { repo: None })
            .await
            .unwrap(),
        Reply::LessonStats(_)
    ));
}
