//! What a paired device may ask archond, by its role. An operator runs the
//! queue, its gates and the operator chat; an observer only reads. Each
//! request runs with `archon-web` standing, so archond's own checks apply
//! as well; nothing here grants authority.

use archon_core::protocol::{DeviceRole, PauseTarget, Request};

/// Why a device may not send a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Host administration, settings, merges and agent requests: CLI only.
    HostOnly,
    /// A hard pause or resume also stops or starts stage agents: CLI only.
    Hard,
    /// A change, and the device only observes.
    Observer,
}

impl Refusal {
    /// The refusal's message, naming the request.
    pub fn message(self, name: &str) -> String {
        match self {
            Self::HostOnly => {
                format!("{name} is not available from the web app; use the archon CLI")
            }
            Self::Hard => {
                format!("{name} with hard is for the archon CLI; the web app pauses softly")
            }
            Self::Observer => format!("{name} changes Archon, and this device only observes"),
        }
    }
}

/// Whether a device in `role` may send `request`, or why not.
pub fn check(role: DeviceRole, request: &Request) -> Result<(), Refusal> {
    if hard(request) {
        return Err(Refusal::Hard);
    }
    if !operates(request) {
        return Err(Refusal::HostOnly);
    }
    match role {
        DeviceRole::Operator => Ok(()),
        DeviceRole::Observer if request.is_read_only() => Ok(()),
        DeviceRole::Observer => Err(Refusal::Observer),
    }
}

/// Whether a device in `role` may send `request`.
pub fn allowed(role: DeviceRole, request: &Request) -> bool {
    check(role, request).is_ok()
}

fn hard(request: &Request) -> bool {
    matches!(
        request,
        Request::Pause(PauseTarget::Global { hard: true })
            | Request::Resume(PauseTarget::Global { hard: true })
    )
}

/// What an operator device may send. Host administration (shutdown,
/// upgrade, gc, remote access itself), settings and ceilings, merges and
/// promotions, and every agent-side request stay with the CLI.
fn operates(request: &Request) -> bool {
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
            | R::ModelPolicy { .. }
            | R::GetInitiative { .. }
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
            | R::SubscribeChanges { .. }
            | R::ChangesPage { .. }
            | R::ItemHistory { .. }
            | R::TaskGraph { .. }
            | R::RunPlan { .. }
            | R::ItemLinks { .. }
            | R::IssueItems { .. }
            | R::ListGithubObjects { .. }
            | R::Enqueue(_)
            | R::SteerItem { .. }
            | R::UpdateItem { .. }
            | R::CancelItem { .. }
            | R::RetryItem { .. }
            | R::Pause(_)
            | R::Resume(_)
            | R::Answer { .. }
            | R::Say { .. }
            | R::LinkIssue { .. }
            | R::UnlinkIssue { .. }
            | R::ImportIssue { .. }
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
    use archon_core::protocol::RequestClass;

    use super::*;

    const ITEM: &str = "qi_01M621D9ZBJPTMDTV26KKQNBE1";

    fn parse(json: serde_json::Value) -> Request {
        serde_json::from_value(json).expect("a request")
    }

    fn github_ref() -> serde_json::Value {
        serde_json::json!({"host": "github.com", "repo": "o/r", "number": 1, "kind": "issue"})
    }

    /// Every change an operator device may send.
    fn operator_changes() -> Vec<Request> {
        let enqueue = serde_json::json!({
            "cmd": "enqueue", "title": "t", "brief": "b", "repo": null, "pipeline": null,
            "depends_on": [], "budget_usd": null, "idempotency_key": null,
        });
        [
            enqueue,
            serde_json::json!({"cmd": "cancel_item", "item": ITEM}),
            serde_json::json!({"cmd": "retry_item", "item": ITEM}),
            serde_json::json!({"cmd": "steer_item", "item": ITEM, "steer": {"note": "note"}}),
            serde_json::json!({"cmd": "update_item", "item": ITEM, "patch": {"priority": "high"}}),
            serde_json::json!({"cmd": "pause", "scope": "global"}),
            serde_json::json!({"cmd": "resume", "scope": "repo", "name": "archon"}),
            serde_json::json!({"cmd": "pause", "scope": "pipeline", "name": "adaptive"}),
            serde_json::json!({"cmd": "answer", "question": "q_01M621D9ZBJPTMDTV26KKQNBE1", "text": "yes"}),
            serde_json::json!({"cmd": "say", "text": "hi"}),
            serde_json::json!({"cmd": "link_issue", "item": ITEM, "ref": github_ref(), "relation": "closes"}),
            serde_json::json!({"cmd": "unlink_issue", "item": ITEM, "ref": github_ref(), "relation": "closes"}),
            serde_json::json!({"cmd": "import_issue", "ref": github_ref()}),
        ]
        .into_iter()
        .map(parse)
        .collect()
    }

    #[test]
    fn daily_operation_is_allowed_to_operators() {
        for json in [
            serde_json::json!({"cmd": "list_questions", "open_only": true}),
            serde_json::json!({"cmd": "say", "text": "hi"}),
            serde_json::json!({"cmd": "feed_history", "before": null, "limit": 10, "verbose": false}),
        ] {
            assert!(
                allowed(DeviceRole::Operator, &parse(json.clone())),
                "{json}"
            );
        }
        let changes = operator_changes();
        assert_eq!(changes.len(), 13, "every change parsed");
        for request in &changes {
            assert!(allowed(DeviceRole::Operator, request), "{request:?}");
        }
    }

    #[test]
    fn an_observer_is_refused_every_change_and_reads_the_rest() {
        let changes = operator_changes();
        assert!(
            changes.iter().all(|r| !r.is_read_only()),
            "the samples change Archon"
        );
        for request in &changes {
            assert_eq!(
                check(DeviceRole::Observer, request),
                Err(Refusal::Observer),
                "{request:?}"
            );
        }
        for json in [
            serde_json::json!({"cmd": "repo_list"}),
            serde_json::json!({"cmd": "subscribe_feed", "after": null, "verbose": false}),
            serde_json::json!({"cmd": "item_history", "item": ITEM}),
            serde_json::json!({"cmd": "changes_page", "after": 0}),
            serde_json::json!({"cmd": "model_policy", "repo": null}),
            serde_json::json!({"cmd": "get_initiative", "initiative": "ini_01M621D9ZBJPTMDTV26KKQNBE1"}),
        ] {
            let request = parse(json.clone());
            assert_eq!(request.class(), RequestClass::Read, "{json}");
            assert!(allowed(DeviceRole::Operator, &request), "{json}");
            assert!(allowed(DeviceRole::Observer, &request), "{json}");
        }
    }

    #[test]
    fn a_hard_pause_or_resume_is_refused_to_every_role() {
        for role in [DeviceRole::Operator, DeviceRole::Observer] {
            for request in [
                Request::Pause(PauseTarget::Global { hard: true }),
                Request::Resume(PauseTarget::Global { hard: true }),
            ] {
                assert_eq!(
                    check(role, &request),
                    Err(Refusal::Hard),
                    "{role:?} {request:?}"
                );
            }
            let soft = Request::Pause(PauseTarget::Global { hard: false });
            assert_eq!(allowed(role, &soft), role == DeviceRole::Operator);
        }
        let hard = parse(serde_json::json!({"cmd": "resume", "scope": "global", "hard": true}));
        assert!(!allowed(DeviceRole::Operator, &hard));
    }

    #[test]
    fn administration_merges_and_agent_requests_are_refused() {
        for json in [
            serde_json::json!({"cmd": "shutdown"}),
            serde_json::json!({"cmd": "remote_pair"}),
            serde_json::json!({"cmd": "remote_pair", "role": "observer"}),
            serde_json::json!({"cmd": "remote_status"}),
            serde_json::json!({"cmd": "remote_revoke", "device": "x"}),
            serde_json::json!({"cmd": "orchestrator_restart"}),
            serde_json::json!({"cmd": "operator_command", "text": "/archon resume --hard"}),
            serde_json::json!({"cmd": "ask_operator", "text": "?", "item": null}),
            serde_json::json!({"cmd": "for_agent", "caller": "agt_1", "request": {"cmd": "status"}}),
            serde_json::json!({"cmd": "merge_item", "item": ITEM}),
        ] {
            let request = parse(json.clone());
            for role in [DeviceRole::Operator, DeviceRole::Observer] {
                assert_eq!(check(role, &request), Err(Refusal::HostOnly), "{json}");
            }
        }
        assert_eq!(
            command_name(&parse(serde_json::json!({"cmd": "shutdown"}))),
            "shutdown"
        );
    }
}
