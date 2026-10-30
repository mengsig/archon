//! `archon doctor` exits 0 when nothing failed, warnings included, and 1
//! as soon as one check fails; its `--json` report says which.

mod common;

use std::process::Output;

use common::Env;

/// Dedicated mode with lavish off, so after `archon setup` only warnings
/// remain: Archon's aetherd and archond are not running.
const DEDICATED: &str = "[aether]\nmode = \"dedicated\"\n\n[lavish]\nenabled = \"off\"\n";

fn doctor(env: &Env) -> (Output, Vec<serde_json::Value>) {
    let out = env.command(&["--json", "doctor"]).output().unwrap();
    let checks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("doctor --json printed no JSON list ({e}): {out:?}"));
    assert!(!checks.is_empty(), "doctor always runs checks");
    (out, checks)
}

fn level_of<'a>(checks: &'a [serde_json::Value], name: &str) -> &'a str {
    let check = checks.iter().find(|c| c["check"] == name);
    let check = check.unwrap_or_else(|| panic!("no {name} check in {checks:?}"));
    check["level"].as_str().unwrap()
}

fn set_up(config: &str) -> Env {
    let env = Env::with_config(config);
    std::fs::create_dir_all(env.path("aether")).unwrap();
    std::fs::write(env.path("aether/config.toml"), "[network]\n").unwrap();
    env.run(&["setup"]);
    env
}

#[test]
fn warnings_alone_exit_zero() {
    let env = set_up(DEDICATED);
    let (out, checks) = doctor(&env);
    assert_eq!(out.status.code(), Some(0), "{checks:#?}");
    assert!(checks.iter().all(|c| c["level"] != "fail"), "{checks:#?}");
    assert_eq!(level_of(&checks, "archond"), "warn");
    assert_eq!(level_of(&checks, "mcp registered"), "ok");
}

#[test]
fn a_failed_check_exits_one() {
    let env = set_up(DEDICATED);
    std::fs::remove_file(env.path("config/aether/config.toml")).unwrap();
    let (out, checks) = doctor(&env);
    assert_eq!(out.status.code(), Some(1), "{checks:#?}");
    assert_eq!(level_of(&checks, "mcp registered"), "fail");
    assert_eq!(level_of(&checks, "config"), "ok", "only one check failed");
}

#[test]
fn a_broken_config_fails_but_the_other_checks_still_run() {
    let env = Env::with_config("[aether]\nmode = \"sideways\"\n");
    let (out, checks) = doctor(&env);
    assert_eq!(out.status.code(), Some(1), "{checks:#?}");
    assert_eq!(level_of(&checks, "config"), "fail");
    assert_eq!(level_of(&checks, "git"), "ok");
}

#[test]
fn doctor_validates_operator_edited_calm_definitions() {
    let env = set_up(DEDICATED);
    let path = env.path("config/aether/modes/calm.toml");
    for text in [
        "version = 99\nshow = []",
        "version = 1\nshow = ['tool', 'tool']",
        "version = 1\nshow = ['unknown']",
        "version = 1\nshow = []\n[scope]",
    ] {
        std::fs::write(&path, text).unwrap();
        let (out, checks) = doctor(&env);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(level_of(&checks, "calm mode"), "fail");
    }
    std::fs::write(&path, "version = 1\nshow = ['tool']\nlabel = 'My calm'").unwrap();
    let (_, checks) = doctor(&env);
    assert_eq!(level_of(&checks, "calm mode"), "ok");
}

#[test]
fn doctor_shows_the_configured_host_context_while_aether_is_offline() {
    let env = set_up(DEDICATED);
    let (_, full) = doctor(&env);
    assert_eq!(level_of(&full, "host context"), "ok");
    let lean = DEDICATED.replace(
        "mode = \"dedicated\"",
        "mode = \"dedicated\"\nhost_context = \"lean\"",
    );
    let env = set_up(&lean);
    let (out, checks) = doctor(&env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(level_of(&checks, "host context"), "warn");
    let context = checks
        .iter()
        .find(|check| check["check"] == "host context")
        .unwrap();
    assert!(
        context["detail"]
            .as_str()
            .unwrap()
            .contains("configured lean")
    );
}

/// Doctor against a fake shared Aether that lists the default orchestrator
/// model and reports claude-code's login as `logged_in`.
async fn doctor_with_login(logged_in: bool) -> (Output, Vec<serde_json::Value>) {
    use archon_aether::fake::{FakeAether, Scripted};
    let env = Env::new();
    let socket = env.path("aether.sock");
    let aether = FakeAether::start(&socket).await.unwrap();
    let models = serde_json::json!([
        {"model": {"provider": "claude-code", "model": "claude-opus-5-5"},
         "configured": logged_in, "aliases": ["opus"]},
        {"model": {"provider": "codex", "model": "gpt-6.1-sol"}, "configured": true},
    ]);
    aether.set_default("list_models", Scripted::reply("models", models));
    let logins = serde_json::json!([
        {"provider": "claude-code", "configured": logged_in, "program": "claude",
         "detail": "claude: not logged in"},
        {"provider": "codex", "configured": true, "program": "codex"},
    ]);
    aether.set_default("auth_status", Scripted::reply("auth", logins));
    let mut command = env.command(&["--json", "doctor"]);
    command.env("AETHER_SOCKET", &socket);
    let out = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert_eq!(aether.received_named("auth_status").len(), 1, "asked once");
    let checks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("doctor --json printed no JSON list ({e}): {out:?}"));
    (out, checks)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_is_not_logged_in_fails_with_its_login_command() {
    let (out, checks) = doctor_with_login(false).await;
    assert_eq!(out.status.code(), Some(1), "{checks:#?}");
    assert_eq!(level_of(&checks, "engine"), "fail");
    let engine = checks.iter().find(|c| c["check"] == "engine").unwrap();
    let detail = engine["detail"].as_str().unwrap();
    assert!(
        detail.contains(
            "claude-code is not logged in: run `claude` once (or choose a `codex` model)"
        ),
        "{detail}"
    );

    let (_, checks) = doctor_with_login(true).await;
    assert_eq!(level_of(&checks, "engine"), "ok", "{checks:#?}");
}
