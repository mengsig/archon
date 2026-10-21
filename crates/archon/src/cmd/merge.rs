//! `archon merge`: the operator merges an item's pull request through archond;
//! `archon promote`: the integration branch into the release branch.

use std::process::ExitCode;

use archon_core::integration::PromotionOutcome;
use archon_core::merge::MergeState;
use archon_core::protocol::RepoRef;

use super::{emit, to_json};
use crate::connect::connect_or_start;
use crate::{format, ops, views};

/// Fails unless archond merged: a refusal names its reason and changes nothing.
pub async fn run(item: &str, ignore_checks: bool, json: bool) -> anyhow::Result<ExitCode> {
    let client = connect_or_start().await?;
    let item = views::resolve_item(&client, item).await?;
    let merge = ops::merge(&client, item, ignore_checks, false).await?;
    emit(json, || to_json(&merge), &format::merge(&merge))?;
    Ok(match merge.state {
        MergeState::Merged { .. } => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

/// Shows the branch a local-ship item left, with its diffstat; fails when the
/// item has no local branch.
pub async fn diff(item: &str, full: bool, json: bool) -> anyhow::Result<ExitCode> {
    let client = connect_or_start().await?;
    let item = views::resolve_item(&client, item).await?;
    let review = ops::diff(&client, item, full).await?;
    emit(
        json,
        || to_json(&review),
        &crate::wrap::sanitize(&review.describe()),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Fails unless the release branch holds the integration head afterwards;
/// a refusal names its reason and changes nothing.
pub async fn promote(repo: &str, json: bool) -> anyhow::Result<ExitCode> {
    let client = connect_or_start().await?;
    let promotion = ops::promote(&client, RepoRef(repo.to_owned())).await?;
    emit(
        json,
        || to_json(&promotion),
        &crate::wrap::sanitize(&promotion.describe()),
    )?;
    Ok(match promotion.outcome {
        PromotionOutcome::Merged { .. } | PromotionOutcome::UpToDate { .. } => ExitCode::SUCCESS,
        PromotionOutcome::AwaitingOperator => ExitCode::FAILURE,
    })
}
