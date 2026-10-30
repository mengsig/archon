//! Queue inspection, enqueue and bounded waits.

mod add;
mod steer;
mod wait;

use std::process::ExitCode;

use archon_client::ArchonClient;
use archon_core::protocol::{ItemQuery, Reply, RepoRef, Request};
use archon_core::queue::Caps;

use super::{emit, to_json};
use crate::cli::{Budgets, QueueCommand};
use crate::connect::{connect_or_start, connect_running, reply_data, unexpected};
use crate::format;
use crate::views;

pub async fn run(command: QueueCommand, json: bool) -> anyhow::Result<ExitCode> {
    match command {
        QueueCommand::Add(args) => add::run(*args, json).await?,
        QueueCommand::Steer(args) => steer::run(*args, json).await?,
        QueueCommand::Wait { items, timeout } => return wait::run(items, &timeout, json).await,
        QueueCommand::List { state, repo, limit } => list(&state, repo, limit, json).await?,
        QueueCommand::Show { item, full } => show(&item, full, json).await?,
        QueueCommand::Cancel { item, reason } => {
            let client = connect_or_start().await?;
            let item = views::resolve_item(&client, &item).await?;
            let done = format!("Cancelled {item}.");
            change(&client, Request::CancelItem { item, reason }, &done, json).await?;
        }
        QueueCommand::Retry {
            item,
            fresh,
            append,
            stage_timeout,
        } => retry(&item, fresh, append, stage_timeout, json).await?,
        QueueCommand::Refresh { item } => refresh(&item, json).await?,
        QueueCommand::Caps {
            max_parallel_agents,
            max_per_repo,
            budget_daily,
            budget_total,
            budgets,
        } => {
            caps(
                max_parallel_agents,
                max_per_repo,
                budget_daily,
                budget_total,
                budgets,
                json,
            )
            .await?
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn list(
    state: &[String],
    repo: Option<String>,
    limit: Option<usize>,
    json: bool,
) -> anyhow::Result<()> {
    let query = ItemQuery {
        states: views::parse_states(state)?,
        repo: repo.map(RepoRef),
        limit,
        ..ItemQuery::default()
    };
    let client = connect_running().await?;
    if json {
        let rows = views::fetch_summaries(&client, query).await?;
        return emit(true, || to_json(&rows), "");
    }
    println!("{}", views::queue_text(&client, query).await?);
    Ok(())
}

async fn show(item: &str, full: bool, json: bool) -> anyhow::Result<()> {
    let client = connect_running().await?;
    let item = views::resolve_item(&client, item).await?;
    if json {
        let reply = client.request(Request::GetItem { item }).await?;
        return emit(true, || reply_data(&reply), "");
    }
    println!("{}", views::item_text(&client, item, full).await?);
    Ok(())
}

async fn retry(
    item: &str,
    fresh: bool,
    append: Option<String>,
    stage_timeout: Vec<(String, u32)>,
    json: bool,
) -> anyhow::Result<()> {
    let client = connect_or_start().await?;
    let item = views::resolve_item(&client, item).await?;
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
        stage_timeouts: stage_timeout.into_iter().collect(),
    };
    change(&client, request, &done, json).await
}

async fn refresh(item: &str, json: bool) -> anyhow::Result<()> {
    let client = connect_or_start().await?;
    let item = views::resolve_item(&client, item).await?;
    let reply = client
        .request(Request::RefreshItem {
            item: item.clone(),
            model_plan: None,
            model_reasons: None,
            stage_timeouts: Default::default(),
        })
        .await?;
    let Reply::Item(refresh) = &reply else {
        return Err(unexpected(&reply, "item"));
    };
    let done = match refresh.id == item {
        true => format!("Refreshed {item} in place; nothing was queued."),
        false => format!(
            "Queued {} to merge the base branch into {item}.",
            refresh.id
        ),
    };
    emit(json, || reply_data(&reply), &done)
}

/// Send an item change that answers with the item (or `ok`).
async fn change(
    client: &ArchonClient,
    request: Request,
    done: &str,
    json: bool,
) -> anyhow::Result<()> {
    let reply = client.request(request).await?;
    if !matches!(reply, Reply::Item(_) | Reply::ItemBudgetsOff(_) | Reply::Ok) {
        return Err(unexpected(&reply, "item"));
    }
    note_budgets_off(&reply);
    emit(json, || reply_data(&reply), done)
}

/// Say on stderr that a budget just set is not enforced, as stdout is data.
pub(super) fn note_budgets_off(reply: &Reply) {
    if matches!(reply, Reply::ItemBudgetsOff(_)) {
        eprintln!("note: {}", archon_core::protocol::BUDGETS_OFF_NOTE);
    }
}

/// Show the caps, or change some of them: `SetCaps` takes the full set, so
/// the current caps are read first. Only a change starts archond.
async fn caps(
    agents: Option<u32>,
    per_repo: Option<u32>,
    daily: Option<f64>,
    total: Option<f64>,
    budgets: Option<Budgets>,
    json: bool,
) -> anyhow::Result<()> {
    let changed = agents.is_some()
        || per_repo.is_some()
        || daily.is_some()
        || total.is_some()
        || budgets.is_some();
    let client = match changed {
        true => connect_or_start().await?,
        false => connect_running().await?,
    };
    let mut caps = views::fetch_status(&client).await?.caps;
    if changed {
        apply_caps(&mut caps, agents, per_repo, daily, total, budgets)?;
        set_caps(&client, &caps).await?;
    }
    let text = format!(
        "{}caps: {}\nbudgets: {}",
        if changed {
            "Saved across daemon restarts (archon settings --reset clears all global overrides):\n"
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
    budgets: Option<Budgets>,
) -> anyhow::Result<()> {
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
    caps.budgets_enabled = budgets.map_or(caps.budgets_enabled, |b| b == Budgets::On);
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
        apply_caps(&mut caps, Some(5), None, Some(12.5), None, None).unwrap();
        assert_eq!((caps.max_parallel_agents, caps.max_per_repo), (5, 0));
        assert_eq!((caps.budget_usd_daily, caps.budget_usd_total), (12.5, 50.0));
        apply_caps(&mut caps, Some(0), None, None, None, None).unwrap();
        assert_eq!(caps.max_parallel_agents, 0, "0 is unlimited");
        assert!(apply_caps(&mut caps, None, None, Some(f64::NAN), None, None).is_err());
        assert!(apply_caps(&mut caps, None, None, Some(-1.0), None, None).is_err());
        assert!(apply_caps(&mut caps, None, None, None, Some(f64::INFINITY), None).is_err());
        apply_caps(&mut caps, None, None, None, Some(75.0), None).unwrap();
        assert_eq!(caps.budget_usd_total, 75.0);
        assert_eq!(
            caps.max_parallel_agents, 0,
            "a refused change changes nothing"
        );
    }

    #[test]
    fn budgets_switch_changes_only_the_switch() {
        let mut caps = Caps::default();
        assert!(caps.budgets_enabled);
        apply_caps(&mut caps, None, None, None, None, Some(Budgets::Off)).unwrap();
        assert!(!caps.budgets_enabled);
        apply_caps(&mut caps, Some(3), None, None, None, None).unwrap();
        assert!(!caps.budgets_enabled, "other changes leave the switch");
        apply_caps(&mut caps, None, None, None, None, Some(Budgets::On)).unwrap();
        assert!(caps.budgets_enabled);
    }
}
