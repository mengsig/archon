//! Arguments of `steer_item`.

use archon_core::protocol::Request;
use archon_core::steer::{Steer, Then};
use serde::Deserialize;

use super::{Args, ToolError};

/// `then` as the tool spells it; `goto` names its stage in a field of its own.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ThenName {
    Retry,
    Skip,
    Goto,
}

pub(super) fn steer_item(args: &mut Args) -> Result<Request, ToolError> {
    let item = args.required("item")?;
    let abort = abort(args)?;
    let steer = Steer {
        note: args.optional_text("note")?,
        stage: args.optional_text("stage")?,
        model: args.optional("model")?,
        effort: args.optional("effort")?,
        budget_usd: args.optional_budget("budget_usd")?,
        extend_minutes: args.optional("extend_minutes")?,
        stage_timeouts: args.optional("stage_timeouts")?.unwrap_or_default(),
        abort,
        insert_stage: args.optional("insert_stage")?,
        switch_pipeline: args.optional_text("switch_pipeline")?,
    };
    steer.check().map_err(|e| args.refusal(e))?;
    Ok(Request::SteerItem { item, steer })
}

/// `abort_stage`, `then` and `goto` as one decision.
fn abort(args: &mut Args) -> Result<Option<Then>, ToolError> {
    let aborts = args.optional::<bool>("abort_stage")?.unwrap_or(false);
    let then = args.optional::<ThenName>("then")?;
    let goto = args.optional_text("goto")?;
    if !aborts {
        return match (then, goto) {
            (None, None) => Ok(None),
            (Some(_), _) => Err(args.error("then", "applies only with abort_stage: true")),
            (None, Some(_)) => Err(args.error("goto", "applies only with abort_stage: true")),
        };
    }
    match (then.unwrap_or(ThenName::Retry), goto) {
        (ThenName::Retry, None) => Ok(Some(Then::Retry)),
        (ThenName::Skip, None) => Ok(Some(Then::Skip)),
        (ThenName::Goto, Some(to)) => Ok(Some(Then::Goto { to })),
        (ThenName::Goto, None) => Err(args.error("goto", "is required with then: goto")),
        (_, Some(_)) => Err(args.error("goto", "applies only with then: goto")),
    }
}

#[cfg(test)]
mod tests {
    use archon_core::id::QueueItemId;
    use serde_json::{Value, json};

    use super::*;
    use crate::tools::to_request;

    fn steer(mut arguments: Value) -> Result<Steer, ToolError> {
        arguments["item"] = json!(QueueItemId::new().to_string());
        match to_request("steer_item", arguments)? {
            Request::SteerItem { steer, .. } => Ok(steer),
            other => panic!("steer_item maps to SteerItem: {other:?}"),
        }
    }

    #[test]
    fn an_aborted_stage_retries_unless_told_otherwise() {
        let abort = |arguments| steer(arguments).unwrap().abort;
        assert_eq!(abort(json!({"abort_stage": true})), Some(Then::Retry));
        assert_eq!(
            abort(json!({"abort_stage": true, "then": "skip"})),
            Some(Then::Skip)
        );
        let goto = json!({"abort_stage": true, "then": "goto", "goto": "review"});
        assert_eq!(
            abort(goto),
            Some(Then::Goto {
                to: "review".into()
            })
        );
        assert_eq!(abort(json!({"note": "n", "abort_stage": false})), None);
    }

    #[test]
    fn then_and_goto_without_their_condition_are_refused() {
        let refused = [
            json!({"then": "skip"}),
            json!({"note": "n", "goto": "review"}),
            json!({"abort_stage": true, "then": "goto"}),
            json!({"abort_stage": true, "then": "skip", "goto": "review"}),
            json!({"abort_stage": true, "then": "restart"}),
            json!({}),
            json!({"insert_stage": {"stage": "review", "before": "ship"}}),
        ];
        for arguments in refused {
            let error = steer(arguments.clone()).expect_err(&arguments.to_string());
            assert!(error.0.starts_with("steer_item: "), "{error}");
        }
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stage_timeout_overrides_parse_without_other_changes() {
        let request = crate::tools::to_request(
            "steer_item",
            json!({
                "item": archon_core::id::QueueItemId::new(), "stage_timeouts": {"implement": 180}
            }),
        )
        .unwrap();
        let Request::SteerItem { steer, .. } = request else {
            panic!("steer");
        };
        assert_eq!(steer.stage_timeouts["implement"], 180);
    }
}
