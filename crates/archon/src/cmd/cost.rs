//! `archon cost`: spend over a range of days, grouped, as the budgets
//! count it (ARCHITECTURE §5.3).

use std::process::ExitCode;

use anyhow::Context as _;
use archon_core::cost::{CostGroup, CostQuery, DayBound};
use archon_core::protocol::{Reply, Request};

use super::{emit, to_json};
use crate::cli::CostBy;
use crate::connect::{connect_running, unexpected};
use crate::format;

pub async fn run(
    since: Option<&str>,
    until: Option<&str>,
    by: CostBy,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let query = CostQuery {
        since: bound("--since", since)?,
        until: bound("--until", until)?,
        by: group(by),
    };
    let client = connect_running().await?;
    let report = match client.request(Request::CostReport(query)).await? {
        Reply::CostReport(report) => *report,
        other => return Err(unexpected(&other, "cost report")),
    };
    assert_eq!(report.by, query.by, "the report groups as asked");
    emit(json, || to_json(&report), &format::cost(&report))?;
    Ok(ExitCode::SUCCESS)
}

/// Checked before connecting, so a typo asks archond nothing.
fn bound(flag: &str, text: Option<&str>) -> anyhow::Result<Option<DayBound>> {
    text.map(|text| DayBound::parse(text).with_context(|| format!("{flag} {text:?}")))
        .transpose()
}

fn group(by: CostBy) -> CostGroup {
    match by {
        CostBy::Day => CostGroup::Day,
        CostBy::Repo => CostGroup::Repo,
        CostBy::Pipeline => CostGroup::Pipeline,
        CostBy::Model => CostGroup::Model,
        CostBy::Provider => CostGroup::Provider,
        CostBy::Item => CostGroup::Item,
        CostBy::Stage => CostGroup::Stage,
    }
}

pub async fn evidence(
    repo: Option<String>,
    pipeline: Option<String>,
    stage: Option<String>,
    since: Option<&str>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let query = archon_core::model_evidence::ModelEvidenceQuery {
        repo: repo.map(archon_core::protocol::RepoRef),
        pipeline,
        stage,
        since: bound("--since", since)?,
    };
    let client = connect_running().await?;
    let report = match client.request(Request::ModelEvidence(query)).await? {
        Reply::ModelEvidence(report) => *report,
        other => return Err(unexpected(&other, "model evidence")),
    };
    emit(json, || to_json(&report), &report.text())?;
    Ok(ExitCode::SUCCESS)
}
