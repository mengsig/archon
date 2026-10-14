//! Arguments of the initiative tools.

use archon_core::initiative::{ContractChange, EnqueueInitiative, InitiativeLimits};
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
    // archond holds it to `[limits]`; this refuses only what no config allows.
    let ceiling = InitiativeLimits::CEILING;
    request.check(&ceiling).map_err(|e| args.refusal(e))?;
    assert!(!request.members.is_empty(), "checked above");
    Ok(Request::EnqueueInitiative(request))
}

pub(super) fn update_initiative(args: &mut Args) -> Result<Request, ToolError> {
    let initiative = args.required("initiative")?;
    let append = args.optional_text("contract_append")?;
    let change = match (append, args.optional_text("contract")?) {
        (Some(text), None) => ContractChange::Append(text),
        (None, Some(text)) => ContractChange::Replace(text),
        _ => {
            let why = "give exactly one of `contract_append` and `contract`";
            return Err(args.refusal(why));
        }
    };
    assert!(
        !change.text().trim().is_empty(),
        "optional_text refuses blanks"
    );
    Ok(Request::UpdateInitiative { initiative, change })
}
