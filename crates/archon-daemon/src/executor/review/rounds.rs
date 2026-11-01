//! A lead that fixes and re-reviews round after round is stopped. A round is
//! a commit some reviewer of the lead failed with a blocking finding;
//! [`REVIEW_ROUND_CAP`] of them end the stage attempt with an escalation to the
//! orchestrator that lists the open blocking findings, instead of another round.

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

/// Finding titles an escalation lists.
const MAX_TITLES: usize = 5;

/// A stage whose review failed [`REVIEW_ROUND_CAP`] rounds: archond stops it
/// with `reason`, an `ESCALATE:` failure that ends its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::executor) struct Capped {
    pub stage: StageRunId,
    pub reason: String,
}

/// Add `reviewer`'s failing verdict to its stage attempt's tally; the stage to
/// stop once the tally reaches the cap. The tally is kept per stage attempt and
/// grows only by settled reviewers seen here, so a lead reused by a later attempt
/// starts at none. An Aether answer that fails is returned, so the pump repeats the event.
pub(in crate::executor) async fn reviewer_settled(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Result<Option<Capped>, ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(lead)?;
    let Some(stage) = stage.filter(|s| !s.state.is_terminal()) else {
        return Ok(None);
    };
    let beneath = ctx.backend.descendants(lead).await?;
    let settled = settled_children(&beneath, lead);
    let Some(child) = settled.into_iter().find(|c| c.agent == *reviewer) else {
        return Ok(None);
    };
    let Some(verdict) = verdict_of(ctx, child).await?.filter(|v| !v.pass) else {
        return Ok(None);
    };
    let mut tally = load(ctx, &stage.id).await?;
    // A replayed reviewer adds no round, but may still owe the stop.
    if !tally.fails.iter().any(|f| f.agent == reviewer.as_str()) {
        tally.fails.push(Fail {
            agent: reviewer.to_string(),
            head: verdict.head,
            titles: verdict.titles,
        });
        save(ctx, &stage.id, &tally).await?;
    }
    let rounds = rounds_of(&tally.fails);
    if rounds < REVIEW_ROUND_CAP {
        return Ok(None);
    }
    let reason = escalation(&stage, rounds, &latest_titles(&tally.fails));
    assert!(StageOutcome::ends_run(&reason), "a capped review escalates");
    Ok(Some(Capped {
        stage: stage.id,
        reason,
    }))
}

/// The direct children of `lead` that completed a task, in the order Aether lists them.
fn settled_children<'a>(beneath: &'a [Descendant], lead: &AetherAgentId) -> Vec<&'a Descendant> {
    beneath
        .iter()
        .filter(|d| d.parent == *lead && d.state == DescendantState::Completed)
        .filter(|d| d.task.is_some())
        .collect()
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
    titles: Vec<String>,
}

/// The failing verdicts of one stage attempt.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Tally {
    fails: Vec<Fail>,
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

/// The titles of the findings of the last failing verdict's round, the latest one.
fn latest_titles(fails: &[Fail]) -> Vec<&str> {
    let Some(last) = fails.last() else {
        return Vec::new();
    };
    let same_round = fails.iter().filter(|f| same_commit(&f.head, &last.head));
    let titles = same_round.flat_map(|f| f.titles.iter().map(String::as_str));
    titles.take(MAX_TITLES).collect()
}

/// The failure a capped stage settles with: its rounds and the open blocking findings.
fn escalation(stage: &StageRun, rounds: usize, titles: &[&str]) -> String {
    assert!(rounds >= REVIEW_ROUND_CAP, "only a capped review escalates");
    assert!(titles.len() <= MAX_TITLES, "the titles are bounded");
    let findings = match titles {
        [] => "The reviewers gave no titles for their blocking findings.".to_owned(),
        titles => format!("Open blocking findings: {}.", titles.join("; ")),
    };
    format!(
        "{} the review of stage `{}` (attempt {}) failed {rounds} rounds, each a commit a \
         reviewer failed with a blocking finding, so Archon stopped it instead of another \
         round. {findings} Reshape the work, settle the disputed point in the brief, or \
         retry the item with a steer note.",
        StageOutcome::ESCALATE_PREFIX,
        stage.stage_id,
        stage.attempt
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(head: &str, titles: &[&str]) -> Fail {
        Fail {
            agent: "agt_x".into(),
            head: head.into(),
            titles: titles.iter().map(|t| (*t).to_owned()).collect(),
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
    fn the_latest_round_lists_all_its_titles_up_to_the_limit() {
        let fails = [
            fail("aaaaaaa1", &["old"]),
            fail("bbbbbbb2", &["x", "y"]),
            fail("bbbbbbb2", &["z", "1", "2", "3"]),
        ];
        assert_eq!(latest_titles(&fails), ["x", "y", "z", "1", "2"]);
    }
}
