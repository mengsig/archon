//! A lead that fixes and re-reviews round after round is stopped. A round is
//! a commit some reviewer of the lead failed with a blocking finding;
//! [`REVIEW_ROUND_CAP`] of them end the stage attempt with an escalation to the
//! orchestrator that lists the open blocking findings, instead of another round.
//! Each re-review is given the open findings to verify, so the last round's
//! blocking findings are the open ones; earlier rounds' are listed apart.
//! Every count rereads the attempt's settled children it has not read, so a
//! reviewer whose event was missed still counts, at the next event or the
//! lead's completion; a resumed lead's attempt holds the children it gained since.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::{StageOutcome, StageRun};
use archon_pipeline::contract::REVIEW_ROUND_CAP;
use archon_pipeline::review::Verdict;
use serde::{Deserialize, Serialize};

use super::{result_of, verdict_in};
use crate::context::DaemonContext;

/// Finding lines an escalation lists before it points to the reviewers' results.
const MAX_LISTED: usize = 20;

/// A stage whose review failed [`REVIEW_ROUND_CAP`] rounds: archond stops it,
/// or settles its completion, with `reason`, an `ESCALATE:` failure that ends its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::executor) struct Capped {
    pub stage: StageRunId,
    pub reason: String,
}

/// Count the blocking fails of the settled children of `lead`'s stage
/// attempt, `reviewer` among them; the stage to stop once the tally reaches
/// the cap. An Aether answer that fails is returned, so the pump repeats the event.
pub(in crate::executor) async fn reviewer_settled(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Result<Option<Capped>, ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(lead)?;
    let Some(stage) = stage.filter(|s| !s.state.is_terminal()) else {
        return Ok(None);
    };
    assert_eq!(
        stage.aether_agent.as_ref(),
        Some(lead),
        "the stage of the lead"
    );
    count(ctx, &stage, lead, Some(reviewer)).await
}

/// The cap a stage completing now reached, counting every child of its
/// attempt whose event was missed. A result not readable yet is returned as
/// the error, so the settle is retried rather than completing past it.
pub(in crate::executor) async fn capped_at_completion(
    ctx: &DaemonContext,
    stage: &StageRun,
) -> Result<Option<Capped>, ArchonError> {
    let Some(lead) = &stage.aether_agent else {
        return Ok(None);
    };
    assert!(!stage.state.is_terminal(), "only a live stage completes");
    count(ctx, stage, lead, None).await
}

/// Record the children `lead` holds as `stage` resumes it: they are earlier
/// attempts', and every later one is this attempt's. Recorded once, before
/// the lead is told to go on, so a repeated hand-over keeps the first list.
pub(in crate::executor) async fn attempt_resumed(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
) -> Result<(), ArchonError> {
    let mut tally = load(ctx, &stage.id).await?;
    if tally.earlier.is_some() {
        return Ok(());
    }
    assert!(tally.fails.is_empty(), "an attempt counts after it resumes");
    let beneath = ctx.backend.descendants(lead).await?;
    let earlier: Vec<String> = beneath
        .iter()
        .filter(|d| d.parent == *lead)
        .map(|d| d.agent.to_string())
        .collect();
    tally.earlier = Some(earlier);
    save(ctx, &stage.id, &tally).await
}

/// Add the blocking fails of the attempt's settled children not read before
/// to its tally, in the order Aether created them; the cap, once reached.
/// `reviewer` is the child an event names, `None` at the lead's completion.
async fn count(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
    reviewer: Option<&AetherAgentId>,
) -> Result<Option<Capped>, ArchonError> {
    let fresh = lead_is_fresh(ctx, stage, lead).await?;
    let beneath = ctx.backend.descendants(lead).await?;
    let mut tally = load(ctx, &stage.id).await?;
    let unread: Vec<&Descendant> = settled_children(&beneath, lead)
        .into_iter()
        .filter(|c| tally.attempt_holds(&c.agent, fresh, reviewer))
        .filter(|c| !tally.has_read(&c.agent))
        .collect();
    if !unread.is_empty() {
        for child in unread {
            // Unreadable yet: no child is skipped, so the event or settle is retried.
            let verdict = verdict_of(ctx, child).await?;
            tally.read.push(child.agent.to_string());
            if let Some(verdict) = verdict.filter(blocks) {
                assert!(
                    !verdict.blocking_findings.is_empty(),
                    "a counted round names its blocking findings"
                );
                tally.fails.push(Fail {
                    agent: child.agent.to_string(),
                    head: verdict.head,
                    findings: verdict.blocking_findings,
                });
            }
        }
        in_creation_order(&mut tally.fails, &beneath, lead);
        save(ctx, &stage.id, &tally).await?;
    }
    // A replayed reviewer adds no round, but may still owe the stop.
    let rounds = rounds_of(&tally.fails);
    if rounds < REVIEW_ROUND_CAP {
        return Ok(None);
    }
    let reason = escalation(stage, rounds, &Open::of(&tally.fails));
    assert!(StageOutcome::ends_run(&reason), "a capped review escalates");
    Ok(Some(Capped {
        stage: stage.id.clone(),
        reason,
    }))
}

/// Only a fail that lists a blocking finding is a round; the gate still
/// refuses any fail left unanswered.
fn blocks(verdict: &Verdict) -> bool {
    !verdict.pass && verdict.blocking.is_some_and(|n| n > 0)
}

/// Whether no earlier attempt of `stage` ran on `lead`.
async fn lead_is_fresh(
    ctx: &DaemonContext,
    stage: &StageRun,
    lead: &AetherAgentId,
) -> Result<bool, ArchonError> {
    let runs = ctx.store.lock().await.stage_runs(&stage.run)?;
    assert!(
        runs.iter().any(|s| s.id == stage.id),
        "the run lists its stage"
    );
    let reused = runs.iter().any(|s| {
        s.id != stage.id && s.stage_id == stage.stage_id && s.aether_agent.as_ref() == Some(lead)
    });
    Ok(!reused)
}

/// The direct children of `lead` that completed a task, in the order Aether lists them.
fn settled_children<'a>(beneath: &'a [Descendant], lead: &AetherAgentId) -> Vec<&'a Descendant> {
    beneath
        .iter()
        .filter(|d| d.parent == *lead && d.state == DescendantState::Completed)
        .filter(|d| d.task.is_some())
        .collect()
}

/// Sort `fails` into the order Aether lists `lead`'s children, which is the
/// order it created them, so a fail read late never passes for the latest round.
fn in_creation_order(fails: &mut [Fail], beneath: &[Descendant], lead: &AetherAgentId) {
    let listed: Vec<&str> = beneath
        .iter()
        .filter(|d| d.parent == *lead)
        .map(|d| d.agent.as_str())
        .collect();
    let place = |fail: &Fail| listed.iter().position(|a| *a == fail.agent);
    if fails.iter().any(|f| place(f).is_none()) {
        // Aether forgot a child counted before: the order read stands.
        tracing::warn!(%lead, "a counted reviewer is no longer listed; rounds keep their read order");
        return;
    }
    fails.sort_by_key(|f| place(f));
    assert!(
        fails.windows(2).all(|w| w[0].agent != w[1].agent),
        "each reviewer counts once"
    );
}

/// `child`'s verdict; `None` for a result that is no verdict, which counts for nothing.
async fn verdict_of(
    ctx: &DaemonContext,
    child: &Descendant,
) -> Result<Option<Verdict>, ArchonError> {
    let outcome = match result_of(ctx, child).await? {
        Ok(outcome) => outcome,
        Err(why) => {
            tracing::debug!(agent = %child.agent, %why, "a reviewer's result counts for no round");
            return Ok(None);
        }
    };
    Ok(verdict_in(outcome).ok())
}

/// A failing verdict one reviewer returned in a stage attempt.
#[derive(Debug, Serialize, Deserialize)]
struct Fail {
    agent: String,
    head: String,
    /// Its blocking findings, one line each; tallies written before they were
    /// described kept the titles alone.
    #[serde(alias = "titles")]
    findings: Vec<String>,
}

/// The failing verdicts of one stage attempt, and every child read for one.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Tally {
    fails: Vec<Fail>,
    /// Tallies written before children were rescanned kept fails alone.
    #[serde(default)]
    read: Vec<String>,
    /// For an attempt that resumed its lead, the children the lead held then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    earlier: Option<Vec<String>>,
}

impl Tally {
    fn has_read(&self, agent: &AetherAgentId) -> bool {
        let agent = agent.as_str();
        self.read.iter().any(|a| a == agent) || self.fails.iter().any(|f| f.agent == agent)
    }

    /// Whether child `agent` belongs to this attempt. A lead resumed before
    /// its children were recorded counts only the reviewer an event names.
    fn attempt_holds(
        &self,
        agent: &AetherAgentId,
        fresh: bool,
        reviewer: Option<&AetherAgentId>,
    ) -> bool {
        match (fresh, &self.earlier) {
            (true, _) => true,
            (false, Some(earlier)) => !earlier.iter().any(|a| a == agent.as_str()),
            (false, None) => reviewer == Some(agent),
        }
    }
}

fn tally_key(stage: &StageRunId) -> String {
    format!("review_rounds:{stage}")
}

async fn load(ctx: &DaemonContext, stage: &StageRunId) -> Result<Tally, ArchonError> {
    let key = tally_key(stage);
    let Some(raw) = ctx.store.lock().await.kv_get(&key)? else {
        return Ok(Tally::default());
    };
    serde_json::from_str(&raw).map_err(|e| ArchonError::Corrupt(format!("kv {key} = {raw:?}: {e}")))
}

async fn save(ctx: &DaemonContext, stage: &StageRunId, tally: &Tally) -> Result<(), ArchonError> {
    let raw = serde_json::to_string(tally)?;
    ctx.store.lock().await.kv_set(&tally_key(stage), &raw)
}

/// Whether two commit ids, each possibly abbreviated, can name one commit.
fn same_commit(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn rounds_of(fails: &[Fail]) -> usize {
    let mut heads: Vec<&str> = Vec::new();
    for fail in fails {
        if !heads.iter().any(|head| same_commit(head, &fail.head)) {
            heads.push(&fail.head);
        }
    }
    heads.len()
}

/// The blocking findings an escalation reports: the last round's, which are
/// open, and those only earlier rounds listed, which its reviewers verified.
#[derive(Debug, PartialEq, Eq)]
struct Open<'a> {
    latest: Vec<&'a str>,
    earlier: Vec<&'a str>,
    /// The reviewers whose results hold every finding in full.
    reviewers: Vec<&'a str>,
}

impl<'a> Open<'a> {
    fn of(fails: &'a [Fail]) -> Self {
        let Some(last) = fails.last() else {
            return Self {
                latest: Vec::new(),
                earlier: Vec::new(),
                reviewers: Vec::new(),
            };
        };
        let (latest_fails, earlier_fails): (Vec<&Fail>, Vec<&Fail>) =
            fails.iter().partition(|f| same_commit(&f.head, &last.head));
        let latest = distinct(latest_fails.iter().flat_map(|f| &f.findings), &[]);
        let earlier = distinct(earlier_fails.iter().flat_map(|f| &f.findings), &latest);
        let reviewers = fails.iter().map(|f| f.agent.as_str()).collect();
        assert!(
            !latest_fails.is_empty(),
            "the last fail is in the latest round"
        );
        assert!(
            earlier.iter().all(|e| !latest.contains(e)),
            "each finding listed once"
        );
        Self {
            latest,
            earlier,
            reviewers,
        }
    }
}

/// The lines of `findings` in order, each once and none of `skip`.
fn distinct<'a>(findings: impl Iterator<Item = &'a String>, skip: &[&str]) -> Vec<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for line in findings {
        if !seen.contains(&line.as_str()) && !skip.contains(&line.as_str()) {
            seen.push(line);
        }
    }
    seen
}

/// The failure a capped stage settles with: its rounds and the open blocking findings.
fn escalation(stage: &StageRun, rounds: usize, open: &Open) -> String {
    assert!(rounds >= REVIEW_ROUND_CAP, "only a capped review escalates");
    assert!(
        !open.reviewers.is_empty(),
        "a capped review had failing reviewers"
    );
    let latest = match open.latest.as_slice() {
        [] => "The last round's reviewers described no blocking finding.".to_owned(),
        lines => format!(
            "Open blocking findings, from the last round: {}.",
            listed(lines, MAX_LISTED)
        ),
    };
    let room = MAX_LISTED.saturating_sub(open.latest.len());
    let earlier = match open.earlier.as_slice() {
        [] => String::new(),
        lines => format!(
            " Earlier rounds also listed, for the last round's reviewers to verify: {}.",
            listed(lines, room)
        ),
    };
    format!(
        "{} the review of stage `{}` (attempt {}) failed {rounds} rounds, each a commit a \
         reviewer failed with a blocking finding, so Archon stopped it instead of another \
         round. {latest}{earlier} The reviewers' results ({}) hold every finding in full. \
         Reshape the work, settle the disputed point in the brief, or retry the item with a \
         steer note.",
        StageOutcome::ESCALATE_PREFIX,
        stage.stage_id,
        stage.attempt,
        open.reviewers.join(", ")
    )
}

/// Up to `room` of `lines` joined, saying how many more there are.
fn listed(lines: &[&str], room: usize) -> String {
    assert!(!lines.is_empty(), "only findings are listed");
    let shown = lines
        .iter()
        .take(room)
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    match lines.len().saturating_sub(room) {
        0 => shown,
        more if shown.is_empty() => format!("{more} more"),
        more => format!("{shown}; and {more} more"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(head: &str, findings: &[&str]) -> Fail {
        Fail {
            agent: format!("agt_{head}_{}", findings.len()),
            head: head.into(),
            findings: findings.iter().map(|t| (*t).to_owned()).collect(),
        }
    }
    #[test]
    fn parallel_reviewers_of_one_head_are_one_round() {
        let fails = [
            fail("aaaaaaa1", &["a"]),
            fail("aaaaaaa1", &["b"]),
            fail("bbbbbbb2", &["c"]),
        ];
        assert_eq!(rounds_of(&fails), 2);
    }

    #[test]
    fn an_abbreviated_head_is_the_same_round_as_the_full_one() {
        let fails = [fail("aaaaaaa", &[]), fail("aaaaaaa1234567", &[])];
        assert_eq!(rounds_of(&fails), 1);
    }

    #[test]
    fn the_last_round_is_open_and_earlier_rounds_are_listed_apart_once() {
        let fails = [
            fail("aaaaaaa1", &["old", "kept"]),
            fail("bbbbbbb2", &["x", "y", "kept"]),
            fail("bbbbbbb2", &["x", "z"]),
        ];
        let open = Open::of(&fails);
        assert_eq!(open.latest, ["x", "y", "kept", "z"]);
        assert_eq!(open.earlier, ["old"]);
        assert_eq!(open.reviewers.len(), 3);
    }

    #[test]
    fn every_finding_is_listed_up_to_the_limit_and_the_rest_counted() {
        let many: Vec<String> = (0..MAX_LISTED + 2).map(|n| format!("f{n}")).collect();
        let lines: Vec<&str> = many.iter().map(String::as_str).collect();
        let text = listed(&lines, MAX_LISTED);
        assert!(text.contains("f0; f1") && text.contains(&format!("f{}", MAX_LISTED - 1)));
        assert!(text.ends_with("; and 2 more"), "{text}");
        assert_eq!(
            listed(&lines[..6], MAX_LISTED).matches("; ").count(),
            5,
            "six listed"
        );
        assert_eq!(listed(&lines[..2], 0), "2 more");
    }

    #[test]
    fn a_tally_written_with_titles_still_reads() {
        let old = r#"{"fails":[{"agent":"agt_a","head":"aaaaaaa1","titles":["t"]}]}"#;
        let tally: Tally = serde_json::from_str(old).unwrap();
        assert_eq!(tally.fails[0].findings, ["t"]);
    }
}
