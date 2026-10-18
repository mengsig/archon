//! Whether the orchestrator runs what this Archon would start now: its
//! prompt, tools and lessons, or a restart is owed.

use archon_client::ArchonClient;
use archon_core::protocol::{ErrorCode, OrchestratorInfo, Reply, Request};

use super::registry::COMMAND_TIMEOUT;
use super::{Check, Level, check};
use crate::connect::unexpected;

pub(super) async fn check_orchestrator(archond: &anyhow::Result<Option<ArchonClient>>) -> Check {
    let Ok(Some(client)) = archond else {
        return check(
            "orchestrator",
            Level::Warn,
            "unknown: archond is not running",
        );
    };
    let call = client.call(Request::OrchestratorStatus);
    let reply = match tokio::time::timeout(COMMAND_TIMEOUT, call).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => return check("orchestrator", Level::Warn, format!("unknown: {error:#}")),
        Err(_) => {
            let detail = format!("unknown: archond did not answer in {COMMAND_TIMEOUT:?}");
            return check("orchestrator", Level::Warn, detail);
        }
    };
    match reply {
        Ok(Reply::Orchestrator(info)) => freshness_verdict(&info),
        Err(refusal) if refusal.code == ErrorCode::NotFound => {
            check("orchestrator", Level::Ok, "not created yet")
        }
        Err(refusal) => check(
            "orchestrator",
            Level::Warn,
            format!("unknown: {}", refusal.message),
        ),
        Ok(other) => {
            let error = unexpected(&other, "orchestrator_status");
            check("orchestrator", Level::Warn, format!("unknown: {error:#}"))
        }
    }
}

/// A warning when a restart is owed, or cannot be ruled out.
fn freshness_verdict(info: &OrchestratorInfo) -> Check {
    let line = info.describe_freshness();
    assert!(
        !line.is_empty(),
        "freshness always describes the generation"
    );
    let stale = info.freshness.as_ref().is_none_or(|fresh| fresh.is_stale());
    if !stale {
        return check("orchestrator", Level::Ok, line);
    }
    check(
        "orchestrator",
        Level::Warn,
        format!("{line}; `archon orchestrator restart` starts a current one"),
    )
}

#[cfg(test)]
mod tests {
    use archon_core::protocol::OrchestratorFreshness;

    use super::*;

    fn info(freshness: Option<OrchestratorFreshness>) -> OrchestratorInfo {
        serde_json::from_value(serde_json::json!({
            "agent": "agt_01M52R3JSXYHJJA0AZX2B6MCD7", "model": "claude-code/opus",
            "effort": "high", "status": "ready", "restarts": 6, "generation": 7,
        }))
        .map(|info: OrchestratorInfo| OrchestratorInfo { freshness, ..info })
        .unwrap()
    }

    fn fresh(prompt_outdated: Option<bool>, new_lessons: u32) -> OrchestratorFreshness {
        OrchestratorFreshness {
            created_by: "1.2.3".into(),
            prompt_outdated,
            tools_outdated: false,
            new_lessons,
            settings_drift: None,
        }
    }

    #[test]
    fn a_current_orchestrator_is_ok_and_a_stale_one_warns_with_the_fix() {
        let current = freshness_verdict(&info(Some(fresh(Some(false), 0))));
        assert_eq!(current.level, Level::Ok);
        assert_eq!(current.detail, "generation 7, created by 1.2.3; up to date");
        let stale = freshness_verdict(&info(Some(fresh(Some(true), 3))));
        assert_eq!(stale.level, Level::Warn);
        assert!(
            stale
                .detail
                .starts_with("generation 7, created by 1.2.3; prompt outdated; 3 new lessons"),
            "{}",
            stale.detail
        );
        assert!(stale.detail.contains("archon orchestrator restart"));
    }

    #[test]
    fn an_unrecorded_or_unknown_prompt_warns() {
        assert_eq!(freshness_verdict(&info(None)).level, Level::Warn);
        let unknown = freshness_verdict(&info(Some(fresh(None, 0))));
        assert_eq!(unknown.level, Level::Warn);
        assert!(
            unknown.detail.contains("prompt unknown"),
            "{}",
            unknown.detail
        );
    }
}
