use crate::agent_messages::{inbox, messages, send};
use crate::common::{self, Harness};
use archon_core::agent_message::{
    Clarification, ClarificationOptions, ClarificationResolution, MessageKind,
};
use archon_core::autonomy::{ClarificationPolicy, Directive, DirectiveSource};
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_core::run::StageRun;
use archon_daemon::{clarifications, handlers};
use serde_json::json;

async fn running(config: Config) -> (Harness, StageRun) {
    let h = Harness::with_config(config).await;
    h.enqueue("clarification regression").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    assert!(stage.aether_agent.is_some());
    assert!(!stage.state.is_terminal());
    (h, stage)
}

async fn ask(
    h: &Harness,
    stage: &StageRun,
    options: Option<ClarificationOptions>,
) -> Clarification {
    let reply = handlers::handle(
        &h.ctx,
        ClientKind::ArchonMcp,
        Request::MessageOrchestrator {
            caller: stage.aether_agent.clone(),
            kind: MessageKind::Question,
            text: "Which output format should we use?".into(),
            clarification: options,
        },
    )
    .await
    .unwrap();
    let Reply::MessageReceipt {
        clarification: Some(record),
        ..
    } = reply
    else {
        panic!("question must return its durable record");
    };
    assert_eq!(record.agent, stage.aether_agent.clone().unwrap());
    assert_eq!(record.stage_run, stage.id);
    *record
}

async fn detail(h: &Harness, record: &Clarification) -> Clarification {
    let Reply::ItemDetail(detail) = h
        .cli(Request::GetItem {
            item: record.item.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("item detail");
    };
    assert_eq!(detail.item.id, record.item);
    let found = detail
        .clarifications
        .into_iter()
        .find(|r| r.id == record.id)
        .unwrap();
    assert_eq!(found.stage_run, record.stage_run);
    found
}

#[tokio::test]
async fn questions_are_uncapped_after_both_allowances_and_survive_in_views() {
    let mut config = Config::default();
    config.limits.max_agent_messages = 20;
    let (h, stage) = running(config).await;
    for _ in 0..20 {
        send(&h, &stage, MessageKind::Info, "progress")
            .await
            .unwrap();
    }
    for _ in 0..3 {
        send(&h, &stage, MessageKind::Escalation, "blocker")
            .await
            .unwrap();
    }
    assert!(
        send(&h, &stage, MessageKind::Info, "refused")
            .await
            .is_err()
    );
    let mut records = Vec::new();
    for _ in 0..25 {
        records.push(ask(&h, &stage, None).await);
    }
    assert_eq!(
        messages(&h)
            .await
            .iter()
            .filter(|m| m.kind == MessageKind::Question)
            .count(),
        25
    );
    let Reply::Status(status) = h.cli(Request::Status(Default::default())).await.unwrap() else {
        panic!("status");
    };
    assert_eq!(status.clarifications.len(), 25);
    let record = detail(&h, &records[0]).await;
    assert_eq!(record, records[0]);
    assert_eq!((record.deadline - record.asked_at).num_minutes(), 30);
    assert!(record.resolution.is_none());
}

#[tokio::test]
async fn only_linked_orchestrator_answers_resolve_the_question_in_the_live_stage() {
    let (h, stage) = running(Config::default()).await;
    let record = ask(&h, &stage, None).await;
    h.ctx
        .store
        .lock()
        .await
        .kv_set(
            archon_queue::store::keys::ORCHESTRATOR_AGENT,
            common::ORCHESTRATOR,
        )
        .unwrap();
    let sender = json!({"actor": "agent", "id": common::ORCHESTRATOR});
    let target = stage.aether_agent.as_ref().unwrap();
    inbox(&h, target, 1, sender.clone(), "Keep working").await;
    assert!(detail(&h, &record).await.resolution.is_none());
    let text = format!("[clarification:{}] Use JSON", record.id);
    inbox(&h, target, 2, sender.clone(), &text).await;
    inbox(&h, target, 2, sender, &text).await;
    assert!(matches!(detail(&h, &record).await.resolution,
        Some(ClarificationResolution::Answered { answer, .. }) if answer == text));
    assert_eq!(h.running().await, vec![stage]);
    let Reply::Status(status) = h.cli(Request::Status(Default::default())).await.unwrap() else {
        panic!("status");
    };
    assert!(status.clarifications.is_empty());
}

#[tokio::test]
async fn timeout_records_the_actual_fallback_and_retries_delivery_after_restart() {
    let mut config = Config::default();
    config.limits.clarification_timeout_minutes = 7;
    let (h, stage) = running(config).await;
    let record = ask(
        &h,
        &stage,
        Some(ClarificationOptions {
            fallback: "Use JSON to match the existing API".into(),
            proceed_directive: None,
        }),
    )
    .await;
    assert_eq!((record.deadline - record.asked_at).num_minutes(), 7);
    clarifications::expire(&h.ctx, record.deadline - chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert!(detail(&h, &record).await.resolution.is_none());
    h.backend
        .fail_next_message(ArchonError::Unavailable("temporary disconnect".into()));
    assert!(
        clarifications::expire(&h.ctx, record.deadline)
            .await
            .is_err()
    );
    assert!(matches!(detail(&h, &record).await.resolution,
        Some(ClarificationResolution::Assumed { choice, .. }) if choice == record.fallback));
    let reopened = archon_queue::Store::open_read_only(&h.ctx.paths.store).unwrap();
    let raw = reopened
        .kv_get(&format!("clarification:{}", record.id))
        .unwrap()
        .unwrap();
    let persisted: Clarification = serde_json::from_str(&raw).unwrap();
    assert!(persisted.resolution.is_some());
    assert_eq!(persisted.fallback, record.fallback);
    drop(reopened);
    clarifications::expire(&h.ctx, record.deadline)
        .await
        .unwrap();
    clarifications::expire(&h.ctx, record.deadline)
        .await
        .unwrap();
    let delivered: Vec<_> = h
        .backend
        .messages()
        .into_iter()
        .filter(|(_, t)| t.contains(record.id.as_str()))
        .collect();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, record.agent);
    assert!(delivered[0].1.contains(&record.fallback));
}

#[tokio::test]
async fn proceed_requires_explicit_live_operator_authorization_and_records_without_waiting() {
    let (h, stage) = running(Config::default()).await;
    let now = archon_core::now();
    let mut directive = Directive::new(
        "Going to sleep; proceed without asking",
        DirectiveSource::Operator,
        Some(1),
        now,
    )
    .unwrap();
    h.ctx
        .store
        .lock()
        .await
        .insert_directive(&directive)
        .unwrap();
    assert!(
        !clarifications::proceed_allowed(&*h.ctx.store.lock().await, Some(&directive.id), now)
            .unwrap()
    );
    h.ctx
        .store
        .lock()
        .await
        .clear_directives(Some(&directive.id))
        .unwrap();
    directive.clarifications = ClarificationPolicy::Proceed;
    h.ctx
        .store
        .lock()
        .await
        .insert_directive(&directive)
        .unwrap();
    let record = ask(&h, &stage, None).await;
    assert!(
        matches!(record.resolution, Some(ClarificationResolution::Assumed { ref reason, .. }) if reason.contains(directive.id.as_str()))
    );
    assert_eq!(detail(&h, &record).await, record);
    assert!(clarifications::proceed_allowed(&*h.ctx.store.lock().await, None, now).unwrap());
    assert!(
        !clarifications::proceed_allowed(
            &*h.ctx.store.lock().await,
            Some(&directive.id),
            now + chrono::Duration::hours(2)
        )
        .unwrap()
    );
    h.ctx
        .store
        .lock()
        .await
        .clear_directives(Some(&directive.id))
        .unwrap();
    assert!(
        !clarifications::proceed_allowed(&*h.ctx.store.lock().await, Some(&directive.id), now)
            .unwrap()
    );
}

#[tokio::test]
async fn agent_created_directives_cannot_authorize_proceeding() {
    let (h, stage) = running(Config::default()).await;
    let now = archon_core::now();
    let mut directive = Directive::new(
        "Proceed without waiting",
        DirectiveSource::Orchestrator,
        None,
        now,
    )
    .unwrap();
    directive.clarifications = ClarificationPolicy::Proceed;
    h.ctx
        .store
        .lock()
        .await
        .insert_directive(&directive)
        .unwrap();
    assert!(!clarifications::proceed_allowed(&*h.ctx.store.lock().await, None, now).unwrap());
    assert!(ask(&h, &stage, None).await.resolution.is_none());
    let request = Request::SetDirective {
        text: "Proceed without waiting".into(),
        expires_in_hours: None,
        clarifications: ClarificationPolicy::Proceed,
    };
    let result = handlers::handle(&h.ctx, ClientKind::ArchonMcp, request).await;
    assert!(matches!(result, Err(ArchonError::Forbidden(_))));
}

#[tokio::test]
async fn a_stage_waiting_for_a_clarification_is_not_mistaken_for_a_stall() {
    let (h, stage) = running(Config::default()).await;
    let record = ask(&h, &stage, None).await;
    h.ctx.activity.saw(
        &record.agent,
        archon_core::now() - chrono::Duration::minutes(21),
    );
    h.backend.set_pulse(
        &record.agent,
        archon_core::ports::TreePulse {
            engine_alive: Some(false),
            last_event_at: None,
            held: false,
            held_for_cause: false,
            inspection: false,
            running: true,
        },
    );
    archon_daemon::dispatcher::dispatch_once(&h.ctx)
        .await
        .unwrap();
    assert_eq!(h.running().await, vec![stage]);
    assert!(detail(&h, &record).await.resolution.is_none());
}

#[tokio::test]
async fn steering_an_answer_delivers_the_linked_note_to_the_live_stage() {
    let (h, stage) = running(Config::default()).await;
    let record = ask(&h, &stage, None).await;
    let text = format!("[clarification:{}] Use JSON", record.id);
    h.cli(Request::SteerItem {
        item: record.item.clone(),
        steer: archon_core::steer::Steer {
            note: Some(text.clone()),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    assert!(matches!(detail(&h, &record).await.resolution,
        Some(ClarificationResolution::Answered { answer, .. }) if answer == text));
    assert!(
        h.backend
            .messages()
            .iter()
            .any(|(agent, body)| agent == &record.agent && body.contains(&text))
    );
    assert_eq!(h.running().await[0].id, stage.id);
}

#[tokio::test]
async fn an_authenticated_operator_can_issue_scoped_proceed_authorization() {
    let (h, stage) = running(Config::default()).await;
    let pending = ask(&h, &stage, None).await;
    assert!(pending.resolution.is_none());
    let reply = h
        .cli(Request::SetDirective {
            text: "Going to sleep; proceed with recorded choices".into(),
            expires_in_hours: Some(1),
            clarifications: ClarificationPolicy::Proceed,
        })
        .await
        .unwrap();
    let Reply::Directive(directive) = reply else {
        panic!("directive")
    };
    assert_eq!(directive.source, DirectiveSource::Operator);
    assert_eq!(directive.clarifications, ClarificationPolicy::Proceed);
    clarifications::expire(&h.ctx, archon_core::now())
        .await
        .unwrap();
    assert!(matches!(
        detail(&h, &pending).await.resolution,
        Some(ClarificationResolution::Assumed { .. })
    ));
    assert!(ask(&h, &stage, None).await.resolution.is_some());
}

#[tokio::test]
async fn long_questions_bypass_recovery_and_standby_policy_holds() {
    let (h, stage) = running(Config::default()).await;
    h.ctx.recovery.begin();
    h.ctx.standby.set("prior disk-full hold".into()).unwrap();
    let text = "Important context. ".repeat(400);
    let reply = handlers::handle(
        &h.ctx,
        ClientKind::ArchonMcp,
        Request::MessageOrchestrator {
            caller: stage.aether_agent.clone(),
            kind: MessageKind::Question,
            text: text.clone(),
            clarification: None,
        },
    )
    .await
    .unwrap();
    let Reply::MessageReceipt {
        clarification: Some(record),
        ..
    } = reply
    else {
        panic!("question receipt")
    };
    assert_eq!(record.question, text);
    assert!(record.resolution.is_none());
}

#[tokio::test]
async fn forwarded_cli_and_observer_cannot_grant_a_proceed_directive() {
    let h = Harness::new().await;
    let request = Request::SetDirective {
        text: "proceed".into(),
        expires_in_hours: Some(1),
        clarifications: ClarificationPolicy::Proceed,
    };
    for (client, request) in [
        (
            ClientKind::ArchonCli,
            Request::ForAgent {
                caller: archon_core::aether_ids::AetherAgentId::parse(common::ORCHESTRATOR)
                    .unwrap(),
                request: Box::new(request.clone()),
            },
        ),
        (ClientKind::ArchonWeb, request),
    ] {
        assert!(matches!(
            handlers::handle(&h.ctx, client, request).await,
            Err(ArchonError::Forbidden(_))
        ));
    }
    assert!(
        h.ctx
            .store
            .lock()
            .await
            .directives(archon_core::now())
            .unwrap()
            .is_empty()
    );
}
