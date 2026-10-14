//! Decode files and send the same enqueue request as MCP.

use archon_core::ArchonError;
use archon_core::autonomy::AutonomyPatch;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};

use crate::cli::QueueAddArgs;
use crate::cmd::{emit, read_map, read_text};
use crate::connect::{connect, reply_data, unexpected};
use crate::views;

pub async fn run(args: QueueAddArgs, json: bool) -> anyhow::Result<()> {
    let brief = read_text(&args.brief_file, "brief")?;
    let model_plan = args.model_plan_file.as_deref().map(read_map).transpose()?;
    let model_plan = model_plan.unwrap_or_default();
    let model_reasons = args
        .model_reasons_file
        .as_deref()
        .map(read_map)
        .transpose()?;
    let priority = serde_json::from_value(serde_json::Value::String(args.priority))
        .map_err(|e| ArchonError::Invalid(format!("--priority: {e}")))?;
    archon_core::queue::check_enqueue(&args.title, &brief, &args.tag, args.budget_usd)?;
    let client = connect().await?;
    let mut depends_on = Vec::with_capacity(args.depends_on.len());
    for dependency in args.depends_on {
        depends_on.push(views::resolve_item(&client, &dependency).await?);
    }
    let request = EnqueueRequest {
        title: args.title,
        repo: args.repo.map(RepoRef),
        pipeline: args.pipeline,
        brief,
        model_plan,
        model_reasons,
        priority,
        depends_on,
        tags: args.tag,
        idempotency_key: args.idempotency_key,
        budget_usd: args.budget_usd,
        autonomy: AutonomyPatch {
            review: args.review,
            proposal: args.proposal,
            report: args.report,
            merge: args.merge,
            tools: args.tools,
        },
        origin: None,
    };
    let reply = client.request(Request::Enqueue(request)).await?;
    let Reply::Item(item) = &reply else {
        return Err(unexpected(&reply, "item"));
    };
    emit(json, || reply_data(&reply), &item.id.to_string())
}
