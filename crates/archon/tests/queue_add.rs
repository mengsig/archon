//! Scripted enqueue exercises the real daemon's shared validation path.
mod common;

use std::io::Write as _;
use std::process::{Output, Stdio};

use common::{Env, git_repo, start_daemon, stop_daemon};
use serde_json::{Value, json};

fn output(env: &Env, extra: &[&str]) -> Output {
    let brief = env.path("brief.md");
    let plan = env.path("plan.json");
    let args = [
        &[
            "--json",
            "queue",
            "add",
            "--title",
            "scripted",
            "--pipeline",
            "ask",
            "--brief-file",
            brief.to_str().unwrap(),
            "--model-plan-file",
            plan.to_str().unwrap(),
        ][..],
        extra,
    ]
    .concat();
    env.command(&args).output().unwrap()
}

fn parsed(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

fn plan(env: &Env, value: Value) {
    std::fs::write(env.path("plan.json"), serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn enqueue_files_policy_idempotency_and_dependencies() {
    let env = Env::with_config(
        "[aether]\nmode = 'shared'\n[orchestrator]\nmodel = 'codex/test'\n[models]\nallow = ['codex/test']\n",
    );
    let daemon = start_daemon(&env);
    env.run(&["pause"]);
    std::fs::write(env.path("brief.md"), "Read the cache design.").unwrap();
    plan(
        &env,
        json!({"answer": [{"model": "codex/test", "effort": "high"}]}),
    );
    let reasons = env.path("reasons.toml");
    std::fs::write(&reasons, "answer = 'Inspect the implementation'\n").unwrap();
    let extra = [
        "--idempotency-key",
        "script-1",
        "--priority",
        "high",
        "--tag",
        "cache",
        "--tag",
        "script",
        "--budget-usd",
        "2.5",
        "--review",
        "none",
        "--proposal",
        "off",
        "--report",
        "off",
        "--merge",
        "manual",
        "--tools",
        "guarded",
        "--model-reasons-file",
        reasons.to_str().unwrap(),
    ];
    let first = output(&env, &extra);
    assert!(first.status.success(), "{first:?}");
    let item = parsed(&first);
    assert_eq!(item["brief"], "Read the cache design.");
    assert_eq!(item["priority"], "high");
    assert_eq!(item["tags"], json!(["cache", "script"]));
    assert_eq!(item["budget_usd"], 2.5);
    assert_eq!(item["autonomy"]["review"], "none");
    assert_eq!(
        item["model_reasons"]["answer"],
        "Inspect the implementation"
    );
    assert_eq!(parsed(&output(&env, &extra))["id"], item["id"]);
    let id = item["id"].as_str().unwrap();
    let dependent = output(&env, &["--depends-on", id]);
    assert!(dependent.status.success(), "{dependent:?}");
    assert_eq!(parsed(&dependent)["dependencies"], json!([id]));

    for (bad, code) in [
        (
            json!({"wrong": [{"model": "codex/test", "effort": "high"}]}),
            "invalid_argument",
        ),
        (json!({"answer": []}), "invalid_argument"),
        (
            json!({"answer": [{"model": "codex/test", "effort": "high", "typo": true}]}),
            "invalid_argument",
        ),
        (
            json!({"answer": [{"model": "codex/denied", "effort": "high"}]}),
            "policy",
        ),
    ] {
        plan(&env, bad);
        let out = output(&env, &[]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert_eq!(parsed(&out)["error"]["code"], code, "{out:?}");
    }
    plan(
        &env,
        json!({"answer": [{"model": "codex/test", "effort": "high"}]}),
    );
    let out = output(&env, &["--merge", "auto"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parsed(&out)["error"]["code"], "invalid_argument");
    for extra in [
        &["--budget-usd=-1"][..],
        &["--budget-usd=0"],
        &["--tag", ""],
        &["--tag", "   "],
        &["--budget-usd=NaN"],
        &["--budget-usd=inf"],
        &["--idempotency-key", "script-1"],
    ] {
        let out = output(&env, extra);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert_eq!(parsed(&out)["error"]["code"], "invalid_argument", "{out:?}");
    }
    std::fs::write(env.path("plan.json"), "{broken").unwrap();
    let out = output(&env, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(parsed(&out)["error"]["code"], "invalid_argument");
    stop_daemon(&env, daemon);
}

/// A plan names the stages whose model the caller chooses; the rest, or all
/// of them without a plan file, get the orchestrator's model.
#[test]
fn a_stage_without_a_model_choice_gets_the_orchestrators_model() {
    let env = Env::with_config(
        "[aether]\nmode = 'shared'\n[orchestrator]\nmodel = 'codex/test'\n[models]\nallow = ['codex/test']\n",
    );
    let daemon = start_daemon(&env);
    env.run(&["pause"]);
    std::fs::write(env.path("brief.md"), "Read the cache design.").unwrap();
    let orchestrators = json!({"provider": "codex", "model": "test"});
    let defaulted = json!([{"model": orchestrators, "effort": "medium"}]);

    plan(&env, json!({}));
    let empty = output(&env, &[]);
    assert!(empty.status.success(), "{empty:?}");
    assert_eq!(parsed(&empty)["model_plan"]["answer"], defaulted);

    let brief = env.path("brief.md");
    let brief = brief.to_str().unwrap();
    let args = ["--json", "queue", "add", "--title", "unplanned"];
    let args = [&args[..], &["--pipeline", "ask", "--brief-file", brief]].concat();
    let unplanned = env.command(&args).output().unwrap();
    assert!(unplanned.status.success(), "{unplanned:?}");
    assert_eq!(parsed(&unplanned)["model_plan"]["answer"], defaulted);
    stop_daemon(&env, daemon);
}

#[test]
fn stdin_brief_toml_plan_repo_defaults_and_text_id() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    env.run(&["pause"]);
    let repo = env.path("demo");
    git_repo(&repo);
    env.run(&["repo", "add", repo.to_str().unwrap(), "--pipeline", "ask"]);
    let plan = env.path("plan.toml");
    std::fs::write(&plan, "[[answer]]\nmodel = 'codex/test'\neffort = 'high'\n").unwrap();
    let mut child = env
        .command(&[
            "queue",
            "add",
            "--title",
            "stdin",
            "--repo",
            "demo",
            "--brief-file",
            "-",
            "--model-plan-file",
            plan.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Brief from stdin\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let id = text.trim();
    assert!(archon_core::id::QueueItemId::parse(id).is_ok(), "{text}");
    let out = env.run(&["queue", "show", id, "--json"]);
    let detail = parsed(&out);
    assert_eq!(detail["item"]["brief"], "Brief from stdin\n");
    assert_eq!(detail["item"]["pipeline"], "ask");
    assert!(detail["item"]["repo"].is_string());
    let out = env
        .command(&["queue", "wait", id, "--timeout", "1s", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert_eq!(parsed(&out)["items"][0]["id"], id);
    env.run(&["queue", "cancel", id]);
    let out = env
        .command(&["queue", "wait", id, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(parsed(&out)["items"][0]["state"]["state"], "cancelled");
    stop_daemon(&env, daemon);
}

#[test]
fn invalid_titles_and_empty_briefs_fail_before_connecting() {
    let env = Env::new();
    plan(
        &env,
        json!({"answer": [{"model": "codex/test", "effort": "high"}]}),
    );
    std::fs::write(env.path("brief.md"), "valid brief").unwrap();
    // Use a complete command to avoid duplicate title flags.
    let out = env
        .command(&[
            "queue",
            "add",
            "--title",
            "two\nlines",
            "--pipeline",
            "ask",
            "--brief-file",
            env.path("brief.md").to_str().unwrap(),
            "--model-plan-file",
            env.path("plan.json").to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(parsed(&out)["error"]["code"], "invalid_argument");
    for brief in ["", " \n\t"] {
        std::fs::write(env.path("brief.md"), brief).unwrap();
        let out = output(&env, &[]);
        assert_eq!(parsed(&out)["error"]["code"], "invalid_argument");
    }
    assert!(!env.path("data").exists());
}
