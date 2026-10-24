//! Text for a tracked change's status (`request_status`, or a repeat under
//! the same idempotency key). The first word is the state, as the caller
//! branches on it.

use archon_core::tracked::{RequestStatus, TrackedState};

pub(super) fn status(status: &RequestStatus) -> String {
    let id = &status.request;
    let command = status.command.as_str();
    match status.state {
        TrackedState::Pending => format!(
            "pending: {id}\n`{command}` is still being carried out. Ask request_status for \
             {id} again shortly{}; do not make the call again without it.",
            repeat_hint(status)
        ),
        TrackedState::Applied => {
            let answer = status
                .reply
                .as_deref()
                .map(|reply| super::reply(command, reply))
                .unwrap_or_default();
            format!("applied: {id}\n`{command}` was carried out. Its answer: {answer}")
        }
        TrackedState::Failed => {
            let why = status.error.as_ref().map(|e| e.message.as_str());
            format!(
                "failed: {id}\n`{command}` failed: {}\nA repeat under the same \
                 idempotency_key returns this failure again; use a new key to try again.",
                why.unwrap_or("no reason was recorded")
            )
        }
        TrackedState::Interrupted => format!(
            "interrupted: {id}\narchond stopped while `{command}` ran, so it may or may not have \
             taken effect. Check with get_item or list_queue before making it again under a new \
             idempotency_key."
        ),
    }
}

fn repeat_hint(status: &RequestStatus) -> String {
    match &status.idempotency_key {
        Some(key) => format!(", or repeat it with idempotency_key `{key}`"),
        None => String::new(),
    }
}
