use super::*;

const ERROR: &str = "Connection closed";
const TOOL: &str = "mcp__archon__model_policy";

fn tool_error(agent: &str, turn: u32, tool: &str) -> Value {
    json!({
        "type": "custom",
        "name": "turn_ended_after_tool_error",
        "payload": {
            "agent_id": agent,
            "turn": turn,
            "tool": tool,
            "error": ERROR,
            "at": "2026-11-21T10:20:00Z"
        }
    })
}

async fn end_turn(harness: &Harness, turn: u32) {
    orch(
        harness,
        json!({"type": "agent.turn_completed", "turn": turn, "next": "waiting"}),
    )
    .await;
}

#[tokio::test]
async fn the_first_tool_error_wakes_the_orchestrator_once_and_ignores_replay() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;

    end_turn(&harness, 1).await;
    orch(&harness, tool_error(ORCH, 1, TOOL)).await;
    let sent = harness.fake.received_named("trigger");
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["agent"], ORCH);
    assert_eq!(sent[0]["source"], "archon");
    let event = &sent[0]["payload"]["events"][0];
    assert_eq!(event["type"], "turn_ended_after_tool_error");
    assert_eq!(event["tool"], TOOL);
    assert_eq!(event["error"], ERROR);
    let instructions = event["text"].as_str().unwrap();
    assert!(instructions.contains("Retry") && instructions.contains("tell the operator"));

    hangup_and_replay(&harness, keys::CURSOR_ORCHESTRATOR).await;
    orch(&harness, tool_error(ORCH, 1, TOOL)).await;
    assert_eq!(harness.fake.received_named("trigger"), sent);
    assert!(
        harness
            .notifications(NotificationKind::Blocker)
            .await
            .is_empty()
    );
    pump.abort();
}

#[tokio::test]
async fn a_lost_wake_reply_replays_with_the_same_trigger_key() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    harness
        .fake
        .script("trigger", Scripted::Hangup { notice: None });
    end_turn(&harness, 1).await;
    orch(&harness, tool_error(ORCH, 1, TOOL)).await;

    let attempts = harness.fake.received_named("trigger");
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    let key = attempts[0]["idempotency_key"].as_str().unwrap();
    assert!(!key.is_empty());
    assert_eq!(attempts[1]["idempotency_key"], key);
    assert_eq!(attempts[1]["payload"], attempts[0]["payload"]);
    assert!(
        harness
            .notifications(NotificationKind::Blocker)
            .await
            .is_empty()
    );
    pump.abort();
}

#[tokio::test]
async fn consecutive_tool_errors_post_one_blocker_then_stop_until_a_turn_succeeds() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    end_turn(&harness, 1).await;
    orch(&harness, tool_error(ORCH, 1, TOOL)).await;

    let second_tool = "mcp__archon__settings";
    end_turn(&harness, 2).await;
    orch(&harness, tool_error(ORCH, 2, second_tool)).await;
    let blockers = harness.notifications(NotificationKind::Blocker).await;
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert!(blockers[0].contains(second_tool) && blockers[0].contains(ERROR));
    assert_eq!(harness.fake.received_named("trigger").len(), 1);

    hangup_and_replay(&harness, keys::CURSOR_ORCHESTRATOR).await;
    end_turn(&harness, 3).await;
    orch(&harness, tool_error(ORCH, 3, TOOL)).await;
    assert_eq!(
        harness.notifications(NotificationKind::Blocker).await,
        blockers
    );
    assert_eq!(harness.fake.received_named("trigger").len(), 1);

    end_turn(&harness, 4).await;
    end_turn(&harness, 5).await;
    orch(&harness, tool_error(ORCH, 5, TOOL)).await;
    assert_eq!(harness.fake.received_named("trigger").len(), 2);
    assert_eq!(
        harness.notifications(NotificationKind::Blocker).await,
        blockers
    );
    pump.abort();
}

#[tokio::test]
async fn other_agents_and_other_custom_events_do_not_trigger_tool_error_recovery() {
    let harness = Harness::new().await;
    let (_agents, pump) = running_pump(&harness).await;
    let mut child_event = fake::event(Some(CHILD), None, tool_error(CHILD, 1, TOOL));
    child_event["parent_agent_id"] = json!(ORCH);
    push(&harness, child_event, keys::CURSOR_ORCHESTRATOR).await;

    let mut other_custom = tool_error(ORCH, 1, TOOL);
    other_custom["name"] = json!("another_custom_event");
    orch(&harness, other_custom).await;
    assert!(harness.fake.received_named("trigger").is_empty());
    assert!(
        harness
            .notifications(NotificationKind::Blocker)
            .await
            .is_empty()
    );
    pump.abort();
}
