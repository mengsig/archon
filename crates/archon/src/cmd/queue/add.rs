//! Decode files and send the same enqueue request as MCP.

use std::io::Read as _;
use std::path::Path;

use anyhow::Context as _;
use archon_core::ArchonError;
use archon_core::autonomy::AutonomyPatch;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use serde::de::DeserializeOwned;

use crate::cli::QueueAddArgs;
use crate::cmd::emit;
use crate::connect::{connect, reply_data, unexpected};
use crate::views;

pub async fn run(args: QueueAddArgs, json: bool) -> anyhow::Result<()> {
    let brief = read_brief(&args.brief_file)?;
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

fn read_brief(path: &Path) -> anyhow::Result<String> {
    if path == Path::new("-") {
        let mut brief = String::new();
        std::io::stdin()
            .read_to_string(&mut brief)
            .context("reading brief from stdin")?;
        return Ok(brief);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading brief {}", path.display()))
}

fn read_map<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match path.extension().and_then(|s| s.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|e| ArchonError::Invalid(format!("{}: {e}", path.display())).into()),
        Some("toml") => toml::from_str(&text)
            .map_err(|e| ArchonError::Invalid(format!("{}: {e}", path.display())).into()),
        _ => Err(ArchonError::Invalid(format!(
            "{} must have a .json or .toml extension",
            path.display()
        ))
        .into()),
    }
}
