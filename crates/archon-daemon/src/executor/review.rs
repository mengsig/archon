//! A declared review is checked against Aether's tree, never taken from the
//! lead's word: a pass counts only with a reviewer the lead delegated to.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{StageOutcome, StageRun};
use archon_pipeline::review;

use crate::context::DaemonContext;

/// Agents named in a refusal: enough to see what the lead delegated to.
const MAX_LISTED: usize = 6;

/// `outcome` with Archon's finding on the pass it claims in place of any an agent
/// wrote; found only for a `stage` that holds the review. `Err` while Aether cannot be asked.
pub(super) async fn verify(
    ctx: &DaemonContext,
    stage: &StageRun,
    holds_review: bool,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    let StageOutcome::Completed {
        summary,
        structured: Some(mut structured),
    } = outcome
    else {
        return Ok(outcome);
    };
    let found = match review::claimed_reviewer(&structured) {
        Some(model) if holds_review => Some(reviewer(ctx, stage, &model).await?),
        _ => None,
    };
    review::record_check(&mut structured, found);
    Ok(StageOutcome::Completed {
        summary,
        structured: Some(structured),
    })
}

/// The agent that reviewed for `stage`'s lead on `model`, or why there is none.
async fn reviewer(
    ctx: &DaemonContext,
    stage: &StageRun,
    model: &str,
) -> Result<Result<String, String>, ArchonError> {
    let lead = stage.aether_agent.as_ref().ok_or_else(|| {
        ArchonError::Protocol(format!("completed stage {} has no agent", stage.id))
    })?;
    match ctx.backend.descendants(lead).await {
        Ok(beneath) => Ok(pick_reviewer(lead, &beneath, model)),
        // Left live: the pump's reconciliation settles and checks it again.
        Err(
            error @ (ArchonError::Unavailable(_)
            | ArchonError::Protocol(_)
            | ArchonError::Io { .. }),
        ) => Err(error),
        Err(error) => Ok(Err(format!(
            "Aether could not list the agents the lead delegated to: {error}"
        ))),
    }
}

/// The latest direct child of `lead` that completed on `model`. Aether's
/// agent ids sort by creation, and the review is the last a lead delegates.
fn pick_reviewer(
    lead: &AetherAgentId,
    beneath: &[Descendant],
    model: &str,
) -> Result<String, String> {
    let children: Vec<&Descendant> = beneath.iter().filter(|d| d.parent == *lead).collect();
    assert!(children.len() <= beneath.len());
    let reviewer = children
        .iter()
        .filter(|d| d.state == DescendantState::Completed && d.model.to_string() == model)
        .max_by(|a, b| a.agent.as_str().cmp(b.agent.as_str()));
    if let Some(reviewer) = reviewer {
        assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
        return Ok(reviewer.agent.to_string());
    }
    if children.is_empty() {
        return Err(format!(
            "it names a reviewer on {model}, but the lead delegated to no agent"
        ));
    }
    let mut listed: Vec<String> = children.iter().take(MAX_LISTED).map(|d| held(d)).collect();
    if children.len() > MAX_LISTED {
        listed.push(format!("{} more", children.len() - MAX_LISTED));
    }
    Err(format!(
        "it names a reviewer on {model}, but no agent the lead delegated to completed on that \
         model ({})",
        listed.join("; ")
    ))
}

/// One delegated agent as a refusal names it.
fn held(child: &Descendant) -> String {
    let how = match child.state {
        DescendantState::Live => "still running",
        DescendantState::Completed => "completed",
        DescendantState::Stopped => "ended without completing",
    };
    format!("{} on {} {how}", child.agent, child.model)
}

#[cfg(test)]
mod tests {
    use archon_core::model::ModelRef;

    use super::*;

    fn agent(id: &str) -> AetherAgentId {
        AetherAgentId::parse(id).unwrap()
    }

    fn child(id: &str, parent: &str, model: &str, state: DescendantState) -> Descendant {
        Descendant {
            agent: agent(id),
            parent: agent(parent),
            model: model.parse::<ModelRef>().unwrap(),
            effort: None,
            state,
        }
    }

    #[test]
    fn the_reviewer_is_the_latest_child_of_the_lead_that_completed_on_the_model() {
        use DescendantState::{Completed, Live, Stopped};
        let lead = agent("agt_lead");
        let beneath = [
            child("agt_c1", "agt_lead", "a/review", Completed),
            child("agt_c2", "agt_lead", "a/review", Completed),
            child("agt_c3", "agt_lead", "a/review", Stopped),
            child("agt_c4", "agt_lead", "a/build", Completed),
            child("agt_c5", "agt_c4", "a/review", Completed),
            child("agt_c6", "agt_lead", "a/review", Live),
        ];
        assert_eq!(
            pick_reviewer(&lead, &beneath, "a/review").unwrap(),
            "agt_c2"
        );
    }

    #[test]
    fn a_pass_without_a_completed_reviewer_says_what_the_lead_delegated_to() {
        use DescendantState::{Completed, Stopped};
        let lead = agent("agt_lead");
        let none = pick_reviewer(&lead, &[], "a/review").unwrap_err();
        assert!(none.contains("delegated to no agent"), "{none}");

        let beneath = [
            child("agt_c1", "agt_lead", "a/review", Stopped),
            child("agt_c2", "agt_lead", "a/build", Completed),
            child("agt_c3", "agt_c2", "a/review", Completed),
        ];
        let refused = pick_reviewer(&lead, &beneath, "a/review").unwrap_err();
        assert!(refused.contains("agt_c1 on a/review ended without completing"));
        assert!(refused.contains("agt_c2 on a/build completed"), "{refused}");
        assert!(
            !refused.contains("agt_c3"),
            "a grandchild is not the lead's reviewer"
        );
    }
}
