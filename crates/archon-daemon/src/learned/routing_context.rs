use archon_core::{ArchonError, autonomy::ReviewLevel, id::QueueItemId, outcome::RouteContext};
use archon_pipeline::PipelineSpec;
use archon_queue::Store;

pub(super) fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(14695981039346656037u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(1099511628211)
    })
}

fn context(
    spec: &PipelineSpec,
    review: ReviewLevel,
    tags: &[String],
) -> Result<RouteContext, ArchonError> {
    let mut contents = spec.manifest()?;
    for (name, prompt) in &spec.prompts {
        contents.push_str(name);
        contents.push('\0');
        contents.push_str(prompt);
    }
    let class = tags
        .iter()
        .find_map(|t| t.strip_prefix("task:"))
        .unwrap_or("general");
    if class.is_empty()
        || class.len() > 64
        || !class
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ArchonError::Invalid(
            "task class must be 1..64 letters, digits, '-' or '_'".into(),
        ));
    }
    let context = RouteContext {
        revision: format!("{:016x}", fingerprint(&contents)),
        review: review.to_string(),
        task_class: class.into(),
    };
    assert!(!contents.is_empty());
    assert!(!context.review.is_empty());
    Ok(context)
}

fn hint_context(
    store: &Store,
    item: &QueueItemId,
    spec: &PipelineSpec,
    review: ReviewLevel,
    tags: &[String],
) -> Result<(), ArchonError> {
    store.save_learning_value(
        &format!("learning/context/{item}"),
        &serde_json::to_string(&context(spec, review, tags)?)?,
    )
}

pub(crate) async fn freeze_context(
    ctx: &crate::context::DaemonContext,
    run: &archon_core::id::PipelineRunId,
    item: &archon_core::queue::QueueItem,
    spec: &PipelineSpec,
) -> Result<(), ArchonError> {
    if !ctx.config.learning.enabled {
        return Ok(());
    }
    let context = context(spec, item.autonomy.review, &item.tags)?;
    ctx.store.lock().await.save_learning_value(
        &format!("learning/run-context/{run}"),
        &serde_json::to_string(&context)?,
    )
}

pub(crate) async fn prepare_routing(
    ctx: &crate::context::DaemonContext,
    item: &QueueItemId,
    resolved: &crate::knobs::Resolved,
    tags: &[String],
) -> Result<(), ArchonError> {
    if !ctx.config.learning.enabled {
        return Ok(());
    }
    let store = ctx.store.lock().await;
    super::routing::hint_size(&store, item, tags)?;
    hint_context(&store, item, &resolved.spec, resolved.autonomy.review, tags)
}
