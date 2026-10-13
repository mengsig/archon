//! Arguments of the initiative tools.

use archon_core::initiative::EnqueueInitiative;
use archon_core::protocol::Request;

use super::{Args, ToolError};

pub(super) fn enqueue_initiative(args: &mut Args) -> Result<Request, ToolError> {
    let request = EnqueueInitiative {
        title: args.required_text("title")?,
        contract: args.required_text("contract")?,
        atomic: args.optional("atomic")?.unwrap_or(false),
        members: args.required("members")?,
        idempotency_key: args.optional_text("idempotency_key")?,
    };
    request.check().map_err(|e| args.refusal(e))?;
    assert!(!request.members.is_empty(), "checked above");
    Ok(Request::EnqueueInitiative(request))
}

pub(super) fn update_initiative(args: &mut Args) -> Result<Request, ToolError> {
    Ok(Request::UpdateInitiative {
        initiative: args.required("initiative")?,
        contract_append: args.required_text("contract_append")?,
    })
}
