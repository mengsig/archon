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
    let mut fresh = OperatorGate {
        run: run.id.clone(),
        item: run.item.clone(),
        stage_id: wait.stage_id.clone(),
        attempt: wait.attempt,
        subject: wait.subject,
        summary: wait.summary,
        page: page.as_ref().map(|p| p.display().to_string()),
        location: None,
        report: None,
        question: None,
        decision: None,
        withdrawn: None,
        opened_at: archon_core::now(),
        decided_at: None,
    };
    if fresh.subject == GateSubject::Proposal
        && requires_intent(&store, &fresh)?
        && store
            .gate(&fresh.run, &fresh.stage_id, fresh.attempt)?
            .is_none()
    {
        let designed = stages.iter().any(|s| {
            s.stage_id == fresh.stage_id
                && s.attempt == fresh.attempt
                && s.kind == Some(archon_core::catalog::StageKind::Design)
        });
        let rendered = if designed {
            super::design_ledger::prepare(ctx, &mut store, &run, &fresh, &stages)?
        } else {
            let original =
                page.ok_or_else(|| ArchonError::Invalid("proposal needs a review page".into()))?;
            super::intent::prepare(ctx, &mut store, &run, &fresh, &stages, &original)?
        };
        fresh.page = Some(rendered.display().to_string());
    }
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

fn requires_intent(store: &Store, gate: &OperatorGate) -> Result<bool, ArchonError> {
    let item = store.item(&gate.item)?;
    assert_eq!(item.id, gate.item);
    Ok(item.intent.is_some()
        || item.model_plan.0.contains_key("intent-acceptance")
        || item.autonomy.proposal == archon_core::autonomy::ProposalMode::Live)
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
    let (feed_item, number) = {
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
        (feed_item, question.number)
    };
    crate::triggers::wake(ctx);
    // No receivers is normal (nobody has chat open); not an error.
    let _ = ctx.feed.send(feed_item);
    crate::push::gate(ctx, &item, gate.subject, number);
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
    let ledger = super::design_ledger::is_ledger_page(ctx, Path::new(page));
    if !ledger && let Err(why) = pages::inside_workspace(workspace, page) {
        crate::feed::system(ctx, format!("Gate report {page} was not published: {why}")).await?;
        return Ok((None, format!("{page} (not published: {why})")));
    }
    let title = match gate.subject {
        GateSubject::Proposal => format!("Proposal: {}", item.title),
        GateSubject::Report => format!("Change report: {}", item.title),
    };
    let pending = if gate.subject == GateSubject::Proposal {
        let store = ctx.store.lock().await;
        if super::intent::has_pending(&store, gate)? {
            Some(super::intent::pending(&store, gate)?)
        } else {
            None
        }
    } else {
        None
    };
    let checked =
        reports::publish_checked_page(ctx, &title, Path::new(page), Some(&item.id), |path| {
            if let Some(saved) = &pending {
                super::intent::verify_page(saved, path)?;
            }
            Ok(())
        });
    match checked.await {
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
        if gate.subject == GateSubject::Proposal
            && matches!(decision, GateDecision::Approve { .. })
            && (requires_intent(&store, &gate)? || super::intent::has_pending(&store, &gate)?)
        {
            approve_intent(&mut store, &gate)?;
        } else {
            store.update_gate(&gate)?;
        }
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
    super::proposal::design_decided(ctx, &gate, &all, &decision).await;
    let actions = match table.drivers.get_mut(&run.id) {
        Some(driver) => {
            driver.on_operator_decision(&gate.stage_id, gate.attempt, decision, &all)?
        }
        // The rebuilt driver reads the decision just persisted.
        None => return recover::resume_run(ctx, table, run, &all).await,
    };
    settle::apply(ctx, table, run, actions).await
}

/// Close the questions of gates that no longer wait: withdrawn with their
/// run, or decided whoever recorded the decision. A decision acted on but
/// not answered (a failed step after it, a crash) would leave its question,
/// and the notification it posted, open for good.
pub(crate) async fn close_settled_questions(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let _answering = ctx.answering.lock().await;
    let now = archon_core::now();
    let answers = {
        let mut store = ctx.store.lock().await;
        let mut answers = store.close_withdrawn_questions(now)?;
        answers.extend(store.close_decided_questions(now)?);
        answers.extend(store.close_cancelled_item_questions(now)?);
        answers
    };
    if answers.is_empty() {
        return Ok(());
    }
    tracing::info!(
        count = answers.len(),
        "closed questions of gates that no longer wait"
    );
    crate::feed::publish(ctx, answers);
    Ok(())
}

fn approve_intent(store: &mut Store, gate: &OperatorGate) -> Result<(), ArchonError> {
    assert_eq!(gate.subject, GateSubject::Proposal);
    assert!(matches!(gate.decision, Some(GateDecision::Approve { .. })));
    let report = gate.report.as_ref().ok_or_else(|| {
        ArchonError::Invalid("proposal ledger must be published before approval".into())
    })?;
    let saved = super::intent::pending(store, gate)?;
    let published = reports::published_path(store, report)?;
    super::intent::verify_page(&saved, &saved.page)?;
    if published != saved.page {
        super::intent::verify_page(&saved, &published)?;
    }
    store.approve_gate_intent(gate, &saved.record)
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
