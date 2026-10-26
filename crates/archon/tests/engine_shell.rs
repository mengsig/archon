//! An archon CLI in an Aether agent's shell acts for that agent: archond
//! applies the agent's rules, never the operator's (ARCHITECTURE §11).
mod common;

use std::process::Output;

use common::{Env, start_daemon};
use serde_json::Value;

/// `archon args` as Aether runs it for an engine child, which names `agent`
/// when it is `Some`.
fn in_agent_shell(env: &Env, agent: Option<&str>, args: &[&str]) -> Output {
    let mut command = env.command(args);
    command.env("AETHER_ENGINE_RUN", "run_01ENGINE");
    if let Some(agent) = agent {
        command.env("AETHER_ENGINE_AGENT", agent);
    }
    command.output().expect("running archon")
}

fn refusal(out: &Output) -> String {
    assert!(!out.status.success(), "the request was made: {out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The operator's global settings overlay, which no refused change touches.
fn scoped_settings(env: &Env) -> Value {
    let view: Value = serde_json::from_slice(&env.run(&["--json", "settings"]).stdout).unwrap();
    view["scoped"].clone()
}

#[test]
fn a_cli_in_an_agents_shell_is_that_agent_not_the_operator() {
    let env = Env::new();
    let _daemon = start_daemon(&env);
    let agent = Some("agt_01STAGE");
    let set = ["settings", "--set", r#"{"max_parallel_agents":1}"#];

    let text = refusal(&in_agent_shell(&env, agent, &set));
    assert!(text.contains("reserved for the operator"), "{text}");
    assert!(
        text.contains("agt_01STAGE"),
        "the refusal names the agent: {text}"
    );
    let text = refusal(&in_agent_shell(&env, agent, &["pause"]));
    assert!(text.contains("only the orchestrator may"), "{text}");

    let status = in_agent_shell(&env, agent, &["status"]);
    assert!(status.status.success(), "reads still work: {status:?}");
    let queue = in_agent_shell(&env, agent, &["--json", "queue", "list"]);
    assert!(queue.status.success(), "reads still work: {queue:?}");
    assert_eq!(scoped_settings(&env), serde_json::json!({}));
    env.run(&set);
    assert_eq!(scoped_settings(&env)["max_parallel_agents"], 1);
}

#[test]
fn a_cli_in_an_unnamed_agents_shell_only_reads() {
    let env = Env::new();
    let _daemon = start_daemon(&env);
    let set = ["settings", "--set", r#"{"max_parallel_agents":1}"#];
    for agent in [None, Some("not-an-agent-id")] {
        let text = refusal(&in_agent_shell(&env, agent, &set));
        assert!(text.contains("not named"), "{agent:?}: {text}");
        let status = in_agent_shell(&env, agent, &["status"]);
        assert!(status.status.success(), "{agent:?}: {status:?}");
    }
    assert_eq!(scoped_settings(&env), serde_json::json!({}));
}
