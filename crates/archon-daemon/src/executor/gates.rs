//! Operator gates (ARCHITECTURE §18.3): a passed proposal or change-report
//! stage parks its run until the operator approves, revises or rejects it.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, QuestionId, ReportId};
use archon_core::notify::{FeedEntry, NotificationKind, Question};
use archon_core::queue::QueueItem;
use archon_core::run::{GateDecision, GateSubject, OperatorGate, PipelineRun, RunState};
use archon_pipeline::OperatorWait;
use archon_queue::Store;

use super::{FollowUp, RunTable, clip, finish, gate_news, pages, recover, settle};
use crate::context::DaemonContext;
use crate::reports;

/// `(run, stage id, attempt)`: the key of one gate.
pub(crate) type GateKey = (PipelineRunId, String, u32);

/// The stage summary quoted in a gate's question is clipped to this.
const MAX_SUMMARY_CHARS: usize = 1_500;

/// Chat options of every gate question; free text is revision feedback.
const GATE_OPTIONS: [&str; 2] = ["yes", "reject"];

pub(crate) fn key_of(gate: &OperatorGate) -> GateKey {
    (gate.run.clone(), gate.stage_id.clone(), gate.attempt)
}

fn load(store: &Store, key: &GateKey) -> Result<OperatorGate, ArchonError> {
    let (run, stage_id, attempt) = key;
    let gate = store
        .gate(run, stage_id, *attempt)?
        .ok_or_else(|| ArchonError::NotFound(format!("gate {stage_id}#{attempt} of run {run}")))?;
    assert_eq!(&key_of(&gate), key, "the store returns the gate asked for");
    Ok(gate)
}

/// Record the gate and park the run on it. The returned follow-up asks the
/// operator, unless this is a replayed wait that was asked already.
pub(super) async fn open(
    ctx: &DaemonContext,
    mut run: PipelineRun,
    wait: OperatorWait,
) -> Result<FollowUp, ArchonError> {
    assert!(
        !run.state.is_terminal(),
        "only a live run waits for the operator"
    );
    assert!(wait.attempt >= 1, "attempts are 1-based");
    let mut store = ctx.store.lock().await;
    let stages = store.stage_runs(&run.id)?;
    let page = pages::gate_page(&run, &stages, &wait);
    let fresh = OperatorGate {
        run: run.id.clone(),
        item: run.item.clone(),
        stage_id: wait.stage_id.clone(),
        attempt: wait.attempt,
        subject: wait.subject,
        summary: wait.summary,
        page: page.map(|p| p.display().to_string()),
        location: None,
        report: None,
        question: None,
        decision: None,
        withdrawn: None,
        opened_at: archon_core::now(),
        decided_at: None,
    };
    let gate = store.open_gate(&fresh)?;
    assert!(gate.is_open(), "a driver never waits on a decided gate");
    let waiting = RunState::AwaitingOperator {
        stage_id: wait.stage_id,
        attempt: wait.attempt,
        subject: wait.subject,
    };
    if run.state != waiting {
        run.state = waiting;
        store.update_run(&run)?;
    }
    let mut follow = FollowUp::poke();
    if needs_asking(&store, &gate)? {
        follow.gates.push(key_of(&gate));
    }
    Ok(follow)
}

/// An open gate needs asking until its question is recorded and still open;
/// one answered without a decision persisted (a failed decide) is asked again.
fn needs_asking(store: &Store, gate: &OperatorGate) -> Result<bool, ArchonError> {
    let Some(question) = &gate.question else {
        return Ok(gate.is_open());
    };
    if !gate.is_open() {
        return Ok(false);
    }
    Ok(!store.questions(true)?.iter().any(|q| &q.id == question))
}

/// Show the gate's page and ask the operator. Publishing runs unlocked, so
/// the question is recorded only if the gate still needs asking.
pub(super) async fn announce(ctx: &DaemonContext, key: &GateKey) -> Result<FollowUp, ArchonError> {
    let (gate, item) = {
        let store = ctx.store.lock().await;
        let gate = load(&store, key)?;
        if !needs_asking(&store, &gate)? {
            return Ok(FollowUp::default());
        }
        let item = store.item(&gate.item)?;
        (gate, item)
    };
    assert!(gate.is_open(), "only an open gate is asked");
    assert_eq!(item.id, gate.item, "the question names the gate's item");
    let (report, location) = show_page(ctx, &gate, &item).await?;
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text: question_text(&gate, &item, &location),
        options: GATE_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
        item: Some(item.id.clone()),
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let feed_item = {
        let mut store = ctx.store.lock().await;
        let mut current = load(&store, key)?;
        if !needs_asking(&store, &current)? {
            return Ok(FollowUp::default());
        }
        // Question first: a crash in between asks again, never not at all.
        let question = store.insert_question(&question)?;
        let feed_item =
            store.append_feed(FeedEntry::Question(question.clone()), archon_core::now())?;
        current.report = report;
        current.location = Some(location);
        current.question = Some(question.id.clone());
        store.update_gate(&current)?;
        let news = gate_news::opened(ctx.config.aether.mode, &current, &item, &question);
        crate::triggers::hold(&mut store, news)?;
        feed_item
    };
    crate::triggers::wake(ctx);
    // No receivers is normal (nobody has chat open); not an error.
    let _ = ctx.feed.send(feed_item);
    Ok(FollowUp::default())
}

/// `(report, location)`: the page published in Lavish or as a file path.
/// A page that cannot be published is still asked about, saying why.
async fn show_page(
    ctx: &DaemonContext,
    gate: &OperatorGate,
    item: &QueueItem,
) -> Result<(Option<ReportId>, String), ArchonError> {
    let (run, stages) = {
        let store = ctx.store.lock().await;
        (store.run(&gate.run)?, store.stage_runs(&gate.run)?)
    };
    let refused: Vec<String> = pages::gate_pages(&run, &stages, &gate.stage_id, gate.attempt)
        .into_iter()
        .filter_map(Result::err)
        .collect();
    for why in &refused {
        crate::feed::system(ctx, format!("Gate report was not published: {why}")).await?;
    }
    if gate.page.is_none() && !refused.is_empty() {
        return Ok((
            None,
            format!("reports not published: {}", refused.join("; ")),
        ));
    }
    let (report, mut location) = show_selected_page(ctx, gate, item, &run.workspace).await?;
    if !refused.is_empty() {
        location.push_str(&format!("\nReports not published: {}", refused.join("; ")));
    }
    Ok((report, location))
}

async fn show_selected_page(
    ctx: &DaemonContext,
    gate: &OperatorGate,
    item: &QueueItem,
    workspace: &Path,
) -> Result<(Option<ReportId>, String), ArchonError> {
    let Some(page) = &gate.page else {
        return Ok((None, "no page was written; the summary is below".to_owned()));
    };
    if let Err(why) = pages::inside_workspace(workspace, page) {
        crate::feed::system(ctx, format!("Gate report {page} was not published: {why}")).await?;
        return Ok((None, format!("{page} (not published: {why})")));
    }
    let title = match gate.subject {
        GateSubject::Proposal => format!("Proposal: {}", item.title),
        GateSubject::Report => format!("Change report: {}", item.title),
    };
    match reports::publish_page(ctx, &title, Path::new(page), Some(&item.id)).await {
        Ok((published, warning)) => {
            let location = match warning {
                Some(warning) => format!("{} ({warning})", published.location),
                None => published.location,
            };
            Ok((Some(published.report), location))
        }
        Err(error @ (ArchonError::Invalid(_) | ArchonError::NotFound(_))) => {
            tracing::warn!(%error, page, "gate page not published");
            crate::feed::system(
                ctx,
                format!("Gate report {page} was not published: {error}"),
            )
            .await?;
            Ok((None, format!("{page} (not published: {error})")))
        }
        Err(error) => Err(error),
    }
}

fn question_text(gate: &OperatorGate, item: &QueueItem, location: &str) -> String {
    let (ask, revise) = match gate.subject {
        GateSubject::Proposal => (
            "Approve the direction for",
            "the proposal is redone with it",
        ),
        GateSubject::Report => (
            "Approve the change report before the PR for",
            "the work goes back to implementation with it",
        ),
    };
    let summary = clip(gate.summary.trim(), MAX_SUMMARY_CHARS);
    format!(
        "{ask} {} \"{}\"? Answer yes (a note after it goes along), reject to cancel the item, \
         or give feedback and {revise}.\nPage: {location}\n\n{summary}",
        item.id, item.title
    )
}

/// Persist the operator's decision, close the reopen question of its page,
/// then act on it. Deciding a closed gate again alike is a no-op;
/// differently, or after a withdrawal, is refused.
pub(super) async fn decide(
    ctx: &DaemonContext,
    table: &mut RunTable,
    key: &GateKey,
    decision: GateDecision,
) -> Result<FollowUp, ArchonError> {
    let (gate, run, all) = {
        let mut store = ctx.store.lock().await;
        let mut gate = load(&store, key)?;
        if !check_open(&gate, &decision)? {
            return Ok(FollowUp::default());
        }
        gate.decision = Some(decision.clone());
        gate.decided_at = Some(archon_core::now().max(gate.opened_at));
        store.update_gate(&gate)?;
        debug_assert!(!gate.is_open(), "a decision closes the gate");
        (
            gate.clone(),
            store.run(&gate.run)?,
            store.stage_runs(&gate.run)?,
        )
    };
    assert!(
        !run.state.is_terminal(),
        "finishing a run withdraws its gates"
    );
    if gate.report.is_some() {
        reports::close_stale_questions(ctx).await?;
    }
    let text = format!(
        "The {} for {} was {}.",
        gate.subject.noun(),
        gate.item,
        decision.word()
    );
    finish::notify(
        ctx,
        NotificationKind::Progress,
        &text,
        Some(&gate.item),
        Some(&run.id),
    )
    .await?;
    let actions = match table.drivers.get_mut(&run.id) {
        Some(driver) => {
            driver.on_operator_decision(&gate.stage_id, gate.attempt, decision, &all)?
        }
        // The rebuilt driver reads the decision just persisted.
        None => return recover::resume_run(ctx, table, run, &all).await,
    };
    settle::apply(ctx, table, run, actions).await
}

/// `true` when `gate` is open; `false` when it holds `decision` already.
fn check_open(gate: &OperatorGate, decision: &GateDecision) -> Result<bool, ArchonError> {
    let name = format!("the {} gate of {}", gate.subject.noun(), gate.item);
    if let Some(reason) = &gate.withdrawn {
        return Err(ArchonError::InvalidTransition(format!(
            "{name} was withdrawn: {reason}"
        )));
    }
    match &gate.decision {
        None => Ok(true),
        Some(known) if known == decision => Ok(false),
        Some(known) => Err(ArchonError::InvalidTransition(format!(
            "{name} was already {}",
            known.word()
        ))),
    }
}
