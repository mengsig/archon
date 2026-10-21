//! Decode files and send the same enqueue request as MCP.

use archon_client::ArchonClient;
use archon_core::ArchonError;
use archon_core::autonomy::AutonomyPatch;
use archon_core::id::QueueItemId;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::pull_request::OntoTarget;

use crate::cli::QueueAddArgs;
use crate::cmd::{emit, read_map, read_text};
use crate::connect::{connect_or_start, reply_data, unexpected};
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
    let client = connect_or_start().await?;
    let mut depends_on = Vec::with_capacity(args.depends_on.len());
    for dependency in args.depends_on {
        depends_on.push(views::resolve_item(&client, &dependency).await?);
    }
    let onto = match args.onto.as_deref() {
        Some(text) => Some(onto_target(&client, text).await?),
        None => None,
    };
    let request = EnqueueRequest {
        onto,
        stage_timeouts: args.stage_timeout.iter().cloned().collect(),
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
    let (Reply::Item(item) | Reply::ItemBudgetsOff(item)) = &reply else {
        return Err(unexpected(&reply, "item"));
    };
    super::note_budgets_off(&reply);
    emit(json, || reply_data(&reply), &item.id.to_string())
}

/// `--onto`: an item id or prefix, resolved now, else a number or a branch.
async fn onto_target(client: &ArchonClient, text: &str) -> anyhow::Result<OntoTarget> {
    let text = text.trim();
    if text.starts_with(QueueItemId::PREFIX) {
        return Ok(OntoTarget::Item(views::resolve_item(client, text).await?));
    }
    Ok(number_or_branch(text)?)
}

/// A pull request number, `123` or `#123`, else the head branch `text` names.
fn number_or_branch(text: &str) -> Result<OntoTarget, ArchonError> {
    if text.is_empty() {
        return Err(ArchonError::Invalid(
            "--onto names an item, a pull request number or a branch".into(),
        ));
    }
    let digits = text.strip_prefix('#').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(OntoTarget::Branch(text.to_owned()));
    }
    let number = digits
        .parse()
        .map_err(|e| ArchonError::Invalid(format!("--onto {text}: {e}")))?;
    Ok(OntoTarget::Pr(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onto_reads_a_number_with_or_without_hash_and_any_other_text_as_a_branch() {
        assert_eq!(number_or_branch("100").unwrap(), OntoTarget::Pr(100));
        assert_eq!(number_or_branch("#7").unwrap(), OntoTarget::Pr(7));
        let branch = number_or_branch("feat/x").unwrap();
        assert_eq!(branch, OntoTarget::Branch("feat/x".into()));
        assert_eq!(
            number_or_branch("#").unwrap(),
            OntoTarget::Branch("#".into())
        );
        assert!(number_or_branch("").is_err());
        assert!(number_or_branch("99999999999999999999999").is_err());
    }
}
