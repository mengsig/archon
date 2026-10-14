//! Lesson mutations and inspectable evidence; operator edits always take precedence.
use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use archon_core::id::{LessonId, QueueItemId};
use archon_core::lesson::{
    Lesson, LessonDraft, LessonPatch, LessonQuery, LessonScope, LessonSource, LessonState,
    LessonWhy,
};
use archon_core::protocol::{ClientKind, Reply, RepoRef};

pub(super) async fn record(
    ctx: &DaemonContext,
    client: ClientKind,
    draft: LessonDraft,
) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    store.configure_learning(&ctx.config.learning)?;
    let repo = draft
        .repo
        .as_ref()
        .map(|r| store.repo(r))
        .transpose()?
        .map(|r| r.id);
    if let Some(item) = &draft.item {
        let item = store.item(item)?;
        if repo.is_some() && item.repo != repo {
            return Err(ArchonError::Invalid(
                "lesson item belongs to another repository".into(),
            ));
        }
    }
    let source = if client == ClientKind::ArchonCli {
        LessonSource::Operator
    } else {
        LessonSource::Orchestrator
    };
    let text = archon_core::lesson::sanitized_text(&draft.text)?;
    let mut lesson = Lesson::new(
        LessonScope {
            repo,
            pipeline: draft.pipeline,
        },
        draft.kind,
        &text,
        source,
        LessonState::Active,
        archon_core::now(),
    )?;
    lesson.item = draft.item;
    if source == LessonSource::Operator {
        lesson.confirmed_at = Some(lesson.created_at);
        lesson.confidence = 1.0;
    }
    assert_eq!(lesson.uses, 0);
    assert_eq!(lesson.source, source);
    Ok(Reply::Lesson(store.record_lesson(lesson)?))
}

pub(super) async fn list(ctx: &DaemonContext, query: &LessonQuery) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    store.configure_learning(&ctx.config.learning)?;
    store.refresh_lessons(archon_core::now())?;
    Ok(Reply::Lessons(store.lessons(query)?))
}

pub(super) async fn update(
    ctx: &DaemonContext,
    lesson: &LessonId,
    patch: LessonPatch,
) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    store.configure_learning(&ctx.config.learning)?;
    Ok(Reply::Lesson(store.update_lesson(lesson, patch)?))
}

pub(super) async fn why(ctx: &DaemonContext, item: &QueueItemId) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    let runs = store.runs_for_item(item)?;
    let run = runs.last();
    let lessons = run
        .map(|r| store.used_lessons(&r.id))
        .transpose()?
        .unwrap_or_default();
    let frozen = run
        .map(|r| store.kv_get(&format!("learning/routing-run/{}", r.id)))
        .transpose()?
        .flatten();
    let routing = frozen
        .or(store.kv_get(&format!("learning/routing/{item}"))?)
        .map(|raw| {
            serde_json::from_str(&raw)
                .map_err(|e| ArchonError::Store(format!("routing evidence: {e}")))
        })
        .transpose()?
        .unwrap_or_default();
    assert!(lessons.len() <= archon_core::lesson::CONTRACT_LESSONS);
    assert!(run.is_none_or(|r| r.item == *item));
    Ok(Reply::LessonsWhy(Box::new(LessonWhy {
        item: item.clone(),
        run: run.map(|r| r.id.clone()),
        lessons,
        routing,
    })))
}

pub(super) async fn stats(
    ctx: &DaemonContext,
    repo: Option<&RepoRef>,
) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    let repo = repo.map(|r| store.repo(r)).transpose()?;
    let mut stats = store.learning_stats(repo.as_ref().map(|r| &r.id))?;
    for pipeline in stats
        .pipelines
        .iter_mut()
        .filter(|_| ctx.config.learning.enabled)
    {
        let repo = pipeline
            .repo
            .as_ref()
            .map(|id| store.repo(&crate::resolve::repo_ref(id)))
            .transpose()?;
        let env = crate::resolve::RepoEnv::new(repo)?;
        let policy = env.policy(&ctx.config);
        pipeline.estimated_saving_usd =
            store.estimated_model_saving(pipeline.repo.as_ref(), &pipeline.pipeline, &policy)?;
        let outcomes = store.outcomes(pipeline.repo.as_ref())?;
        let stages: std::collections::BTreeSet<_> = outcomes
            .iter()
            .filter(|o| o.pipeline == pipeline.pipeline)
            .flat_map(|o| o.stages.iter().map(|s| s.stage.clone()))
            .collect();
        for stage in stages {
            let mut choices =
                store.route_evidence(pipeline.repo.as_ref(), &pipeline.pipeline, &stage, None)?;
            choices.retain(|choice| policy.check(&choice.route.choice).is_ok());
            let best = choices
                .iter()
                .filter(|e| e.route.runs >= 5)
                .map(|e| f64::from(e.route.passed) / f64::from(e.route.runs))
                .max_by(f64::total_cmp);
            if let Some(best) = best
                && let Some(chosen) = choices.into_iter().find(|e| {
                    e.route.runs >= 5
                        && f64::from(e.route.passed) / f64::from(e.route.runs) + 0.05 >= best
                })
            {
                stats.routes.push(chosen.route);
            }
        }
    }
    assert!(stats.corrected <= stats.outcomes);
    assert!(stats.merged <= stats.outcomes);
    Ok(Reply::LessonStats(Box::new(stats)))
}
