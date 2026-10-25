//! Read-only item views for the CLI: summaries for `archon queue list`, and
//! the item a short id prefix names.

use archon_core::error::ArchonError;
use archon_core::id::queue_item_prefix;
use archon_core::notify::Question;
use archon_core::protocol::{ItemQuery, ItemSummary, Reply, WaitingOn};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run;
use archon_core::run::{GateSubject, PipelineRun, RunState, StageRun, StageState};
use archon_queue::store::Store;

use crate::context::DaemonContext;

/// Matches an ambiguous prefix names; one more tells there are others.
const LISTED_MATCHES: usize = 10;

/// The listing `ListItems` gives, each item with its live stage, spend and
/// newest PR URL. Each item takes the store lock anew, so a long listing
/// lets other requests in between.
pub(super) async fn summaries(
    ctx: &DaemonContext,
    query: &ItemQuery,
) -> Result<Reply, ArchonError> {
    Ok(Reply::ItemSummaries(read_summaries(ctx, query).await?))
}

pub(super) async fn read_summaries(
    ctx: &DaemonContext,
    query: &ItemQuery,
) -> Result<Vec<ItemSummary>, ArchonError> {
    let waiting = super::waiting::WaitingView::read(ctx).await?;
    let estimates = ctx.config.limits.count_estimated_cost;
    let store = ctx.read_store().await?;
    let items = store.items(query)?;
    let count = items.len();
    let mut summaries = Vec::with_capacity(count);
    for item in items {
        summaries.push(summarize(&store, item, estimates, &waiting)?);
    }
    drop(store);
    assert_eq!(summaries.len(), count, "one summary per listed item");
    Ok(summaries)
}

fn summarize(
    store: &Store,
    item: QueueItem,
    estimates: bool,
    waiting: &super::waiting::WaitingView,
) -> Result<ItemSummary, ArchonError> {
    let runs = store.runs_for_item(&item.id)?;
    assert!(
        runs.iter().all(|r| r.item == item.id),
        "runs belong to their item"
    );
    let stage = match &item.state {
        QueueState::Running { run } => runs
            .iter()
            .find(|r| &r.id == run)
            .and_then(|r| r.state.live_stage())
            .map(str::to_owned),
        _ => None,
    };
    // Grouped by run, oldest first, so the newest run's PR comes first.
    let stages = store.stage_runs_for_item(&item.id)?;
    debug_assert!(
        stages.iter().all(|s| runs.iter().any(|r| r.id == s.run)),
        "stage runs belong to the item's runs"
    );
    let pr_url = run::pr_url(stages.iter()).map(str::to_owned);
    let spent_usd = store.spend_for_item(&item.id, estimates)?;
    let waiting_on = waiting_on(store, &item, &runs, &stages)?;
    let waiting_reason = waiting.reason(store, &item, waiting_on.as_ref())?;
    let switched_to = crate::executor::steer::switched_to(store, &item)?;
    Ok(ItemSummary {
        waiting_reason,
        waiting_on,
        item,
        stage,
        spent_usd,
        pr_url,
        switched_to,
    })
}

/// Only the current run's gates and blocking Aether questions imply a wait.
/// MCP ask_operator questions are advisory and do not pause a stage.
pub(super) fn waiting_on(
    store: &Store,
    item: &QueueItem,
    runs: &[PipelineRun],
    stages: &[StageRun],
) -> Result<Option<WaitingOn>, ArchonError> {
    let QueueState::Running { run } = &item.state else {
        return Ok(None);
    };
    let current = runs.iter().find(|r| &r.id == run).ok_or_else(|| {
        ArchonError::Corrupt(format!("current run {run} missing for {}", item.id))
    })?;
    if let RunState::AwaitingOperator {
        stage_id,
        attempt,
        subject,
    } = &current.state
    {
        let gate = store.gate(run, stage_id, *attempt)?;
        if gate.as_ref().is_some_and(|g| !g.is_open()) {
            return Ok(None);
        }
        let question = gate
            .as_ref()
            .and_then(|g| g.question.as_ref())
            .map(|id| store.question(id))
            .transpose()?;
        if question.as_ref().is_some_and(|q| !q.is_open()) {
            return Ok(None);
        }
        return Ok(Some(WaitingOn {
            subject: match subject {
                GateSubject::Proposal => "proposal",
                GateSubject::Report => "report",
            }
            .into(),
            stage: stage_id.clone(),
            question: question.as_ref().map(|q| q.id.clone()),
            prompt: question.as_ref().map_or_else(
                || format!("Approve the {}", subject.noun()),
                |q| q.text.clone(),
            ),
            next_action: question.as_ref().map_or_else(
                || "archon questions".into(),
                |q| format!("archon answer {} {}", q.id, approving_word(q)),
            ),
        }));
    }
    for question in store.questions(true)? {
        let Some(reference) = &question.aether else {
            continue;
        };
        if let Some(stage) = stages.iter().find(|s| {
            &s.run == run
                && s.state == StageState::Running
                && s.aether_agent.as_ref() == Some(&reference.agent)
        }) {
            return Ok(Some(WaitingOn {
                subject: "question".into(),
                stage: stage.stage_id.clone(),
                question: Some(question.id.clone()),
                prompt: question.text,
                next_action: format!("archon answer {} <answer>", question.id),
            }));
        }
    }
    Ok(None)
}

/// The option the question offers for going ahead, else `approve`, which a
/// gate always reads as approval.
fn approving_word(question: &Question) -> &str {
    let offers = |word: &str| {
        question
            .options
            .iter()
            .any(|o| o.eq_ignore_ascii_case(word))
    };
    for word in ["approve", "yes"] {
        if offers(word) {
            return question
                .options
                .iter()
                .find(|o| o.eq_ignore_ascii_case(word))
                .map_or(word, String::as_str);
        }
    }
    question.options.first().map_or("approve", String::as_str)
}

/// The one item whose id starts with `text`; an unknown prefix is
/// `NotFound`, an ambiguous one `Invalid` naming what it matches.
pub(super) async fn resolve(ctx: &DaemonContext, text: &str) -> Result<Reply, ArchonError> {
    let prefix = queue_item_prefix(text)?;
    let mut matches = ctx
        .store
        .lock()
        .await
        .items_with_id_prefix(&prefix, LISTED_MATCHES + 1)?;
    assert!(
        matches.len() <= LISTED_MATCHES + 1,
        "the store honours the limit"
    );
    match matches.len() {
        0 => Err(ArchonError::NotFound(format!(
            "queue item starting with {prefix}"
        ))),
        1 => Ok(Reply::Item(matches.remove(0))),
        _ => Err(ArchonError::Invalid(ambiguous(&prefix, &matches))),
    }
}

/// `qi_01K is ambiguous; it starts 3 items: …; give more of the id`.
fn ambiguous(prefix: &str, matches: &[QueueItem]) -> String {
    assert!(matches.len() > 1, "one match is not ambiguous");
    let mut named: Vec<String> = matches
        .iter()
        .take(LISTED_MATCHES)
        .map(|item| format!("{} ({})", item.id, item.title))
        .collect();
    let count = if matches.len() > LISTED_MATCHES {
        named.push("and more".to_owned());
        format!("more than {LISTED_MATCHES}")
    } else {
        matches.len().to_string()
    };
    format!(
        "{prefix} is ambiguous; it starts {count} items: {}; give more of the id",
        named.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use archon_core::id::QuestionId;

    use super::*;

    fn question(options: &[&str]) -> Question {
        Question {
            id: QuestionId::new(),
            number: 3,
            text: "Go?".into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            item: None,
            aether: None,
            asked_by: None,
            asked_at: archon_core::now(),
            answer: None,
            answered_at: None,
        }
    }

    #[test]
    fn the_hint_uses_a_word_the_question_offers() {
        assert_eq!(approving_word(&question(&["yes", "reject"])), "yes");
        assert_eq!(approving_word(&question(&["Approve", "reject"])), "Approve");
        assert_eq!(approving_word(&question(&["go", "stop"])), "go");
        assert_eq!(approving_word(&question(&[])), "approve");
    }
}
