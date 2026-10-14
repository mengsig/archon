//! Queue inspection, enqueue and bounded waits.

mod add;
mod steer;
mod wait;

use std::process::ExitCode;

use archon_client::ArchonClient;
use archon_core::protocol::{ItemQuery, Reply, RepoRef, Request};
use archon_core::queue::Caps;

use super::{emit, to_json};
use crate::cli::QueueCommand;
use crate::connect::{connect, reply_data, unexpected};
use crate::format;
use crate::views;

pub async fn run(command: QueueCommand, json: bool) -> anyhow::Result<ExitCode> {
    match command {
        QueueCommand::Add(args) => add::run(*args, json).await?,
        QueueCommand::Steer(args) => steer::run(*args, json).await?,
        QueueCommand::Wait { items, timeout } => return wait::run(items, &timeout, json).await,
        QueueCommand::List { state, repo, limit } => {
            let query = ItemQuery {
                states: views::parse_states(&state)?,
                repo: repo.map(RepoRef),
                limit,
                ..ItemQuery::default()
            };
            let client = connect().await?;
            if json {
                let rows = views::fetch_summaries(&client, query).await?;
                emit(true, || to_json(&rows), "")?;
            } else {
                println!("{}", views::queue_text(&client, query).await?);
            }
        }
        QueueCommand::Show { item, full } => {
            let client = connect().await?;
            let item = views::resolve_item(&client, &item).await?;
            if json {
                let reply = client.request(Request::GetItem { item }).await?;
                emit(true, || reply_data(&reply), "")?;
            } else {
                println!("{}", views::item_text(&client, item, full).await?);
            }
        }
        QueueCommand::Cancel { item, reason } => {
            let client = connect().await?;
            let item = views::resolve_item(&client, &item).await?;
            let done = format!("Cancelled {item}.");
            change(&client, Request::CancelItem { item, reason }, &done, json).await?;
        }
        QueueCommand::Retry {
            item,
            fresh,
            append,
        } => {
            let client = connect().await?;
            let item = views::resolve_item(&client, &item).await?;
            let done = match fresh {
                true => format!("Queued {item} again, fresh from the base branch."),
                false => format!("Queued {item} again."),
            };
            let request = Request::RetryItem {
                item,
                model_plan: None,
                model_reasons: None,
                fresh,
                brief_append: append,
            };
            change(&client, request, &done, json).await?;
        }
        QueueCommand::Caps {
            max_parallel_agents,
            max_per_repo,
            budget_daily,
            budget_total,
        } => {
            caps(
                max_parallel_agents,
                max_per_repo,
                budget_daily,
                budget_total,
                json,
            )
            .await?
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Send an item change that answers with the item (or `ok`).
async fn change(
    client: &ArchonClient,
    request: Request,
    done: &str,
    json: bool,
) -> anyhow::Result<()> {
    let reply = client.request(request).await?;
    if !matches!(reply, Reply::Item(_) | Reply::Ok) {
        return Err(unexpected(&reply, "item"));
    }
    emit(json, || reply_data(&reply), done)
}

/// Show the caps, or change some of them: `SetCaps` takes the full set, so
/// the current caps are read first.
async fn caps(
    agents: Option<u32>,
    per_repo: Option<u32>,
    daily: Option<f64>,
    total: Option<f64>,
    json: bool,
) -> anyhow::Result<()> {
    let client = connect().await?;
    let mut caps = views::fetch_status(&client).await?.caps;
    let changed = agents.is_some() || per_repo.is_some() || daily.is_some() || total.is_some();
    if changed {
        apply_caps(&mut caps, agents, per_repo, daily, total)?;
        set_caps(&client, &caps).await?;
    }
    let text = format!(
        "{}caps: {}\nbudgets: {}",
        if changed {
            "Until archond restarts:\n"
        } else {
            ""
        },
        format::caps_line(&caps),
        format::budgets_line(&caps)
    );
    emit(json, || to_json(&caps), &text)
}

fn apply_caps(
    caps: &mut Caps,
    agents: Option<u32>,
    per_repo: Option<u32>,
    daily: Option<f64>,
    total: Option<f64>,
) -> anyhow::Result<()> {
    if agents == Some(0) {
        return Err(archon_core::ArchonError::Invalid(
            "--max-parallel-agents must be at least 1".into(),
        )
        .into());
    }
    for (flag, amount) in [("--budget-daily", daily), ("--budget-total", total)] {
        if let Some(amount) = amount
            && !(amount.is_finite() && amount >= 0.0)
        {
            return Err(archon_core::ArchonError::Invalid(format!(
                "{flag} must be a dollar amount of 0 (unlimited) or more"
            ))
            .into());
        }
    }
    caps.max_parallel_agents = agents.unwrap_or(caps.max_parallel_agents);
    caps.max_per_repo = per_repo.unwrap_or(caps.max_per_repo);
    caps.budget_usd_daily = daily.unwrap_or(caps.budget_usd_daily);
    caps.budget_usd_total = total.unwrap_or(caps.budget_usd_total);
    Ok(())
}

async fn set_caps(client: &ArchonClient, caps: &Caps) -> anyhow::Result<()> {
    let request = Request::SetCaps { caps: caps.clone() };
    match client.request(request).await? {
        Reply::Ok => Ok(()),
        other => Err(unexpected(&other, "ok")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_change_only_what_was_given() {
        let mut caps = Caps {
            budget_usd_total: 50.0,
            ..Caps::default()
        };
        apply_caps(&mut caps, Some(5), None, Some(12.5), None).unwrap();
        assert_eq!((caps.max_parallel_agents, caps.max_per_repo), (5, 0));
        assert_eq!((caps.budget_usd_daily, caps.budget_usd_total), (12.5, 50.0));
        assert!(apply_caps(&mut caps, Some(0), None, None, None).is_err());
        assert!(apply_caps(&mut caps, None, None, Some(f64::NAN), None).is_err());
        assert!(apply_caps(&mut caps, None, None, Some(-1.0), None).is_err());
        assert!(apply_caps(&mut caps, None, None, None, Some(f64::INFINITY)).is_err());
        apply_caps(&mut caps, None, None, None, Some(75.0)).unwrap();
        assert_eq!(caps.budget_usd_total, 75.0);
        assert_eq!(
            caps.max_parallel_agents, 5,
            "a refused change changes nothing"
        );
    }
}
