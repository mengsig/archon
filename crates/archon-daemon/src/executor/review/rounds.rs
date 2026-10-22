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

use super::{result_of, transient, verdict_in};
use crate::context::DaemonContext;

/// Finding titles a notice lists.
const MAX_TITLES: usize = 5;

/// Count `lead`'s failing review rounds now that `reviewer` settled, and post the
/// notice when this is the round that reaches the limit. Aether not answering
/// skips the count: the next settled reviewer counts again.
pub(in crate::executor) async fn reviewer_settled(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Result<(), ArchonError> {
    match count_and_notify(ctx, lead, reviewer).await {
        Err(error) if transient(&error) => {
            tracing::warn!(%lead, %reviewer, %error, "review rounds not counted; the next settled reviewer counts them again");
            Ok(())
        }
        other => other,
    }
}

async fn count_and_notify(
    ctx: &DaemonContext,
    lead: &AetherAgentId,
    reviewer: &AetherAgentId,
) -> Result<(), ArchonError> {
    let stage = ctx.store.lock().await.stage_run_by_agent(lead)?;
    let Some(stage) = stage.filter(|s| !s.state.is_terminal()) else {
        return Ok(());
    };
    let beneath = ctx.backend.descendants(lead).await?;
    let children = settled_children(&beneath, lead);
    let Some(settled) = children.iter().find(|c| c.agent == *reviewer) else {
        return Ok(());
    };
    // Only a failing verdict can add a round: a pass or prose is no new round.
    if !matches!(verdict_of(ctx, settled).await?, Some(v) if !v.pass) {
        return Ok(());
    }
    let fails = failing_verdicts(ctx, &children).await?;
    let rounds = rounds_of(&fails);
    if rounds < REVIEW_ROUNDS_BEFORE_NOTICE || already_told(ctx, &stage.id).await? {
        return Ok(());
    }
    notify_once(ctx, &stage, rounds, &fails).await
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

async fn failing_verdicts(
    ctx: &DaemonContext,
    children: &[&Descendant],
) -> Result<Vec<Verdict>, ArchonError> {
    let mut fails = Vec::new();
    for child in children {
        if let Some(verdict) = verdict_of(ctx, child).await?
            && !verdict.pass
        {
            fails.push(verdict);
        }
    }
    Ok(fails)
}

/// Whether two commit ids, each possibly abbreviated, can name one commit.
fn same_commit(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// The commits `fails` read, each once.
fn failed_heads(fails: &[Verdict]) -> Vec<&str> {
    let mut heads: Vec<&str> = Vec::new();
    for fail in fails {
        if !heads.iter().any(|head| same_commit(head, &fail.head)) {
            heads.push(&fail.head);
        }
    }
    heads
}

fn rounds_of(fails: &[Verdict]) -> usize {
    failed_heads(fails).len()
}

/// The titles of the findings of the last failing verdict, which read the latest round.
fn latest_titles(fails: &[Verdict]) -> Vec<&str> {
    let Some(last) = fails.last() else {
        return Vec::new();
    };
    let same_round = fails.iter().filter(|f| same_commit(&f.head, &last.head));
    let titles = same_round.flat_map(|f| f.titles.iter().map(String::as_str));
    titles.take(MAX_TITLES).collect()
}

fn told_key(stage: &StageRunId) -> String {
    format!("review_rounds_told:{stage}")
}

async fn already_told(ctx: &DaemonContext, stage: &StageRunId) -> Result<bool, ArchonError> {
    Ok(ctx.store.lock().await.kv_get(&told_key(stage))?.is_some())
}

/// Post the notice, marking the stage as told first so a second settling reviewer
/// cannot post it again; the mark is dropped if the post fails.
async fn notify_once(
    ctx: &DaemonContext,
    stage: &StageRun,
    rounds: usize,
    fails: &[Verdict],
) -> Result<(), ArchonError> {
    assert!(
        rounds >= REVIEW_ROUNDS_BEFORE_NOTICE,
        "a notice is for a long loop"
    );
    let key = told_key(&stage.id);
    let item = {
        let mut store = ctx.store.lock().await;
        if store.kv_get(&key)?.is_some() {
            return Ok(());
        }
        store.kv_set(&key, &rounds.to_string())?;
        store.run(&stage.run)?.item
    };
    let text = notice_text(&item.to_string(), stage, rounds, &latest_titles(fails));
    let posted = crate::executor::notify(
        ctx,
        NotificationKind::Blocker,
        &text,
        Some(&item),
        Some(&stage.run),
    )
    .await;
    if posted.is_err() {
        ctx.store.lock().await.kv_delete(&key)?;
    }
    posted
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

    fn fail(head: &str, titles: &[&str]) -> Verdict {
        Verdict {
            pass: false,
            head: head.into(),
            base: None,
            findings: Some(titles.len()),
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
