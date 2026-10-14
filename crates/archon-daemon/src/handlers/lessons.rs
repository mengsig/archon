//! Lesson mutations and inspectable evidence; operator edits always take precedence.
use crate::context::DaemonContext;
use archon_core::error::ArchonError;
use archon_core::id::{LessonId, QueueItemId};
use archon_core::lesson::{
    Lesson, LessonDraft, LessonPatch, LessonQuery, LessonScope, LessonSource, LessonState,
};
use archon_core::protocol::{ClientKind, Reply, RepoRef};

pub(super) async fn record(
    ctx: &DaemonContext,
    client: ClientKind,
    draft: LessonDraft,
) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    let repo = source_repository(&store, client, &draft)?;
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
        if source == LessonSource::Operator {
            LessonState::Active
        } else {
            LessonState::Proposed
        },
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

fn source_repository(
    store: &archon_queue::Store,
    client: ClientKind,
    draft: &LessonDraft,
) -> Result<Option<archon_core::id::RepoId>, ArchonError> {
    let mut repo = draft
        .repo
        .as_ref()
        .map(|r| store.repo(r))
        .transpose()?
        .map(|r| r.id);
    if client != ClientKind::ArchonCli && draft.item.is_none() {
        return Err(ArchonError::Invalid(
            "orchestrator lessons require their source item".into(),
        ));
    }
    if let Some(item) = &draft.item {
        let item = store.item(item)?;
        if repo.is_some() && item.repo != repo {
            return Err(ArchonError::Invalid(
                "lesson item belongs to another repository".into(),
            ));
        }
        if client != ClientKind::ArchonCli {
            repo = Some(item.repo.ok_or_else(|| {
                ArchonError::Invalid("orchestrator lessons require a repository item".into())
            })?);
        }
    }
    assert!(client == ClientKind::ArchonCli || repo.is_some());
    assert!(client == ClientKind::ArchonCli || draft.item.is_some());
    Ok(repo)
}

pub(super) async fn list(ctx: &DaemonContext, query: &LessonQuery) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    Ok(Reply::Lessons(store.lessons(query)?))
}

pub(super) async fn update(
    ctx: &DaemonContext,
    lesson: &LessonId,
    patch: LessonPatch,
) -> Result<Reply, ArchonError> {
    let store = ctx.store.lock().await;
    Ok(Reply::Lesson(store.update_lesson(lesson, patch)?))
}

pub(super) async fn why(ctx: &DaemonContext, item: &QueueItemId) -> Result<Reply, ArchonError> {
    Ok(Reply::LessonsWhy(Box::new(
        ctx.store.lock().await.lesson_why(item)?,
    )))
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
            if let Some(chosen) = archon_queue::store::preferred_route(&choices) {
                stats.routes.push(chosen.route.clone());
            }
        }
    }
    assert!(stats.corrected <= stats.outcomes);
    assert!(stats.merged <= stats.outcomes);
    Ok(Reply::LessonStats(Box::new(stats)))
}
