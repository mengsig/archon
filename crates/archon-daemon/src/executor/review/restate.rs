//! A reviewer whose result Archon cannot read as a verdict counts for nothing,
//! and its lead used to learn that only from the refusal of its own record,
//! after the work was done, so the whole stage was repaired. Aether cannot ask
//! a finished agent again, so archond tells the live lead the moment such a
//! reviewer settles: one note says why the result does not count and the
//! exact shape its replacement must return. The note reaches a running turn
//! at its next model call; the turn is not interrupted, because an engine can
//! end the interrupted turn with the note read and leave the lead waiting for
//! mail that never comes. Each reviewer is named once, and at most
//! [`MAX_NOTES`] per stage attempt; nothing is read as a verdict that the
//! gate would not read, which still judges the lead's record unchanged.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::StageRun;

use super::{result_of, verdict_in};
use crate::context::DaemonContext;
use crate::executor::resume;

/// Unreadable reviewers one stage attempt's lead is told about; later ones are left to the gate.
pub(in crate::executor) const MAX_NOTES: usize = 2;

/// The role a lead delegates its reviewers with, as its contract asks.
const REVIEWER_ROLE: &str = "reviewer";

/// What the replacement reviewer is told to return, pasted by the lead.
const SHAPE: &str = "End by calling `complete_task` with `structured` set to \
{\"verdict\": \"pass\" or \"fail\", \"head\": \"<full commit you read>\", \"base\": \"<commit the diff starts at>\", \
\"findings\": [...]}, each finding marked \"blocking\": true or false. If you cannot call it, \
your final message is that JSON object alone: no `complete_task` word, no YAML, nothing before or after it.";

/// Tell `lead` at once when `reviewer`, its child that just settled, returned
/// no verdict the review gate reads, if `lead` holds a live stage attempt that
/// owes a review. An Aether answer that fails is returned, so the pump repeats the event.
pub(in crate::executor) async fn reviewer_unreadable(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Result<(), ArchonError> {
    let Some(stage) = owing_stage(ctx, lead).await? else {
        return Ok(());
    };
    let beneath = ctx.backend.descendants(lead).await?;
    let Some(child) = settled_reviewer(&beneath, lead, reviewer) else {
        return Ok(());
    };
    let why = match result_of(ctx, child).await? {
        Ok(outcome) => match verdict_in(outcome) {
            Ok(_) => return Ok(()),
            Err(why) => why,
        },
        // Aether could not show the result: that is no shape a reviewer can fix.
        Err(_) => return Ok(()),
    };
    assert!(!why.is_empty(), "a refusal says why");
    tell(ctx, &stage, lead, reviewer, &why).await
}

/// The live stage attempt `lead` holds, when it runs on its own task and its
/// pipeline asks it for a review; `None` otherwise.
async fn owing_stage(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
) -> Result<Option<StageRun>, ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(lead)?;
    // A resumed attempt settles on its turn's last word, so it is sent no note (§6.7).
    let Some(stage) = stage.filter(|s| !s.state.is_terminal() && s.aether_task.is_some()) else {
        return Ok(None);
    };
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(lead),
        "the stage of the lead"
    );
    let table = ctx.runs.lock().await;
    let owed = table
        .drivers
        .get(&stage.run)
        .map_or(0, |driver| driver.reviewers_owed(&stage.stage_id));
    Ok((owed > 0).then_some(stage))
}

/// `reviewer` as a direct child of `lead` delegated as a reviewer that completed its task.
fn settled_reviewer<'a>(
    beneath: &'a [Descendant],
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Option<&'a Descendant> {
    let child = beneath
        .iter()
        .find(|d| d.agent == *reviewer && d.parent == *lead)?;
    let settled = child.state == DescendantState::Completed && child.task.is_some();
    (settled && child.role.eq_ignore_ascii_case(REVIEWER_ROLE)).then_some(child)
}

/// Send `lead` the note on `reviewer`, unless it was told about it already
/// or about [`MAX_NOTES`] others. The reviewer is recorded first, so a
/// repeated event resends the keyed note, which Aether delivers once.
async fn tell(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
    why: &str,
) -> Result<(), ArchonError> {
    let mut told = load(ctx, &stage.id).await?;
    let repeat = told.iter().any(|agent| agent == reviewer.as_str());
    if !repeat && told.len() >= MAX_NOTES {
        tracing::info!(stage = %stage.id, %reviewer, "unreadable reviewer left to the gate: the lead was told {MAX_NOTES} times");
        return Ok(());
    }
    if !repeat {
        told.push(reviewer.to_string());
        save(ctx, &stage.id, &told).await?;
    }
    assert!(
        told.len() <= MAX_NOTES,
        "the notes of one attempt are bounded"
    );
    let key = format!("archon:restate:{}:{reviewer}", stage.id);
    match ctx
        .backend
        .send_note(lead, &note(reviewer, why), &key)
        .await
    {
        Ok(()) => {
            tracing::info!(stage = %stage.id, %lead, %reviewer, "lead told its reviewer's result is unreadable");
            Ok(())
        }
        Err(error) if resume::unanswered(&error) || error.is_store() => Err(error),
        // The lead ended meanwhile: its record meets the gate as before.
        Err(refusal) => {
            tracing::warn!(stage = %stage.id, %lead, %refusal, "unreadable-reviewer note not delivered");
            Ok(())
        }
    }
}

/// The note on `reviewer`: why its result does not count, and what to do instead.
fn note(reviewer: &AetherAgentId, why: &str) -> String {
    let why = why.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = format!(
        "Archon cannot count reviewer {reviewer}: {why}. A finished agent cannot be asked \
         again, so before you complete, delegate one new reviewer (role `reviewer`) for the \
         same read, and name it instead of {reviewer} in `review.reviewers`. Paste this into \
         its instructions: \"{SHAPE}\""
    );
    assert!(text.contains(SHAPE), "the note states the shape required");
    text
}

fn key(stage: &StageRunId) -> String {
    format!("review_restate:{stage}")
}

async fn load(ctx: &DaemonContext, stage: &StageRunId) -> Result<Vec<String>, ArchonError> {
    let key = key(stage);
    let Some(raw) = ctx.store.lock().await.kv_get(&key)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))
}

async fn save(ctx: &DaemonContext, stage: &StageRunId, told: &[String]) -> Result<(), ArchonError> {
    assert!(
        told.len() <= MAX_NOTES,
        "at most {MAX_NOTES} notes are recorded"
    );
    let raw = serde_json::to_string(told)?;
    ctx.store.lock().await.kv_set(&key(stage), &raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_names_the_reviewer_the_reason_and_the_shape_on_one_line() {
        let reviewer = AetherAgentId::parse("agt_01M6E466SN1MYBYNPT02P7CBAA").unwrap();
        let text = note(&reviewer, "its result carries\nno readable verdict");
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains(
            "reviewer agt_01M6E466SN1MYBYNPT02P7CBAA: its result carries no readable verdict."
        ));
        assert!(text.contains("instead of agt_01M6E466SN1MYBYNPT02P7CBAA in `review.reviewers`"));
        assert!(text.contains("\"verdict\": \"pass\" or \"fail\"") && text.contains("no YAML"));
    }

    #[test]
    fn only_a_settled_reviewer_child_of_the_lead_is_read() {
        let id = |s: &str| AetherAgentId::parse(s).unwrap();
        let (lead, other) = (id("agt_lead"), id("agt_other"));
        let child = |agent: &str, parent: &AetherAgentId, role: &str, state| Descendant {
            agent: id(agent),
            parent: parent.clone(),
            model: "a/review".parse().unwrap(),
            role: role.into(),
            effort: None,
            state,
            task: Some(archon_core::aether_ids::AetherTaskId::parse("tsk_x").unwrap()),
            deadline: None,
            cost_budget_usd: None,
        };
        let beneath = [
            child("agt_r", &lead, "Reviewer", DescendantState::Completed),
            child("agt_w", &lead, "worker", DescendantState::Completed),
            child("agt_l", &lead, "reviewer", DescendantState::Live),
            child("agt_g", &other, "reviewer", DescendantState::Completed),
        ];
        assert!(settled_reviewer(&beneath, &lead, &id("agt_r")).is_some());
        for not in ["agt_w", "agt_l", "agt_g", "agt_none"] {
            assert!(
                settled_reviewer(&beneath, &lead, &id(not)).is_none(),
                "{not}"
            );
        }
    }
}
