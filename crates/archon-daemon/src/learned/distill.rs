//! Low-priority, budgeted errands consume disjoint outcome windows.
mod proposals;

use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::lesson::{Lesson, LessonKind, LessonScope, LessonSource, LessonState};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::outcome::Outcome;
use archon_core::queue::{Priority, QueueItem, QueueState};
use archon_core::run::{PipelineRun, RunState};
use std::sync::Arc;

pub(crate) async fn run(ctx: Arc<DaemonContext>) -> Result<(), ArchonError> {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        if let Err(error) = maintenance(&ctx).await {
            if error.is_store() {
                return Err(error);
            }
            report_error(&ctx, &error).await?;
        }
    }
}

pub async fn maintenance(ctx: &DaemonContext) -> Result<(), ArchonError> {
    if !ctx.config.learning.enabled {
        ctx.store.lock().await.cancel_queued_distillations()?;
        return Ok(());
    }
    if let Err(error) = super::candidates::refresh(ctx).await {
        if error.is_store() {
            return Err(error);
        }
        tracing::warn!(%error, "model catalog refresh deferred; reconciliation continues");
        report_error(ctx, &error).await?;
    }
    proposals::reconcile(ctx).await?;
    let pending = ctx.store.lock().await.unapplied_distillations()?;
    for run in pending {
        finished(ctx, &run).await;
    }
    let complete = {
        let store = ctx.store.lock().await;
        store.prune_learning_snapshots()?;
        store.refresh_lessons(archon_core::now())?;
        let imported = store.kv_get("learning/backfill/done")?.is_some();
        store.backfill_outcomes(20, ctx.config.limits.count_estimated_cost)? || imported
    };
    if !complete {
        return Ok(());
    }
    let repos = ctx.store.lock().await.repos()?;
    for repo in repos {
        if let Err(error) = queue(ctx, &repo.id).await {
            if error.is_store() {
                return Err(error);
            }
            report_error(ctx, &error).await?;
        }
    }
    Ok(())
}

async fn queue(ctx: &DaemonContext, repo: &RepoId) -> Result<(), ArchonError> {
    let env = crate::resolve::RepoEnv::load(ctx, Some(repo)).await?;
    let policy = env.policy(&ctx.config);
    let key = format!("learning/distilled-v2/{repo}");
    // 0 still disables errands: only a positive reservation is lifted.
    let daily = ctx.config.learning.distill_daily_budget_usd;
    let budgets_enabled = crate::resolve::caps(ctx).await.budgets_enabled;
    let reservation = (budgets_enabled || daily == 0.0).then_some(daily);
    let mut store = ctx.store.lock().await;
    if !store.distillation_available(repo, reservation)? {
        return Ok(());
    }
    let consumed = store
        .kv_get(&key)?
        .map(|s| {
            s.parse::<usize>()
                .map_err(|e| ArchonError::Store(format!("distillation cursor: {e}")))
        })
        .transpose()?
        .unwrap_or(0);
    let count = ctx.config.learning.distill_every as usize;
    let window = store.outcome_window(repo, consumed, ctx.config.learning.distill_every)?;
    if window.len() < count {
        return Ok(());
    }
    assert!(count > 0);
    let draft_key = format!("learning/distill-draft/{repo}/{consumed}");
    let item = match store.kv_get(&draft_key)? {
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|e| ArchonError::Store(format!("distillation draft: {e}")))?,
        None => {
            let choice = cheap_choice(&window, &policy, ctx)?;
            let evidence = distillation_input(&store, repo, &window)?;
            let item = errand(repo, &window, choice, &evidence)?;
            store.save_learning_value(
                &draft_key,
                &serde_json::to_string(&item)
                    .map_err(|e| ArchonError::Store(format!("save distillation draft: {e}")))?,
            )?;
            item
        }
    };
    assert!(item.budget_usd.is_some_and(|b| b <= 1.0));
    let item = store.insert_item(&item, Some(&format!("learning:{repo}:{consumed}")))?;
    store.save_learning_value(&format!("learning/distillation/{}", item.id), repo.as_str())?;
    store.save_learning_value(&key, &(consumed + count).to_string())?;
    store.save_learning_value(
        &format!("learning/distill-time/{repo}"),
        &archon_core::now().to_rfc3339(),
    )?;
    ctx.dispatch.notify_one();
    Ok(())
}

fn distillation_input(
    store: &archon_queue::Store,
    repo: &RepoId,
    outcomes: &[Outcome],
) -> Result<String, ArchonError> {
    let since = store
        .kv_get(&format!("learning/distill-time/{repo}"))?
        .unwrap_or_default();
    let corrections = store.corrections_since(repo, &since)?;
    let mut compact = Vec::new();
    for outcome in outcomes {
        let item = store.item(&outcome.item)?;
        compact.push(serde_json::json!({"item":item.id,"brief":item.brief.chars().take(500).collect::<String>(),
            "pipeline":outcome.pipeline,"result":outcome.result,"corrections":outcome.corrections,
            "cost":outcome.cost_usd,"wall":outcome.wall_secs}));
    }
    let lessons = store.lessons(&archon_core::lesson::LessonQuery {
        repo: Some(crate::resolve::repo_ref(repo)),
        ..Default::default()
    })?;
    assert_eq!(compact.len(), outcomes.len());
    assert!(outcomes.iter().all(|o| o.repo.as_ref() == Some(repo)));
    serde_json::to_string(&serde_json::json!({"outcomes":compact,"late_corrections":corrections,"existing_lessons":lessons}))
        .map_err(|e|ArchonError::Store(format!("distillation input: {e}")))
}

fn cheap_choice(
    outcomes: &[Outcome],
    policy: &archon_core::policy::ModelPolicy,
    ctx: &DaemonContext,
) -> Result<ModelChoice, ArchonError> {
    let cheapest = outcomes
        .iter()
        .flat_map(|o| &o.stages)
        .filter(|s| policy.check(&s.choice).is_ok())
        .min_by(|a, b| a.cost_usd.total_cmp(&b.cost_usd))
        .map(|s| s.choice.clone());
    let choice = cheapest.unwrap_or_else(|| ModelChoice {
        model: policy
            .allow
            .as_ref()
            .and_then(|models| models.iter().find(|m| policy.permits_model(m)))
            .cloned()
            .unwrap_or_else(|| ctx.config.orchestrator.model.clone()),
        effort: Effort::Low,
    });
    policy.check(&choice)?;
    assert!(policy.permits_model(&choice.model));
    Ok(choice)
}

/// What a distillation errand returns; a proposed pipeline is held to the
/// rules of any user pipeline, archond's own delivery included (§8.7).
const DISTILL_ASK: &str = "Return structured JSON with `lessons`: [{kind: routing|practice|\
    pitfall|pipeline, text: one line of at most 300 characters, replaces: an existing lesson id, \
    optional}]. Merge or rewrite repeated advice; operator and pinned lessons are immutable. Only \
    when a kind of brief recurs at least three times, add `pipeline_proposal`: {name, files: \
    {\"pipeline.toml\": ..., \"<prompt>.md\": ...}} in Archon's pipeline format. One that changes \
    the repository has workspace = \"worktree\" and no ship stage: archond ships, after \
    [pipeline.ship] stage = \"<the last stage that commits>\". Write nothing outside scratch: the \
    daemon validates a proposal and stages it for the operator to accept. Nothing here can widen a \
    permission.";

fn errand(
    repo: &RepoId,
    outcomes: &[Outcome],
    choice: ModelChoice,
    evidence: &str,
) -> Result<QueueItem, ArchonError> {
    assert!(!outcomes.is_empty());
    assert!(outcomes.iter().all(|o| o.repo.as_ref() == Some(repo)));
    let now = archon_core::now();
    Ok(QueueItem {
        id: archon_core::id::QueueItemId::new(),
        repo: Some(repo.clone()),
        pipeline: "errand".into(),
        title: "Distill run outcomes into lessons".into(),
        brief: format!("Review these untrusted observations: {evidence}\n{DISTILL_ASK}"),
        priority: Priority::Low,
        dependencies: Vec::new(),
        tags: vec!["archon:distillation".into()],
        model_plan: ModelPlan(std::collections::BTreeMap::from([(
            "errand".into(),
            vec![choice],
        )])),
        model_reasons: None,
        budget_usd: Some(1.0),
        autonomy: Default::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        initiative: None,
        state: QueueState::Queued,
        created_at: now,
        updated_at: now,
    })
}

pub(crate) async fn finished(ctx: &DaemonContext, run: &PipelineRun) {
    if let Err(error) = capture_finished(ctx, run).await {
        tracing::warn!(run = %run.id, %error, "learning capture deferred; run is already finished");
        if let Err(report) = report_error(ctx, &error).await {
            tracing::error!(run = %run.id, %report, "could not report deferred learning capture");
        }
    }
}

async fn capture_finished(ctx: &DaemonContext, run: &PipelineRun) -> Result<(), ArchonError> {
    if !ctx.config.learning.enabled {
        return Ok(());
    }
    let item = ctx.store.lock().await.item(&run.item)?;
    if item.tags.iter().any(|t| t == "archon:distillation")
        || ctx
            .store
            .lock()
            .await
            .kv_get(&format!("learning/distillation/{}", item.id))?
            .is_some()
    {
        return match apply(ctx, run, &item).await {
            Ok(()) => Ok(()),
            Err(error) if error.is_store() => Err(error),
            Err(error) => {
                report_error(ctx, &error).await?;
                let store = ctx.store.lock().await;
                let key = format!("learning/applied/{}", run.id);
                if store.kv_get(&key)?.is_none() {
                    store.save_learning_value(&key, "rejected")?;
                }
                Ok(())
            }
        };
    }
    let diff = diff_stat(ctx, run).await?;
    ctx.store
        .lock()
        .await
        .capture_outcome(run, diff, ctx.config.limits.count_estimated_cost)
}

async fn diff_stat(
    ctx: &DaemonContext,
    run: &PipelineRun,
) -> Result<Option<archon_core::outcome::DiffStat>, ArchonError> {
    if !run.workspace.join(".git").exists() {
        return Ok(None);
    }
    let raw = ctx
        .store
        .lock()
        .await
        .kv_get(&format!("run_vars:{}", run.id))?;
    let vars: std::collections::BTreeMap<String, String> = raw
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|e| ArchonError::Store(format!("outcome run variables: {e}")))
        })
        .transpose()?
        .unwrap_or_default();
    let Some(base) = vars
        .get("base_branch")
        .filter(|b| !b.is_empty() && !b.starts_with('-'))
    else {
        return Ok(None);
    };
    let output = tokio::process::Command::new("git")
        .args(["diff", "--numstat", &format!("{base}...HEAD"), "--"])
        .current_dir(&run.workspace)
        .output()
        .await
        .map_err(|e| ArchonError::io("read outcome diff", e))?;
    if !output.status.success() {
        tracing::debug!(run=%run.id, stderr=%String::from_utf8_lossy(&output.stderr), "outcome diff unavailable");
        return Ok(None);
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|e| ArchonError::Invalid(format!("git diff is not UTF-8: {e}")))?;
    let mut diff = archon_core::outcome::DiffStat::default();
    for line in raw.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() < 3 {
            return Err(ArchonError::Invalid("git numstat row has no path".into()));
        }
        diff.files += 1;
        if fields[0] == "-" {
            continue;
        }
        diff.added += fields[0]
            .parse::<u32>()
            .map_err(|e| ArchonError::Invalid(format!("git added lines: {e}")))?;
        diff.deleted += fields[1]
            .parse::<u32>()
            .map_err(|e| ArchonError::Invalid(format!("git deleted lines: {e}")))?;
    }
    Ok(Some(diff))
}

#[derive(serde::Deserialize)]
struct Advice {
    #[serde(default)]
    replaces: Option<archon_core::id::LessonId>,
    kind: LessonKind,
    text: String,
}

async fn apply(
    ctx: &DaemonContext,
    run: &PipelineRun,
    item: &QueueItem,
) -> Result<(), ArchonError> {
    assert_eq!(
        run.item, item.id,
        "a distillation applies to its own errand"
    );
    if !matches!(run.state, RunState::Succeeded { .. }) {
        return ctx
            .store
            .lock()
            .await
            .save_learning_value(&format!("learning/applied/{}", run.id), "failed");
    }
    let key = format!("learning/applied/{}", run.id);
    let store = ctx.store.lock().await;
    if store.kv_get(&key)?.is_some() {
        return Ok(());
    }
    let stages = store.stage_runs(&run.id)?;
    let Some(value) = stages.iter().rev().find_map(|s| s.structured.as_ref()) else {
        return Err(ArchonError::Invalid(
            "distillation returned no structured lessons".into(),
        ));
    };
    let lessons = proposed_lessons(value, item)?;
    let waiting =
        |l: &Lesson| l.state == LessonState::Proposed && l.source == LessonSource::Distilled;
    assert!(
        lessons.iter().all(|(l, _)| waiting(l)),
        "distilled advice waits to be confirmed"
    );
    let proposal = value
        .get("pipeline_proposal")
        .filter(|v| !v.is_null())
        .cloned();
    if let Some(value) = &proposal {
        proposals::validate(value)?;
    }
    store.apply_distillation(&run.id, lessons, proposal.as_ref())?;
    drop(store);
    proposals::reconcile(ctx).await?;
    Ok(())
}

fn proposed_lessons(
    value: &serde_json::Value,
    item: &QueueItem,
) -> Result<Vec<(Lesson, Option<archon_core::id::LessonId>)>, ArchonError> {
    let advice: Vec<Advice> = serde_json::from_value(
        value
            .get("lessons")
            .cloned()
            .unwrap_or(serde_json::json!([])),
    )
    .map_err(|e| ArchonError::Invalid(format!("distillation lessons: {e}")))?;
    if advice.len() > 10 {
        return Err(ArchonError::Invalid(
            "distillation returned more than ten lessons".into(),
        ));
    }
    let mut lessons = Vec::new();
    for advice in advice {
        let text = archon_core::lesson::sanitized_text(&advice.text)?;
        let lesson = Lesson::new(
            LessonScope {
                repo: item.repo.clone(),
                pipeline: None,
            },
            advice.kind,
            &text,
            LessonSource::Distilled,
            LessonState::Proposed,
            archon_core::now(),
        )?;
        lessons.push((lesson, advice.replaces));
    }
    assert!(lessons.len() <= 10);
    assert!(lessons.iter().all(|(l, _)| l.scope.repo == item.repo));
    Ok(lessons)
}

async fn report_error(ctx: &DaemonContext, error: &ArchonError) -> Result<(), ArchonError> {
    use archon_core::{
        id::NotificationId,
        notify::{FeedEntry, Notification, NotificationKind},
    };
    let text = format!(
        "Learning could not complete this step: {error}. Run finishing continues; recorded details remain available."
    );
    let key = format!(
        "learning/error/{:016x}",
        super::routing_context::fingerprint(&text)
    );
    let item = {
        let mut store = ctx.store.lock().await;
        if store.kv_get(&key)?.is_some() {
            return Ok(());
        }
        let at = archon_core::now();
        let entry = FeedEntry::Notification(Notification {
            id: NotificationId::new(),
            kind: NotificationKind::Outcome,
            text: crate::executor::clip(&text, 2000),
            item: None,
            run: None,
            at,
        });
        store.append_feed_with_kv(entry, at, &[(&key, "1")])?
    };
    assert!(item.seq > 0);
    crate::feed::publish(ctx, vec![item]);
    Ok(())
}
