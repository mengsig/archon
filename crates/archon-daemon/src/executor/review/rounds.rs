//! A lead that fixes and re-reviews for many rounds is told to the operator
//! once, while it runs. A round is a commit some reviewer of the lead failed;
//! [`REVIEW_ROUNDS_BEFORE_NOTICE`] of them post one notification and change nothing else.

use archon_core::aether_ids::AetherAgentId;
use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::notify::NotificationKind;
use archon_core::ports::{Descendant, DescendantState};
use archon_core::run::StageRun;
use archon_pipeline::contract::REVIEW_ROUNDS_BEFORE_NOTICE;
use archon_pipeline::review::Verdict;
use serde::{Deserialize, Serialize};

use super::{result_of, verdict_in};
use crate::context::DaemonContext;

/// Finding titles a notice lists.
const MAX_TITLES: usize = 5;

/// Add `reviewer`'s failing verdict to its stage attempt's tally and post the notice
/// when the tally reaches the limit. The tally is kept per stage attempt and grows only
/// by settled reviewers seen here, so a lead reused by a later attempt starts at none.
/// An Aether answer that fails is returned, so the pump repeats the event.
pub(in crate::executor) async fn reviewer_settled(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Result<(), ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(lead)?;
    let Some(stage) = stage.filter(|s| !s.state.is_terminal()) else {
        return Ok(());
    };
    let beneath = ctx.backend.descendants(lead).await?;
    let settled = settled_children(&beneath, lead);
    let Some(child) = settled.into_iter().find(|c| c.agent == *reviewer) else {
        return Ok(());
    };
    let Some(verdict) = verdict_of(ctx, child).await?.filter(|v| !v.pass) else {
        return Ok(());
    };
    let mut tally = load(ctx, &stage.id).await?;
    // A replayed reviewer adds no round, but may still owe the notice.
    if !tally.fails.iter().any(|f| f.agent == reviewer.as_str()) {
        tally.fails.push(Fail {
            agent: reviewer.to_string(),
            head: verdict.head,
            titles: verdict.titles,
        });
        save(ctx, &stage.id, &tally).await?;
    }
    let rounds = rounds_of(&tally.fails);
    if rounds < REVIEW_ROUNDS_BEFORE_NOTICE || tally.told {
        return Ok(());
    }
    notify_once(ctx, &stage, rounds, tally).await
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

/// The failing verdicts of one stage attempt, and whether the operator was told.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Tally {
    fails: Vec<Fail>,
    told: bool,
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

/// Post the notice, then mark the attempt as told. A crash between the two repeats the
/// notice when the event is replayed: a repeat is preferred to a notice lost for good.
async fn notify_once(
    ctx: &DaemonContext,
    stage: &StageRun,
    rounds: usize,
    mut tally: Tally,
) -> Result<(), ArchonError> {
    assert!(
        rounds >= REVIEW_ROUNDS_BEFORE_NOTICE && !tally.told,
        "a notice is for a long loop and is posted once"
    );
    let item = ctx.store.lock().await.run(&stage.run)?.item;
    let text = notice_text(
        &item.to_string(),
        stage,
        rounds,
        &latest_titles(&tally.fails),
    );
    let kind = NotificationKind::Blocker;
    crate::executor::notify(ctx, kind, &text, Some(&item), Some(&stage.run)).await?;
    tally.told = true;
    save(ctx, &stage.id, &tally).await
}

fn notice_text(item: &str, stage: &StageRun, rounds: usize, titles: &[&str]) -> String {
    let findings = match titles {
        [] => "The reviewers gave no finding titles.".to_owned(),
        titles => format!("Latest findings: {}.", titles.join("; ")),
    };
    format!(
        "Item `{item}`: the review of stage `{}` (attempt {}) has failed {rounds} rounds and \
         the lead is still fixing it. {findings} The run goes on. To stop it, run `archon queue \
         cancel {item}`; to change course, `archon queue steer {item} --note \"…\"`.",
        stage.stage_id, stage.attempt
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
