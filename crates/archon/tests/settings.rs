//! Saved caps, settings diagnostics and reset confirmation through the CLI.
mod common;

use common::{Env, start_daemon};
use serde_json::Value;

#[test]
fn zero_is_unlimited_in_config_settings_and_queue_caps() {
    let env = Env::with_config("[aether]\nmode = 'shared'\n[limits]\nmax_parallel_agents = 0\n");
    let _daemon = start_daemon(&env);
    let initial = env.run(&["--json", "queue", "caps"]);
    let caps: Value = serde_json::from_slice(&initial.stdout).unwrap();
    assert_eq!(caps["max_parallel_agents"], 0);

    env.run(&["settings", "--set", r#"{"max_parallel_agents":2}"#]);
    let changed = env.run(&["queue", "caps", "--max-parallel-agents", "0"]);
    let text = String::from_utf8(changed.stdout).unwrap();
    assert!(text.contains("Saved across daemon restarts"), "{text}");
    assert!(text.contains("unlimited agents"), "{text}");

    env.run(&["queue", "caps", "--budgets", "off"]);
    let status = env.run(&["status"]);
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(text.contains("budgets: off"), "{text}");
    assert!(text.contains("saved across restarts"), "{text}");
    assert!(text.contains("archon queue caps --budgets on"), "{text}");

    let reset = env.run(&["settings", "--reset"]);
    let text = String::from_utf8(reset.stdout).unwrap();
    assert!(
        text.contains("Cleared all saved overrides for global"),
        "{text}"
    );
    assert!(text.contains("global settings: nothing set"), "{text}");

    let set = env.run(&[
        "--json",
        "settings",
        "--set",
        r#"{"max_parallel_agents":0}"#,
    ]);
    let view: Value = serde_json::from_slice(&set.stdout).unwrap();
    assert_eq!(view["scoped"]["max_parallel_agents"], 0);
    let reset = env.run(&["--json", "settings", "--reset"]);
    let view: Value = serde_json::from_slice(&reset.stdout).unwrap();
    assert_eq!(view["scoped"], serde_json::json!({}));
}

#[test]
fn caps_help_explains_persistence_reset_and_zero() {
    let env = Env::new();
    let out = env.run(&["queue", "caps", "--help"]);
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(help.contains("survive daemon restarts"), "{help}");
    assert!(help.contains("archon settings --reset"), "{help}");
    assert!(help.contains("0 means no limit"), "{help}");
}

#[test]
fn invalid_settings_report_the_key_and_expected_type_before_connecting() {
    let env = Env::new();
    let out = env
        .command(&["settings", "--set", r#"{"max_parallel_agents":"three"}"#])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8(out.stderr).unwrap();
    assert!(error.contains("max_parallel_agents"), "{error}");
    assert!(error.contains("expected u32"), "{error}");
    assert!(!env.path("archond.sock").exists());
}

#[test]
fn a_change_starts_a_stopped_archond_and_a_read_does_not() {
    let env = Env::new();
    let read = env.command(&["settings"]).output().unwrap();
    assert!(!read.status.success(), "{read:?}");
    assert!(!env.path("archond.sock").exists(), "a read started archond");

    let set = env
        .command(&[
            "--json",
            "settings",
            "--set",
            r#"{"max_parallel_agents":2}"#,
        ])
        .output()
        .unwrap();
    let after_set = env.command(&["daemon", "stop"]).output().unwrap();
    let reset = env.command(&["settings", "--reset"]).output().unwrap();
    // Stopped before any assertion, so a failed one leaves no daemon behind.
    let after_reset = env.command(&["daemon", "stop"]).output().unwrap();
    assert!(set.status.success(), "{set:?}");
    assert!(after_set.status.success(), "{after_set:?}");
    assert!(after_reset.status.success(), "{after_reset:?}");
    let stderr = String::from_utf8(set.stderr).unwrap();
    assert!(stderr.contains("starting it"), "{stderr}");
    let view: Value = serde_json::from_slice(&set.stdout).unwrap();
    assert_eq!(view["scoped"]["max_parallel_agents"], 2);
    assert!(reset.status.success(), "{reset:?}");
    let text = String::from_utf8(reset.stdout).unwrap();
    assert!(text.contains("Cleared all saved overrides"), "{text}");
}
