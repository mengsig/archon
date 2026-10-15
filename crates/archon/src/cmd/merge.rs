//! `archon merge`: the operator merges an item's pull request through archond.

use std::process::ExitCode;

use archon_core::merge::MergeState;

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
