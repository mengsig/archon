//! Whether the configured orchestrator model can run: Aether lists it and
//! its engine is logged in. Without Aether, whether its engine's CLI is on PATH.

use std::path::Path;

use archon_aether::AetherClient;
use archon_aether::auth::{AuthInfo, login_problem};
use archon_aether::wire::ModelInfo;
use archon_core::model::ModelRef;

use super::{Check, Level, check, find_program};

pub(super) const NAME: &str = "engine";
/// The CLI behind each engine provider Aether ships, for when Aether cannot say.
const ENGINE_PROGRAMS: [(&str, &str); 2] = [("claude-code", "claude"), ("codex", "codex")];

/// The readiness of `model`, asked of `aether` when it is reachable.
pub(crate) async fn check_engine(aether: Option<&AetherClient>, model: &ModelRef) -> Check {
    let Some(client) = aether else {
        return offline_verdict(model, |program| find_program(Path::new(program)).is_some());
    };
    let models = match client.list_models().await {
        Ok(models) => models,
        Err(e) => return check(NAME, Level::Warn, format!("{model}: not checked: {e}")),
    };
    let auth = match client.auth_status().await {
        Ok(auth) => auth,
        Err(e) => return unchecked_login(engine_verdict(model, &models, None), &e.to_string()),
    };
    engine_verdict(model, &models, Some(&auth))
}

/// `verdict` from the model list alone, as Aether did not say whether the
/// engine is logged in (`why`): a ready model is only a warning then.
fn unchecked_login(verdict: Check, why: &str) -> Check {
    assert_eq!(verdict.name, NAME, "only the engine verdict");
    if verdict.level != Level::Ok {
        return verdict;
    }
    let detail = format!("{}, but its login was not checked: {why}", verdict.detail);
    check(NAME, Level::Warn, detail)
}

/// The verdict on `model` from Aether's catalogue and, when it served one, its logins.
fn engine_verdict(model: &ModelRef, models: &[ModelInfo], auth: Option<&[AuthInfo]>) -> Check {
    assert!(!model.provider.is_empty(), "a model names its provider");
    if let Some(problem) = auth.and_then(|auth| login_problem(&model.provider, auth)) {
        return check(NAME, Level::Fail, format!("{model}: {problem}"));
    }
    let listed = models.iter().find(|info| {
        info.model.provider == model.provider
            && (info.model.model == model.model || info.aliases.contains(&model.model))
    });
    let fix = "set [orchestrator] model to one `archon aether models` lists";
    let Some(info) = listed else {
        let detail = format!("Aether lists no model {model}; {fix}");
        return check(NAME, Level::Fail, detail);
    };
    assert_eq!(info.model.provider, model.provider, "the match is exact");
    if !info.configured {
        let detail = format!(
            "{model}: its provider has no credentials; `archon aether auth` says why, or {fix}"
        );
        return check(NAME, Level::Fail, detail);
    }
    check(NAME, Level::Ok, format!("{model} is ready"))
}

/// Without Aether only a missing engine CLI is certain; `on_path` finds a program.
fn offline_verdict(model: &ModelRef, on_path: impl Fn(&str) -> bool) -> Check {
    let program = ENGINE_PROGRAMS
        .iter()
        .find(|(provider, _)| *provider == model.provider)
        .map(|(_, program)| *program);
    let Some(program) = program.filter(|program| !on_path(program)) else {
        let detail = format!("{model}: login not checked: Aether is unreachable");
        return check(NAME, Level::Warn, detail);
    };
    let others: Vec<String> = ENGINE_PROGRAMS
        .iter()
        .filter(|(provider, other)| *provider != model.provider && on_path(other))
        .map(|(provider, _)| format!("`{provider}`"))
        .collect();
    let instead = match others.is_empty() {
        true => String::new(),
        false => format!(" (or choose a {} model)", others.join(" or ")),
    };
    let detail = format!(
        "{model}: `{program}` is not on PATH, so the orchestrator cannot run: install it and \
         run `{program}` once{instead}"
    );
    check(NAME, Level::Fail, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, configured: bool) -> ModelInfo {
        serde_json::from_value(serde_json::json!({
            "model": {"provider": provider, "model": id}, "configured": configured,
            "aliases": ["opus"],
        }))
        .unwrap()
    }

    fn auth(provider: &str, configured: bool, program: &str) -> AuthInfo {
        AuthInfo {
            provider: provider.into(),
            configured,
            detail: None,
            program: Some(program.into()),
        }
    }

    fn opus() -> ModelRef {
        ModelRef::new("claude-code", "claude-opus-5-5")
    }

    #[test]
    fn an_engine_without_a_login_fails_with_the_login_command() {
        let models = [
            model("claude-code", "claude-opus-5-5", false),
            model("codex", "gpt-6.1-sol", true),
        ];
        let logins = [
            auth("claude-code", false, "claude"),
            auth("codex", true, "codex"),
        ];
        let verdict = engine_verdict(&opus(), &models, Some(&logins));
        assert_eq!(verdict.level, Level::Fail);
        assert_eq!(
            verdict.detail,
            "claude-code/claude-opus-5-5: claude-code is not logged in: run `claude` once \
             (or choose a `codex` model)"
        );
        let codex = ModelRef::new("codex", "gpt-6.1-sol");
        let ready = engine_verdict(&codex, &models, Some(&logins));
        assert_eq!(ready.level, Level::Ok, "{}", ready.detail);
    }

    #[test]
    fn the_exact_model_must_be_listed_and_ready() {
        let models = [model("claude-code", "claude-opus-5-5", true)];
        assert_eq!(engine_verdict(&opus(), &models, None).level, Level::Ok);
        let alias = ModelRef::new("claude-code", "opus");
        assert_eq!(engine_verdict(&alias, &models, None).level, Level::Ok);
        let other = ModelRef::new("codex", "claude-opus-5-5");
        let missing = engine_verdict(&other, &models, None);
        assert_eq!(missing.level, Level::Fail);
        assert!(
            missing
                .detail
                .contains("lists no model codex/claude-opus-5-5")
        );
        let unready = [model("claude-code", "claude-opus-5-5", false)];
        let verdict = engine_verdict(&opus(), &unready, Some(&[]));
        assert_eq!(verdict.level, Level::Fail);
        assert!(
            verdict.detail.contains("no credentials"),
            "{}",
            verdict.detail
        );
    }

    #[test]
    fn a_login_aether_could_not_check_is_never_ready() {
        let models = [model("claude-code", "claude-opus-5-5", true)];
        let ready = engine_verdict(&opus(), &models, None);
        let unchecked = unchecked_login(ready, "timed out");
        assert_eq!(unchecked.level, Level::Warn);
        assert!(
            unchecked
                .detail
                .ends_with("is ready, but its login was not checked: timed out"),
            "{}",
            unchecked.detail
        );
        let missing = engine_verdict(&ModelRef::new("codex", "x"), &models, None);
        assert_eq!(unchecked_login(missing, "timed out").level, Level::Fail);
    }

    #[test]
    fn without_aether_only_a_missing_engine_cli_fails() {
        let codex_only = |program: &str| program == "codex";
        let verdict = offline_verdict(&opus(), codex_only);
        assert_eq!(verdict.level, Level::Fail);
        assert!(
            verdict.detail.contains("`claude` is not on PATH")
                && verdict.detail.ends_with("(or choose a `codex` model)"),
            "{}",
            verdict.detail
        );
        assert_eq!(offline_verdict(&opus(), |_| true).level, Level::Warn);
        let api = ModelRef::new("anthropic", "claude-opus-5-5");
        assert_eq!(offline_verdict(&api, |_| false).level, Level::Warn);
    }
}
