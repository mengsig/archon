//! The order of `handlers::handle`'s authorization steps: an operator-only
//! request for an agent is refused before recovery holds it, and standby
//! refuses a change before the orchestrator's authority is checked.

mod common;

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::protocol::{PauseTarget, Request};
use archon_daemon::recovery::RECOVERY_GRACE;
use common::Harness;

/// Well under [`RECOVERY_GRACE`], which a change waits while recovery runs.
const PROMPT: Duration = Duration::from_millis(500);

fn for_agent(caller: &str, request: Request) -> Request {
    Request::ForAgent {
        caller: AetherAgentId::parse(caller).expect("agent id"),
        request: Box::new(request),
    }
}

fn soft_pause() -> Request {
    Request::Pause(PauseTarget::Global { hard: false })
}

#[tokio::test]
async fn a_forwarded_operator_only_request_is_refused_before_recovery() {
    assert!(PROMPT < RECOVERY_GRACE, "the refusal must beat the grace");
    let h = Harness::new().await;
    h.ctx.recovery.begin();
    let say = for_agent(common::ORCHESTRATOR, Request::Say { text: "hi".into() });
    let refused = tokio::time::timeout(PROMPT, h.cli(say))
        .await
        .expect("refused without waiting for recovery")
        .expect_err("operator only");
    assert!(matches!(refused, ArchonError::Forbidden(_)), "{refused:?}");

    let pause = for_agent(common::ORCHESTRATOR, soft_pause());
    let held = h.cli(pause).await.expect_err("recovery holds a change");
    assert!(matches!(held, ArchonError::Recovering(_)), "{held:?}");
}

#[tokio::test]
async fn standby_refuses_a_change_before_authority_is_checked() {
    let h = Harness::new().await;
    h.orchestrator(Request::Status(Default::default()))
        .await
        .expect("the orchestrator is named");
    let stranger = || for_agent("agt_stranger", soft_pause());
    let unauthorized = h.cli(stranger()).await.expect_err("not the orchestrator");
    assert!(
        matches!(&unauthorized, ArchonError::Forbidden(why) if why.contains("only the orchestrator")),
        "{unauthorized:?}"
    );

    h.ctx
        .standby
        .set("disk full".into())
        .expect("not in standby yet");
    let refused = h.cli(stranger()).await.expect_err("standby");
    assert!(matches!(refused, ArchonError::DiskFull(_)), "{refused:?}");
    let orchestrator = h.orchestrator(soft_pause()).await.expect_err("standby");
    assert!(
        matches!(orchestrator, ArchonError::DiskFull(_)),
        "{orchestrator:?}"
    );
}
