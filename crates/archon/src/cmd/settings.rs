//! Inspect or replace the operator overlay at one scope.
use super::{emit, to_json};
use crate::connect::{connect_or_start, connect_running, unexpected};
use anyhow::Context;
use archon_core::protocol::{Reply, RepoRef, Request};
use archon_core::settings::{OperatorSettings, SettingsView};
use std::process::ExitCode;

/// What `archon settings` changes, if anything; clap lets only one through.
pub enum Change {
    Show,
    Set(String),
    Reset,
    ClearDesignRecipe,
}

impl Change {
    pub fn of(set: Option<String>, reset: bool, clear_design_recipe: bool) -> Self {
        match (set, reset, clear_design_recipe) {
            (Some(raw), false, false) => Self::Set(raw),
            (None, true, false) => Self::Reset,
            (None, false, true) => Self::ClearDesignRecipe,
            (None, false, false) => Self::Show,
            _ => unreachable!("clap allows one settings change at a time"),
        }
    }
}

pub async fn run(repo: Option<String>, change: Change, json: bool) -> anyhow::Result<ExitCode> {
    let repo = repo.map(RepoRef);
    let reset = matches!(change, Change::Reset);
    let cleared = matches!(change, Change::ClearDesignRecipe);
    let request = match change {
        Change::Set(raw) => Request::SetSettings {
            legacy_proposals: false,
            repo,
            settings: parse_settings(&raw)?,
        },
        Change::Reset => Request::ResetSettings { repo },
        Change::ClearDesignRecipe => Request::ClearDesignRecipe {
            repo: repo.context("--clear-design-recipe names a repository with --repo")?,
        },
        Change::Show => Request::Settings { repo },
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
    let mut text = describe(&view, reset);
    if cleared {
        let scope = view.repo.as_deref().unwrap_or_default();
        text = format!(
            "Cleared the saved design preview recipe of {scope}; the next live session discovers the stack again.\n{text}"
        );
    }
    emit(json, || to_json(&view), &text)?;
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
