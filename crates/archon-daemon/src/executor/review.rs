//! A declared review is checked against Aether and the run's branch, never
//! taken from the lead's word: each reviewer the lead names must be an agent
//! it delegated to whose own result passed the change as the branch holds it.

mod coverage;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_pipeline::review::{self, MAX_NAMED_REVIEWERS, Reviewer, Verdict};

use super::recover;
use crate::context::DaemonContext;
use coverage::History;

/// Agents named in a refusal: enough to see what the lead delegated to.
const MAX_LISTED: usize = 6;

/// What Archon puts on the record: the reviewers it found, or why not.
type Finding = Result<Vec<Reviewer>, String>;

/// `outcome` with Archon's own finding on the review pass it claims, for a
/// `stage` owing `owed` reviewers (0: none). `Err` while Aether cannot be asked.
pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
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
    let found = match review::claimed_reviewers(&structured) {
        Some(named) if owed > 0 => Some(reviewers(ctx, run, stage, &named, owed).await?),
        _ => None,
    };
    review::record_check(&mut structured, found);
    Ok(StageOutcome::Completed {
        summary,
        structured: Some(structured),
    })
}

/// The reviewers among the `named` agents that passed `stage`'s change as
/// the run's branch holds it, or why they do not make the `owed` reviewers.
async fn reviewers(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    named: &[String],
    owed: u32,
) -> Result<Finding, ArchonError> {
    let lead = stage.aether_agent.as_ref().ok_or_else(|| {
        ArchonError::Protocol(format!("completed stage {} has no agent", stage.id))
    })?;
    let beneath = match ctx.backend.descendants(lead).await {
        Ok(beneath) => beneath,
        // Left live: the pump's reconciliation settles and checks it again.
        Err(error) if transient(&error) => return Err(error),
        Err(error) => {
            return Ok(Err(format!(
                "Aether could not list the agents the lead delegated to: {error}"
            )));
        }
    };
    let picked = match pick_named(lead, &beneath, named, owed) {
        Ok(picked) => picked,
        Err(why) => return Ok(Err(why)),
    };
    let mut verdicts = Vec::with_capacity(picked.len());
    for child in &picked {
        match verdict_of(ctx, child).await? {
            Ok(verdict) => verdicts.push((child.agent.to_string(), verdict)),
            Err(why) => return Ok(Err(format!("reviewer {} {why}", child.agent))),
        }
    }
    let counted = holding(ctx, run, verdicts, owed).await?;
    Ok(counted.map(|passes| {
        let found = passes.into_iter().map(|i| picked[i]);
        found
            .map(|d| Reviewer {
                model: d.model.to_string(),
                agent: d.agent.to_string(),
            })
            .collect()
    }))
}

/// An Aether failure the pump retries, not a verdict on the review.
fn transient(error: &ArchonError) -> bool {
    matches!(
        error,
        ArchonError::Unavailable(_) | ArchonError::Protocol(_) | ArchonError::Io { .. }
    )
}

/// The `named` agents as distinct direct children of `lead` that completed
/// a task, or why one is not.
fn pick_named<'a>(
    lead: &AetherAgentId,
    beneath: &'a [Descendant],
    named: &[String],
    owed: u32,
) -> Result<Vec<&'a Descendant>, String> {
    assert!(owed >= 1, "only an owed review is checked");
    if named.is_empty() {
        return Err("`review.reviewers` names no reviewer agent".into());
    }
    if named.len() < owed as usize || named.len() > MAX_NAMED_REVIEWERS {
        return Err(format!(
            "it names {} reviewer(s), and this item's review level needs {owed} \
             (at most {MAX_NAMED_REVIEWERS}), each a different agent",
            named.len()
        ));
    }
    let children: Vec<&Descendant> = beneath.iter().filter(|d| d.parent == *lead).collect();
    let mut picked: Vec<&Descendant> = Vec::with_capacity(named.len());
    for name in named {
        if picked.iter().any(|p| p.agent.as_str() == name) {
            return Err(format!("it names reviewer {name} twice"));
        }
        let Some(child) = children.iter().find(|d| d.agent.as_str() == name) else {
            return Err(missing(&children, name));
        };
        if child.state != DescendantState::Completed || child.task.is_none() {
            return Err(format!("it names {}", held(child)));
        }
        assert_ne!(child.agent, *lead, "a lead is not its own reviewer");
        picked.push(child);
    }
    assert_eq!(picked.len(), named.len(), "one agent per name");
    Ok(picked)
}

/// The verdict `reviewer`'s task returned, or what keeps it from being one.
async fn verdict_of(
    ctx: &DaemonContext,
    reviewer: &Descendant,
) -> Result<Result<Verdict, String>, ArchonError> {
    let task = reviewer.task.as_ref().expect("picked with a task");
    match ctx.backend.task_outcome(task).await {
        Ok(Some(StageOutcome::Completed { structured, .. })) => {
            Ok(Verdict::read(structured.as_ref()))
        }
        Ok(Some(_)) => Ok(Err("did not complete its task".into())),
        // Aether shows the agent completed before its task settles.
        Ok(None) => Err(ArchonError::Unavailable(format!(
            "reviewer {}'s task {task} has not settled in Aether",
            reviewer.agent
        ))),
        Err(error) if transient(&error) => Err(error),
        Err(error) => Ok(Err(format!("has a result Aether could not show: {error}"))),
    }
}

/// Indexes of the `verdicts` that hold for the run's branch as it stands.
async fn holding(
    ctx: &DaemonContext,
    run: &PipelineRun,
    verdicts: Vec<(String, Verdict)>,
    owed: u32,
) -> Result<Result<Vec<usize>, String>, ArchonError> {
    assert!(!verdicts.is_empty(), "a named reviewer was read");
    let base_branch = {
        let store = ctx.store.lock().await;
        match recover::load_run_vars(&store, &run.id) {
            Ok(vars) => vars.get("base_branch").cloned().unwrap_or_default(),
            // Without a base only reviews stating no base read the whole change.
            Err(ArchonError::NotFound(_)) => String::new(),
            Err(error) => return Err(error),
        }
    };
    let workspace = run.workspace.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let mut history = History::new(&workspace);
        let (head, placed) = match coverage::place(&mut history, &base_branch, &verdicts)? {
            Ok(placed) => placed,
            Err(why) => return Ok(Err(why)),
        };
        coverage::counted(&mut history, &placed, &head, owed)
    });
    let counted = checked
        .await
        .map_err(|e| ArchonError::io("reviewed commits task", std::io::Error::other(e)))??;
    Ok(counted)
}

/// Why `name` is not an agent the lead delegated to, listing those it did.
fn missing(children: &[&Descendant], name: &str) -> String {
    if children.is_empty() {
        return format!("it names reviewer {name}, but the lead delegated to no agent");
    }
    let mut listed: Vec<String> = children.iter().take(MAX_LISTED).map(|d| held(d)).collect();
    if children.len() > MAX_LISTED {
        listed.push(format!("{} more", children.len() - MAX_LISTED));
    }
    format!(
        "it names reviewer {name}, which is not an agent the lead delegated to ({})",
        listed.join("; ")
    )
}

/// One delegated agent as a refusal names it.
fn held(child: &Descendant) -> String {
    let how = match child.state {
        DescendantState::Live => "still running",
        DescendantState::Completed if child.task.is_none() => "completed without a task",
        DescendantState::Completed => "completed",
        DescendantState::Stopped => "ended without completing",
    };
    format!("{} on {} {how}", child.agent, child.model)
}

#[cfg(test)]
mod tests {
    use archon_core::aether_ids::AetherTaskId;
    use archon_core::model::ModelRef;

    use super::*;

    fn agent(id: &str) -> AetherAgentId {
        AetherAgentId::parse(id).unwrap()
    }

    fn child(id: &str, parent: &str, state: DescendantState) -> Descendant {
        Descendant {
            agent: agent(id),
            parent: agent(parent),
            model: "a/review".parse::<ModelRef>().unwrap(),
            effort: None,
            state,
            task: Some(AetherTaskId::parse(&id.replace("agt_", "tsk_")).unwrap()),
        }
    }

    fn pick(beneath: &[Descendant], named: &[&str], owed: u32) -> Result<Vec<String>, String> {
        let named: Vec<String> = named.iter().map(|m| (*m).to_owned()).collect();
        let picked = pick_named(&agent("agt_lead"), beneath, &named, owed)?;
        Ok(picked.iter().map(|d| d.agent.to_string()).collect())
    }

    #[test]
    fn a_named_reviewer_is_a_direct_child_of_the_lead_that_completed_its_task() {
        use DescendantState::{Completed, Live, Stopped};
        let beneath = [
            child("agt_c1", "agt_lead", Completed),
            child("agt_c2", "agt_lead", Stopped),
            child("agt_c3", "agt_c1", Completed),
            child("agt_c4", "agt_lead", Live),
        ];
        assert_eq!(pick(&beneath, &["agt_c1"], 1).unwrap(), ["agt_c1"]);
        let stopped = pick(&beneath, &["agt_c2"], 1).unwrap_err();
        assert!(stopped.contains("ended without completing"), "{stopped}");
        let live = pick(&beneath, &["agt_c4"], 1).unwrap_err();
        assert!(live.contains("still running"), "{live}");
        let grandchild = pick(&beneath, &["agt_c3"], 1).unwrap_err();
        assert!(grandchild.contains("not an agent the lead delegated to"));
        assert!(!grandchild.contains("agt_c3 on"), "{grandchild}");
        let none = pick(&[], &["agt_c1"], 1).unwrap_err();
        assert!(none.contains("delegated to no agent"), "{none}");
    }

    /// A strict review is several agents, so one reviewer named twice or a
    /// record shorter than the level asks for is not it.
    #[test]
    fn several_reviewers_are_different_agents_and_as_many_as_the_level_owes() {
        use DescendantState::Completed;
        let beneath = [
            child("agt_c1", "agt_lead", Completed),
            child("agt_c2", "agt_lead", Completed),
            child("agt_c3", "agt_lead", Completed),
        ];
        let three = ["agt_c3", "agt_c1", "agt_c2"];
        assert_eq!(pick(&beneath, &three, 3).unwrap(), three);
        let short = pick(&beneath, &three[..2], 3).unwrap_err();
        assert!(short.contains("names 2 reviewer(s)") && short.contains("needs 3"));
        let twice = pick(&beneath, &["agt_c1", "agt_c2", "agt_c1"], 3).unwrap_err();
        assert!(twice.contains("agt_c1 twice"), "{twice}");
        let unnamed = pick(&beneath, &[], 1).unwrap_err();
        assert!(unnamed.contains("`review.reviewers`"), "{unnamed}");
        let many = vec!["agt_c1"; MAX_NAMED_REVIEWERS + 1];
        assert!(pick(&beneath, &many, 1).unwrap_err().contains("at most"));
    }
}
