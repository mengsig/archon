//! A declared review is checked against Aether's tree, never taken from the
//! lead's word: a pass counts only with as many reviewers the lead delegated
//! to as the item's review level asks for, each a different agent.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{StageOutcome, StageRun};
use archon_pipeline::review;

use crate::context::DaemonContext;

/// Agents named in a refusal: enough to see what the lead delegated to.
const MAX_LISTED: usize = 6;

/// `outcome` with Archon's own finding on the review pass it claims, for a
/// `stage` owing `owed` reviewers (0: none). `Err` while Aether cannot be asked.
pub(super) async fn verify(
    ctx: &DaemonContext,
    stage: &StageRun,
    owed: u32,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    let StageOutcome::Completed {
        summary,
        structured: Some(mut structured),
    } = outcome
    else {
        return Ok(outcome);
    };
    let models = review::claimed_reviewers(&structured);
    let found = match owed == 0 || models.is_empty() {
        true => None,
        false => Some(reviewers(ctx, stage, &models, owed).await?),
    };
    review::record_check(&mut structured, found);
    Ok(StageOutcome::Completed {
        summary,
        structured: Some(structured),
    })
}

/// The agents that reviewed for `stage`'s lead on `models`, or why they do
/// not make the `owed` reviewers.
async fn reviewers(
    ctx: &DaemonContext,
    stage: &StageRun,
    models: &[String],
    owed: u32,
) -> Result<Result<Vec<String>, String>, ArchonError> {
    let lead = stage.aether_agent.as_ref().ok_or_else(|| {
        ArchonError::Protocol(format!("completed stage {} has no agent", stage.id))
    })?;
    match ctx.backend.descendants(lead).await {
        Ok(beneath) => Ok(pick_reviewers(lead, &beneath, models, owed)),
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

/// A distinct direct child of `lead` that completed on each of `models`,
/// newest first: Aether's ids sort by creation, and a review comes last.
fn pick_reviewers(
    lead: &AetherAgentId,
    beneath: &[Descendant],
    models: &[String],
    owed: u32,
) -> Result<Vec<String>, String> {
    assert!(
        owed >= 1 && !models.is_empty(),
        "only a claimed, owed review is checked"
    );
    if models.len() < owed as usize {
        return Err(format!(
            "it names {} reviewer(s), and this item's review level needs {owed}, each a \
             different agent",
            models.len()
        ));
    }
    let mut children: Vec<&Descendant> = beneath.iter().filter(|d| d.parent == *lead).collect();
    children.sort_by(|a, b| b.agent.as_str().cmp(a.agent.as_str()));
    let mut picked: Vec<&Descendant> = Vec::with_capacity(models.len());
    for model in models {
        let free = |d: &&&Descendant| {
            d.state == DescendantState::Completed
                && d.model.to_string() == *model
                && !picked.iter().any(|p| p.agent == d.agent)
        };
        let Some(reviewer) = children.iter().find(free) else {
            return Err(missing(&children, model, picked.len()));
        };
        assert_ne!(reviewer.agent, *lead, "a lead is not its own reviewer");
        picked.push(reviewer);
    }
    assert_eq!(picked.len(), models.len(), "one agent per model named");
    Ok(picked.iter().map(|d| d.agent.to_string()).collect())
}

/// Why no further child of the lead reviewed on `model`, after `found` others.
fn missing(children: &[&Descendant], model: &str, found: usize) -> String {
    if children.is_empty() {
        return format!("it names a reviewer on {model}, but the lead delegated to no agent");
    }
    let mut listed: Vec<String> = children.iter().take(MAX_LISTED).map(|d| held(d)).collect();
    if children.len() > MAX_LISTED {
        listed.push(format!("{} more", children.len() - MAX_LISTED));
    }
    let another = match found {
        0 => "no agent",
        _ => "no further agent",
    };
    format!(
        "it names a reviewer on {model}, but {another} the lead delegated to completed on that \
         model ({})",
        listed.join("; ")
    )
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

    fn pick(beneath: &[Descendant], models: &[&str], owed: u32) -> Result<Vec<String>, String> {
        let models: Vec<String> = models.iter().map(|m| (*m).to_owned()).collect();
        pick_reviewers(&agent("agt_lead"), beneath, &models, owed)
    }

    #[test]
    fn the_reviewer_is_the_latest_child_of_the_lead_that_completed_on_the_model() {
        use DescendantState::{Completed, Live, Stopped};
        let beneath = [
            child("agt_c1", "agt_lead", "a/review", Completed),
            child("agt_c2", "agt_lead", "a/review", Completed),
            child("agt_c3", "agt_lead", "a/review", Stopped),
            child("agt_c4", "agt_lead", "a/build", Completed),
            child("agt_c5", "agt_c4", "a/review", Completed),
            child("agt_c6", "agt_lead", "a/review", Live),
        ];
        assert_eq!(pick(&beneath, &["a/review"], 1).unwrap(), ["agt_c2"]);
    }

    #[test]
    fn a_pass_without_a_completed_reviewer_says_what_the_lead_delegated_to() {
        use DescendantState::{Completed, Stopped};
        let none = pick(&[], &["a/review"], 1).unwrap_err();
        assert!(none.contains("delegated to no agent"), "{none}");

        let beneath = [
            child("agt_c1", "agt_lead", "a/review", Stopped),
            child("agt_c2", "agt_lead", "a/build", Completed),
            child("agt_c3", "agt_c2", "a/review", Completed),
        ];
        let refused = pick(&beneath, &["a/review"], 1).unwrap_err();
        assert!(refused.contains("agt_c1 on a/review ended without completing"));
        assert!(refused.contains("agt_c2 on a/build completed"), "{refused}");
        assert!(
            !refused.contains("agt_c3"),
            "a grandchild is not the lead's reviewer"
        );
    }

    /// A strict review is several agents, so one reviewer named twice or a
    /// record shorter than the level asks for is not it.
    #[test]
    fn several_reviewers_are_different_agents_and_as_many_as_the_level_owes() {
        use DescendantState::Completed;
        let beneath = [
            child("agt_c1", "agt_lead", "a/review", Completed),
            child("agt_c2", "agt_lead", "b/review", Completed),
            child("agt_c3", "agt_lead", "a/review", Completed),
        ];
        let three = ["a/review", "b/review", "a/review"];
        assert_eq!(
            pick(&beneath, &three, 3).unwrap(),
            ["agt_c3", "agt_c2", "agt_c1"]
        );
        let short = pick(&beneath, &three[..2], 3).unwrap_err();
        assert!(
            short.contains("names 2 reviewer(s)") && short.contains("needs 3"),
            "{short}"
        );
        let twice = pick(&beneath[..2], &three, 3).unwrap_err();
        assert!(
            twice.contains("no further agent the lead delegated to"),
            "{twice}"
        );
        let more = ["a/review", "b/review", "a/review", "b/review"];
        assert!(
            pick(&beneath, &more, 3).is_err(),
            "every model named is checked"
        );
    }
}
