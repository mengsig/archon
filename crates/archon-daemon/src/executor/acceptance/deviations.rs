use super::BuildEvidence;
use crate::context::DaemonContext;
use archon_core::acceptance::RequirementStatus;
use archon_core::error::ArchonError;
use archon_core::id::{QuestionId, StageRunId};
use archon_core::intent::ApprovedIntent;
use archon_core::notify::{FeedEntry, Question};
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_queue::Store;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "intent_deviation_wait:";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    stage: StageRunId,
    question: QuestionId,
    outcome: StageOutcome,
    evidence: BuildEvidence,
}

fn authorization_key(run: &PipelineRun) -> String {
    format!("intent_deviations:{}", run.id)
}
fn pending_key(stage: &StageRunId) -> String {
    format!("{PREFIX}{stage}")
}

pub(in crate::executor) fn pending(store: &Store, stage: &StageRunId) -> Result<bool, ArchonError> {
    Ok(store.kv_get(&pending_key(stage))?.is_some())
}

pub(super) async fn authorize(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    intent: &ApprovedIntent,
    evidence: &BuildEvidence,
    outcome: &StageOutcome,
) -> Result<bool, ArchonError> {
    let deviations: Vec<_> = evidence
        .requirements
        .iter()
        .filter(|r| r.status == RequirementStatus::Deviation)
        .collect();
    if deviations.is_empty() {
        return Ok(true);
    }
    let mut store = ctx.store.lock().await;
    if is_authorized(&store, run, evidence)? {
        return Ok(true);
    }
    if pending(&store, &stage.id)? {
        return Ok(false);
    }
    let proceed = deviations
        .iter()
        .map(|r| {
            crate::clarifications::proceed_allowed(
                &store,
                r.proceed_directive.as_ref(),
                archon_core::now(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .all(|allowed| *allowed);
    if proceed {
        record(
            &mut store,
            run,
            evidence,
            "proceed under active operator directive",
        )?;
        return Ok(true);
    }
    drop(store);
    ask(ctx, run, stage, intent, evidence, outcome).await
}

async fn ask(
    ctx: &DaemonContext,
    run: &PipelineRun,
    stage: &StageRun,
    intent: &ApprovedIntent,
    evidence: &BuildEvidence,
    outcome: &StageOutcome,
) -> Result<bool, ArchonError> {
    let page = super::pages::write(run, intent, &evidence.requirements, None, "deviations")?;
    let (published, warning) = crate::reports::publish_page(
        ctx,
        "Deviations to review",
        std::path::Path::new(&page),
        Some(&run.item),
    )
    .await?;
    let location = format!(
        "{}{}",
        published.location,
        warning.map(|w| format!(" ({w})")).unwrap_or_default()
    );
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text: format!(
            "Approve these deviations from approved intent for {}? Reference | proposed pictures and reasons: {location}",
            run.item
        ),
        options: vec!["approve".into(), "reject".into()],
        item: Some(run.item.clone()),
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let mut store = ctx.store.lock().await;
    let question = store.insert_question(&question)?;
    let pending = Pending {
        stage: stage.id.clone(),
        question: question.id.clone(),
        outcome: outcome.clone(),
        evidence: evidence.clone(),
    };
    store.kv_set(&pending_key(&stage.id), &serde_json::to_string(&pending)?)?;
    let feed = store.append_feed(FeedEntry::Question(question), archon_core::now())?;
    drop(store);
    crate::feed::publish(ctx, vec![feed]);
    assert!(!page.is_empty());
    assert_eq!(pending.stage, stage.id);
    Ok(false)
}

fn is_authorized(
    store: &Store,
    run: &PipelineRun,
    evidence: &BuildEvidence,
) -> Result<bool, ArchonError> {
    let Some(raw) = store.kv_get(&authorization_key(run))? else {
        return Ok(false);
    };
    let recorded: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("deviation authorization: {e}")))?;
    Ok(recorded.get("evidence") == Some(&serde_json::to_value(evidence)?))
}

fn record(
    store: &mut Store,
    run: &PipelineRun,
    evidence: &BuildEvidence,
    decision: &str,
) -> Result<(), ArchonError> {
    assert!(!decision.is_empty());
    assert!(!evidence.head.is_empty());
    let directives: Vec<_> = store
        .directives(archon_core::now())?
        .into_iter()
        .filter(|d| {
            d.source == archon_core::autonomy::DirectiveSource::Operator
                && d.clarifications == archon_core::autonomy::ClarificationPolicy::Proceed
        })
        .collect();
    store.kv_set(
        &authorization_key(run),
        &serde_json::to_string(
            &serde_json::json!({"decision":decision,"evidence":evidence,"directives_in_force":directives,"at":archon_core::now()}),
        )?,
    )
}

pub(super) fn require_authorized(
    store: &Store,
    run: &PipelineRun,
    evidence: &BuildEvidence,
) -> Result<(), ArchonError> {
    let has_deviations = evidence
        .requirements
        .iter()
        .any(|r| r.status == RequirementStatus::Deviation);
    if has_deviations && !is_authorized(store, run, evidence)? {
        return Err(ArchonError::Invalid(
            "acceptance found an unapproved deviation".into(),
        ));
    }
    Ok(())
}

pub(super) fn section(store: &Store, run: &PipelineRun) -> Result<String, ArchonError> {
    Ok(store
        .kv_get(&authorization_key(run))?
        .map(|raw| format!("\nDeviation authorization on record:\n{raw}\n"))
        .unwrap_or_default())
}

pub(in crate::executor) async fn resume_decisions(ctx: &DaemonContext) -> Result<(), ArchonError> {
    let entries = ctx.store.lock().await.kv_prefix(PREFIX)?;
    for (key, raw) in entries {
        let pending: Pending =
            serde_json::from_str(&raw).map_err(|e| ArchonError::Corrupt(format!("{key}: {e}")))?;
        let decision = decision(ctx, &pending).await?;
        let Some(outcome) = decision else { continue };
        let follow = super::super::settle::settle(ctx, &pending.stage, outcome).await?;
        super::super::run_follow_up(ctx, follow).await?;
        ctx.store.lock().await.kv_delete(&key)?;
    }
    Ok(())
}

async fn decision(
    ctx: &DaemonContext,
    pending: &Pending,
) -> Result<Option<StageOutcome>, ArchonError> {
    let mut store = ctx.store.lock().await;
    let stage = store.stage_run(&pending.stage)?;
    let question = store.question(&pending.question)?;
    if stage.state.is_terminal() {
        if question.answer.is_none() {
            store.answer_question(&question.id, "stage ended", archon_core::now())?;
        }
        return Ok(Some(StageOutcome::Cancelled {
            reason: Some("stage already settled".into()),
        }));
    }
    let run = store.run(&stage.run)?;
    let directive = crate::clarifications::proceed_allowed(&store, None, archon_core::now())?;
    let answer = match question.answer {
        Some(answer) => answer,
        None if directive => {
            let answer = "approve".to_owned();
            store.answer_question(
                &question.id,
                "proceed under active operator directive",
                archon_core::now(),
            )?;
            answer
        }
        None => return Ok(None),
    };
    if !matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "approve" | "yes" | "proceed under active operator directive"
    ) {
        return Ok(Some(StageOutcome::Failed {
            error: format!("operator rejected approved-intent deviation: {answer}"),
            infra: false,
        }));
    }
    drop(store);
    if let Err(error) = verify_pending(&run, &pending.evidence).await {
        return Ok(Some(StageOutcome::Failed {
            error: error.to_string(),
            infra: false,
        }));
    }
    let mut store = ctx.store.lock().await;
    record(&mut store, &run, &pending.evidence, &answer)?;
    assert_eq!(pending.stage, stage.id);
    assert!(is_authorized(&store, &run, &pending.evidence)?);
    Ok(Some(pending.outcome.clone()))
}

async fn verify_pending(run: &PipelineRun, evidence: &BuildEvidence) -> Result<(), ArchonError> {
    let (head, tree) = super::records::head(&run.workspace).await?;
    if head != evidence.head || tree != evidence.tree {
        return Err(ArchonError::Invalid(
            "deviation evidence head or tree changed while awaiting approval".into(),
        ));
    }
    super::images::verify(&evidence.images)?;
    assert_eq!(head, evidence.head);
    assert_eq!(tree, evidence.tree);
    Ok(())
}

pub(super) fn summary(store: &Store, run: &PipelineRun) -> Result<String, ArchonError> {
    let Some(raw) = store.kv_get(&authorization_key(run))? else {
        return Ok(String::new());
    };
    let record: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| ArchonError::Corrupt(format!("deviation decision: {e}")))?;
    let decision = record
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ArchonError::Corrupt("deviation decision is missing".into()))?;
    let mut text = format!("Decision: {decision}\nRecorded at: {}", record["at"]);
    if let Some(directives) = record
        .get("directives_in_force")
        .and_then(serde_json::Value::as_array)
    {
        for directive in directives {
            text.push_str(&format!(
                "\nOperator directive {}: {} (expires {})",
                directive["id"], directive["text"], directive["expires_at"]
            ));
        }
    }
    Ok(text)
}
