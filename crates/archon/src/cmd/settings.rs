//! Inspect or replace the operator overlay at one scope.
use super::{emit, to_json};
use crate::connect::{connect_or_start, connect_running, unexpected};
use anyhow::Context;
use archon_core::protocol::{Reply, RepoRef, Request};
use archon_core::settings::{OperatorSettings, SettingsView};
use std::process::ExitCode;

pub async fn run(
    repo: Option<String>,
    set: Option<String>,
    reset: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let repo = repo.map(RepoRef);
    let request = match (set, reset) {
        (Some(raw), false) => Request::SetSettings {
            repo,
            settings: parse_settings(&raw)?,
        },
        (None, true) => Request::ResetSettings { repo },
        (None, false) => Request::Settings { repo },
        (Some(_), true) => unreachable!("clap excludes set with reset"),
    };
    // Like every write, a change starts archond; reading the settings never does.
    let client = match request {
        Request::Settings { .. } => connect_running().await?,
        _ => connect_or_start().await?,
    };
    let view = match client.request(request).await? {
        Reply::Settings(view) => view,
        other => return Err(unexpected(&other, "settings")),
    };
    emit(json, || to_json(&view), &describe(&view, reset))?;
    Ok(ExitCode::SUCCESS)
}

fn parse_settings(raw: &str) -> anyhow::Result<OperatorSettings> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("settings --set expects a JSON object")?;
    let fields = value
        .as_object()
        .context("settings --set expects a JSON object of setting names and values")?;
    for (key, value) in fields {
        let field = serde_json::json!({key: value});
        serde_json::from_value::<OperatorSettings>(field)
            .with_context(|| format!("invalid setting {key:?}"))?;
    }
    serde_json::from_str(raw).context("invalid settings --set object")
}

fn describe(view: &SettingsView, reset: bool) -> String {
    let description = view.describe();
    if !reset {
        return description;
    }
    let scope = view.repo.as_deref().unwrap_or("global");
    format!("Cleared all saved overrides for {scope}; inherited settings now apply.\n{description}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_values_name_the_setting_and_expected_type() {
        for (raw, key, expected) in [
            (
                r#"{"max_parallel_agents":"three"}"#,
                "max_parallel_agents",
                "u32",
            ),
            (r#"{"budgets_enabled":"off"}"#, "budgets_enabled", "boolean"),
            (r#"{"budget_usd_daily":true}"#, "budget_usd_daily", "f64"),
            (
                r#"{"autonomy_max":{"merge":"typo"}}"#,
                "autonomy_max",
                "manual",
            ),
        ] {
            let error = format!("{:#}", parse_settings(raw).unwrap_err());
            assert!(error.contains(key), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn settings_require_an_object_and_accept_zero_as_unlimited() {
        for raw in [
            "[]",
            "null",
            "false",
            "{",
            r#"{"unknown":1}"#,
            r#"{"max_parallel_agents":1,"max_parallel_agents":2}"#,
        ] {
            assert!(parse_settings(raw).is_err(), "{raw}");
        }
        let settings = parse_settings(r#"{"max_parallel_agents":0}"#).unwrap();
        assert_eq!(settings.max_parallel_agents, Some(0));
        assert_eq!(parse_settings("{}").unwrap(), OperatorSettings::default());
    }
}
