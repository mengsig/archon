//! What a paired device may ask archond: daily operation of the queue, its
//! gates and the operator chat. It is sent as the CLI would send it, so
//! archond's own checks apply unchanged; nothing here grants authority.

use archon_core::protocol::Request;

/// Whether the web app may send `request`. Host administration (shutdown,
/// upgrade, gc, remote access itself), settings and ceilings, merges and
/// promotions, and every agent-side request stay with the CLI.
pub fn allowed(request: &Request) -> bool {
    use Request as R;
    matches!(
        request,
        R::Hello { .. }
            | R::Status(_)
            | R::RepoList
            | R::ListItems(_)
            | R::ListItemSummaries(_)
            | R::GetItem { .. }
            | R::GetStage(_)
            | R::ResolveItem { .. }
            | R::ListPipelines { .. }
            | R::ListQuestions { .. }
            | R::FeedHistory { .. }
            | R::SubscribeFeed { .. }
            | R::ListReports
            | R::OrchestratorStatus
            | R::CostReport(_)
            | R::ListArtifacts { .. }
            | R::ReadArtifact { .. }
            | R::ItemDiff { .. }
            | R::Settings { .. }
            | R::Autonomy { .. }
            | R::Enqueue(_)
            | R::CancelItem { .. }
            | R::RetryItem { .. }
            | R::Pause(_)
            | R::Resume(_)
            | R::Answer { .. }
            | R::Say { .. }
    )
}

/// The wire name of a request, for a refusal's message.
pub fn command_name(request: &Request) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "this request".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::protocol::PauseTarget;

    fn parse(json: serde_json::Value) -> Request {
        serde_json::from_value(json).expect("a request")
    }

    #[test]
    fn daily_operation_is_allowed() {
        for json in [
            serde_json::json!({"cmd": "list_questions", "open_only": true}),
            serde_json::json!({"cmd": "say", "text": "hi"}),
            serde_json::json!({"cmd": "feed_history", "before": null, "limit": 10, "verbose": false}),
        ] {
            assert!(allowed(&parse(json.clone())), "{json}");
        }
        assert!(allowed(&Request::Pause(PauseTarget::Global {
            hard: false
        })));
    }

    #[test]
    fn administration_merges_and_agent_requests_are_refused() {
        for json in [
            serde_json::json!({"cmd": "shutdown"}),
            serde_json::json!({"cmd": "remote_pair"}),
            serde_json::json!({"cmd": "remote_status"}),
            serde_json::json!({"cmd": "remote_revoke", "device": "x"}),
            serde_json::json!({"cmd": "orchestrator_restart"}),
            serde_json::json!({"cmd": "operator_command", "text": "/archon resume --hard"}),
            serde_json::json!({"cmd": "ask_operator", "text": "?", "item": null}),
            serde_json::json!({"cmd": "for_agent", "caller": "agt_1", "request": {"cmd": "status"}}),
        ] {
            let request = parse(json.clone());
            assert!(!allowed(&request), "{json}");
        }
        assert_eq!(
            command_name(&parse(serde_json::json!({"cmd": "shutdown"}))),
            "shutdown"
        );
    }
}
