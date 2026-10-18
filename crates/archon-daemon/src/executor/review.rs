//! A declared review is checked against Aether and the run's branch, never
//! taken from the lead's word: each reviewer the lead names must be an agent
//! it delegated to whose own result passed the change as the branch holds it.
//! A named delta read it left unanchored may rest on a fresh lead's settled
//! earlier reviewers, which Archon finds, checks and records itself.

mod coverage;

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_pipeline::ShipRecord;
use archon_pipeline::review::{self, MAX_NAMED_REVIEWERS, Reviewer, Verdict};

use super::recover;
use crate::context::DaemonContext;
use coverage::{Base, Held, History};

/// Agents named in a refusal: enough to see what the lead delegated to.
const MAX_LISTED: usize = 6;

/// A review Archon found holds: the named reviewers that passed, and the
/// earlier reviewers it added to anchor their delta reads.
#[derive(Debug)]
struct Found {
    reviewers: Vec<Reviewer>,
    implicit: Vec<String>,
}

/// What Archon puts on the record: the review it found, or why not.
type Finding = Result<Found, String>;

/// A reviewer's agent id and the verdict its task returned.
type Read = (String, Verdict);

/// What checking the verdicts against the branch came to.
enum Checked {
    Held(Held),
    Refused(String),
    /// A named delta read needs an anchor, and the earlier reviewers were not
    /// read: why it is refused if none anchors it.
    Unanchored(String),
}

/// What a settling stage's result owes besides its gate.
#[derive(Debug, Clone, Copy)]
pub(super) struct Owed {
    /// Reviewers this item's review level asks of the stage; 0: none.
    pub reviewers: u32,
    /// Whether the result is also the ship record archond delivers.
    pub records_ship: bool,
}

/// `outcome` with Archon's own finding on the review pass it claims, for a
/// `stage` owing `owed`. `Err` while Aether cannot be asked.
pub(super) async fn verify(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    owed: Owed,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    assert_eq!(
        stage.run, run.id,
        "a stage's review is checked with its own run"
    );
    let StageOutcome::Completed {
        summary,
        structured: Some(mut structured),
    } = outcome
    else {
        return Ok(outcome);
    };
    // A missing ship record binds nothing: the stage's gate refuses it.
    let record = owed
        .records_ship
        .then(|| ShipRecord::read(Some(&structured)));
    let shipped = record.and_then(Result::ok).map(|record| record.tree);
    let found = match review::claimed_reviewers(&structured) {
        Some(named) if owed.reviewers > 0 => {
            let owed = owed.reviewers;
            Some(reviewers(ctx, run, stage, &named, owed, shipped).await?)
        }
        _ => None,
    };
    let implicit = match &found {
        Some(Ok(found)) => found.implicit.clone(),
        _ => Vec::new(),
    };
    if !implicit.is_empty() {
        tracing::info!(stage = %stage.id, ?implicit, "anchored named delta reviews on the lead's earlier reviewers");
    }
    review::record_check(&mut structured, found.map(|f| f.map(|f| f.reviewers)));
    review::record_implicit(&mut structured, &implicit);
    Ok(StageOutcome::Completed {
        summary,
        structured: Some(structured),
    })
}

/// The `named` agents that passed `stage`'s change as the branch holds it, or why they
/// are not the `owed` reviewers or the branch does not hold the `shipped` tree.
async fn reviewers(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    named: &[String],
    owed: u32,
    shipped: Option<String>,
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
    let verdicts = match verdicts_of(ctx, &picked).await? {
        Ok(verdicts) => verdicts,
        Err(why) => return Ok(Err(why)),
    };
    assert!(
        picked.len() >= owed as usize,
        "the review level's reviewers were picked"
    );
    let check = |earlier: Option<Vec<Read>>| {
        holding(ctx, run, verdicts.clone(), earlier, owed, shipped.clone())
    };
    let checked = match check(None).await? {
        Checked::Unanchored(stray) => {
            match earlier_reads(ctx, stage, lead, &beneath, &picked).await? {
                Ok(earlier) => check(Some(earlier)).await?,
                Err(why) => Checked::Refused(format!("{stray}: {why}")),
            }
        }
        checked => checked,
    };
    Ok(match checked {
        Checked::Held(held) => Ok(found(&picked, held)),
        Checked::Refused(why) => Err(why),
        Checked::Unanchored(_) => unreachable!("the earlier reviewers were read"),
    })
}

/// The verdicts of `stage`'s `lead`'s earlier reviewers that may anchor the
/// `picked` ones' delta reads, or why Archon does not anchor on them.
async fn earlier_reads(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    beneath: &[Descendant],
    picked: &[&Descendant],
) -> Result<Result<Vec<Read>, String>, ArchonError> {
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(lead),
        "the stage's own lead"
    );
    if ran_another_attempt(ctx, stage).await? {
        return Ok(Err(
            "the lead's agent also ran another attempt of this run, so Archon cannot tell \
             which earlier reviewers are this attempt's: name the one that read the change up to it"
                .into(),
        ));
    }
    let earlier = match earlier_reviewers(lead, beneath, picked) {
        Ok(earlier) => earlier,
        Err(why) => return Ok(Err(why)),
    };
    earlier_verdicts(ctx, &earlier).await
}

/// The `picked` reviewers whose passes `held` counts, and the earlier ones it added.
fn found(picked: &[&Descendant], held: Held) -> Found {
    assert!(!held.passes.is_empty(), "a review that holds has a pass");
    assert!(
        held.passes.iter().all(|i| *i < picked.len()),
        "only a named pass counts, by its index among those picked"
    );
    let reviewers = held.passes.into_iter().map(|i| Reviewer {
        model: picked[i].model.to_string(),
        agent: picked[i].agent.to_string(),
    });
    Found {
        reviewers: reviewers.collect(),
        implicit: held.implicit,
    }
}

/// The verdict of each `picked` reviewer, in order, or why one is none.
async fn verdicts_of(
    ctx: &DaemonContext,
    picked: &[&Descendant],
) -> Result<Result<Vec<Read>, String>, ArchonError> {
    let mut verdicts = Vec::with_capacity(picked.len());
    for child in picked {
        match result_of(ctx, child).await?.and_then(verdict_in) {
            Ok(verdict) => verdicts.push((child.agent.to_string(), verdict)),
            Err(why) => return Ok(Err(format!("reviewer {} {why}", child.agent))),
        }
    }
    // `holding` counts passes by index, which must name the picked agent.
    assert_eq!(
        verdicts.len(),
        picked.len(),
        "a verdict for every reviewer picked"
    );
    Ok(Ok(verdicts))
}

/// The verdicts of the `earlier` reviewers that returned one, or why one's
/// result is unknown. A result that is no verdict anchors nothing: it is left out.
async fn earlier_verdicts(
    ctx: &DaemonContext,
    earlier: &[&Descendant],
) -> Result<Result<Vec<Read>, String>, ArchonError> {
    let mut verdicts = Vec::with_capacity(earlier.len());
    for child in earlier {
        // A fail Aether could not show may be the one that stands.
        let outcome = match result_of(ctx, child).await? {
            Ok(outcome) => outcome,
            Err(why) => return Ok(Err(format!("earlier reviewer {} {why}", child.agent))),
        };
        match verdict_in(outcome) {
            Ok(verdict) => verdicts.push((child.agent.to_string(), verdict)),
            Err(why) => {
                tracing::debug!(agent = %child.agent, %why, "an earlier reviewer anchors nothing")
            }
        }
    }
    assert!(
        verdicts.len() <= earlier.len(),
        "one verdict per reviewer at most"
    );
    Ok(Ok(verdicts))
}

/// Whether `stage`'s lead also ran another attempt of its run: what it
/// delegated then cannot be told from what it delegated in this one.
async fn ran_another_attempt(ctx: &DaemonContext, stage: &StageRun) -> Result<bool, ArchonError> {
    let runs = ctx.store.lock().await.stage_runs(&stage.run)?;
    Ok(shares_agent(&runs, stage))
}

/// Whether another of the run's stage `runs` names `stage`'s agent.
fn shares_agent(runs: &[StageRun], stage: &StageRun) -> bool {
    let lead = stage
        .aether_agent
        .as_ref()
        .expect("a checked stage has its agent");
    assert!(runs.iter().all(|s| s.run == stage.run), "one run's stages");
    assert!(
        runs.iter().any(|s| s.id == stage.id),
        "a stage is checked within its own run"
    );
    runs.iter()
        .any(|s| s.id != stage.id && s.aether_agent.as_ref() == Some(lead))
}

/// The lead's direct children, other than the `picked` ones, that completed
/// a task, or why none is read: one still running may yet fail the head.
fn earlier_reviewers<'a>(
    lead: &AetherAgentId,
    beneath: &'a [Descendant],
    picked: &[&Descendant],
) -> Result<Vec<&'a Descendant>, String> {
    assert!(!picked.is_empty(), "earlier reviewers anchor named ones");
    let children = || beneath.iter().filter(|d| d.parent == *lead);
    let running: Vec<&Descendant> = children()
        .filter(|d| d.state == DescendantState::Live)
        .collect();
    if !running.is_empty() {
        return Err(format!(
            "Archon anchors on earlier reviewers only once none still runs, and {}: \
             wait for each to finish, or cancel it, before completing",
            listed(&running)
        ));
    }
    let earlier: Vec<&Descendant> = children()
        .filter(|d| d.state == DescendantState::Completed && d.task.is_some())
        .filter(|d| picked.iter().all(|p| p.agent != d.agent))
        .collect();
    assert!(
        earlier.iter().all(|d| d.agent != *lead),
        "a lead is not its own reviewer"
    );
    Ok(earlier)
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

/// What `reviewer`'s task returned, or why Aether could not show it.
async fn result_of(
    ctx: &DaemonContext,
    reviewer: &Descendant,
) -> Result<Result<StageOutcome, String>, ArchonError> {
    let task = reviewer.task.as_ref().expect("picked with a task");
    match ctx.backend.task_outcome(task).await {
        Ok(Some(outcome)) => Ok(Ok(outcome)),
        // Aether shows the agent completed before its task settles.
        Ok(None) => Err(ArchonError::Unavailable(format!(
            "reviewer {}'s task {task} has not settled in Aether",
            reviewer.agent
        ))),
        Err(error) if transient(&error) => Err(error),
        Err(error) => Ok(Err(format!("has a result Aether could not show: {error}"))),
    }
}

/// The verdict a reviewer's task `outcome` returned, or what keeps it from being one.
fn verdict_in(outcome: StageOutcome) -> Result<Verdict, String> {
    match outcome {
        StageOutcome::Completed {
            summary,
            structured,
        } => Verdict::read_result(&summary, structured.as_ref()),
        _ => Err("did not complete its task".into()),
    }
}

/// Whether the named `verdicts` hold for the run's branch as it stands, which
/// must hold the `shipped` tree if one is given. `earlier` reads, once read,
/// may anchor named delta reads; `None` asks first whether any is needed.
async fn holding(
    ctx: &DaemonContext,
    run: &PipelineRun,
    verdicts: Vec<Read>,
    earlier: Option<Vec<Read>>,
    owed: u32,
    shipped: Option<String>,
) -> Result<Checked, ArchonError> {
    assert!(!verdicts.is_empty(), "a named reviewer was read");
    let (base_branch, onto_head) = base_of(ctx, run).await?;
    let workspace = run.workspace.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let mut history = History::new(&workspace);
        let read = earlier.as_deref().unwrap_or_default();
        let base = Base {
            branch: &base_branch,
            onto_head: onto_head.as_deref(),
        };
        let placed = coverage::place(&mut history, base, &verdicts, read)?;
        let (head, placed) = match placed {
            Ok(placed) => placed,
            Err(why) => return Ok(Checked::Refused(why)),
        };
        if let Some(tree) = &shipped
            && let Err(why) = coverage::holds_tree(&workspace, &head, tree)?
        {
            return Ok(Checked::Refused(why));
        }
        if earlier.is_none()
            && let Some(stray) = coverage::unanchored_named(&mut history, &placed)?
        {
            return Ok(Checked::Unanchored(stray));
        }
        Ok(
            match coverage::counted(&mut history, &placed, &head, owed)? {
                Ok(held) => Checked::Held(held),
                Err(why) => Checked::Refused(why),
            },
        )
    });
    checked
        .await
        .map_err(|e| ArchonError::io("reviewed commits task", std::io::Error::other(e)))?
}

/// The branch `run` forked from, empty when its variables name none, and the
/// frozen head of the pull request its item is queued `onto`.
async fn base_of(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<(String, Option<String>), ArchonError> {
    let store = ctx.store.lock().await;
    let onto_head = store.item(&run.item)?.onto.map(|onto| onto.head_sha);
    match recover::load_run_vars(&store, &run.id) {
        Ok(vars) => Ok((
            vars.get("base_branch").cloned().unwrap_or_default(),
            onto_head,
        )),
        // Without a base only reviews stating no base read the whole change.
        Err(ArchonError::NotFound(_)) => Ok((String::new(), onto_head)),
        Err(error) => Err(error),
    }
}

/// Why `name` is not an agent the lead delegated to, listing those it did.
fn missing(children: &[&Descendant], name: &str) -> String {
    if children.is_empty() {
        return format!("it names reviewer {name}, but the lead delegated to no agent");
    }
    format!(
        "it names reviewer {name}, which is not an agent the lead delegated to ({})",
        listed(children)
    )
}

/// The first `children` as a refusal names them, and how many more there are.
fn listed(children: &[&Descendant]) -> String {
    let mut listed: Vec<String> = children.iter().take(MAX_LISTED).map(|d| held(d)).collect();
    if children.len() > MAX_LISTED {
        listed.push(format!("{} more", children.len() - MAX_LISTED));
    }
    listed.join("; ")
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
    use archon_core::id::PipelineRunId;
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

    /// Attempt `n` of `run`'s `build` stage, run by `agent_id`.
    fn attempt(run: &PipelineRunId, n: u32, agent_id: &str) -> StageRun {
        StageRun {
            id: archon_core::id::StageRunId::new(),
            run: run.clone(),
            stage_id: "build".into(),
            kind: None,
            attempt: n,
            fan_index: 0,
            choice: archon_core::model::ModelChoice {
                model: "a/work".parse().unwrap(),
                effort: archon_core::model::Effort::Low,
                tier: None,
            },
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            aether_agent: Some(agent(agent_id)),
            aether_task: None,
            state: archon_core::run::StageState::Running,
            verdict: None,
            summary: None,
            structured: None,
            cost_usd: 0.0,
            started_at: archon_core::now(),
            finished_at: None,
        }
    }

    /// A kept agent's earlier attempt delegated too, so no earlier reviewer
    /// of its can be told to be this attempt's.
    #[test]
    fn a_lead_that_ran_another_attempt_shares_its_agent() {
        let run = PipelineRunId::new();
        let earlier = attempt(&run, 1, "agt_lead");
        let kept = attempt(&run, 2, "agt_lead");
        let fresh = attempt(&run, 2, "agt_fresh");
        let runs = [earlier, fresh.clone(), kept.clone()];
        assert!(shares_agent(&runs, &kept));
        assert!(!shares_agent(&runs, &fresh));
        assert!(
            !shares_agent(std::slice::from_ref(&kept), &kept),
            "alone in its run"
        );
    }

    /// Only what this lead delegated and completed may anchor, and nothing
    /// does while one of its reviewers still runs.
    #[test]
    fn earlier_reviewers_are_the_lead_s_settled_children() {
        use DescendantState::{Completed, Live, Stopped};
        let mut beneath = vec![
            child("agt_named", "agt_lead", Completed),
            child("agt_whole", "agt_lead", Completed),
            child("agt_cancelled", "agt_lead", Stopped),
            child("agt_nested", "agt_whole", Completed),
            child("agt_nested_running", "agt_whole", Live),
        ];
        let lead = agent("agt_lead");
        let named = beneath[0].clone();
        let earlier = |beneath: &[Descendant]| -> Result<Vec<String>, String> {
            let found = earlier_reviewers(&lead, beneath, &[&named])?;
            Ok(found.iter().map(|d| d.agent.to_string()).collect())
        };
        assert_eq!(earlier(&beneath).unwrap(), ["agt_whole"]);
        let stranger = agent("agt_other_lead");
        let found = earlier_reviewers(&stranger, &beneath, &[&named]).unwrap();
        assert!(found.is_empty(), "{found:?}");
        beneath.push(child("agt_running", "agt_lead", Live));
        let running = earlier(&beneath).unwrap_err();
        assert!(
            running.contains("agt_running on a/review still running"),
            "{running}"
        );
        assert!(running.contains("or cancel it"), "{running}");
    }
}
