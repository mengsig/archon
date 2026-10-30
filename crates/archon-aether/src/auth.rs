//! Aether's login state per provider (`auth_status`, which `aether auth
//! --json` prints) and the step that logs an engine in.

use serde::Deserialize;

/// One row of `auth_status`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuthInfo {
    pub provider: String,
    /// Whether credentials resolve now; for an engine, whether its CLI is logged in.
    #[serde(alias = "logged_in")]
    pub configured: bool,
    /// Why the provider is not ready, in Aether's words.
    #[serde(default)]
    pub detail: Option<String>,
    /// The CLI that owns the login (`claude`, `codex`), when one does.
    #[serde(default)]
    pub program: Option<String>,
}

/// Why `provider` cannot run, with the step that fixes it; `None` when it is
/// logged in or `auth` does not list it. Logged-in providers are offered instead.
pub fn login_problem(provider: &str, auth: &[AuthInfo]) -> Option<String> {
    assert!(!provider.is_empty(), "a model names its provider");
    let row = auth.iter().find(|row| row.provider == provider)?;
    if row.configured {
        return None;
    }
    let step = match row.program.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(program) => format!("{provider} is not logged in: run `{program}` once"),
        None => format!("{provider} has no credentials: run `archon aether login {provider}`"),
    };
    let others: Vec<String> = auth
        .iter()
        .filter(|other| other.configured && other.provider != provider)
        .map(|other| format!("`{}`", other.provider))
        .collect();
    let instead = match others.is_empty() {
        true => String::new(),
        false => format!(" (or choose a {} model)", others.join(" or ")),
    };
    let detail = row
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| format!("; Aether says: {d}"))
        .unwrap_or_default();
    let problem = format!("{step}{instead}{detail}");
    assert!(
        problem.starts_with(provider),
        "the problem names the provider"
    );
    Some(problem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, configured: bool, program: Option<&str>) -> AuthInfo {
        AuthInfo {
            provider: provider.into(),
            configured,
            detail: None,
            program: program.map(str::to_owned),
        }
    }

    #[test]
    fn an_engine_without_a_login_names_its_cli_and_the_logged_in_alternatives() {
        let auth = [
            row("claude-code", false, Some("claude")),
            row("codex", true, Some("codex")),
            row("anthropic", false, None),
        ];
        assert_eq!(
            login_problem("claude-code", &auth).unwrap(),
            "claude-code is not logged in: run `claude` once (or choose a `codex` model)"
        );
        assert_eq!(login_problem("codex", &auth), None, "logged in");
        assert_eq!(login_problem("openai", &auth), None, "not listed");
        let keyed = login_problem("anthropic", &auth).unwrap();
        assert!(
            keyed.starts_with("anthropic has no credentials: run `archon aether login anthropic`"),
            "{keyed}"
        );
    }

    #[test]
    fn aethers_reason_follows_and_logged_in_is_read_as_configured() {
        let auth: Vec<AuthInfo> = serde_json::from_value(serde_json::json!([
            {"provider": "claude-code", "provider_name": "Claude Code", "logged_in": false,
             "detail": "claude: not logged in", "program": "claude"},
            {"provider": "codex", "configured": false}
        ]))
        .unwrap();
        let problem = login_problem("claude-code", &auth).unwrap();
        assert!(
            problem.ends_with("once; Aether says: claude: not logged in"),
            "{problem}"
        );
        assert!(!problem.contains("choose"), "no provider is logged in");
    }
}
