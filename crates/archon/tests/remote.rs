//! `archon remote` against a private archond: the listener survives a
//! restart, and the setup wizard drives a stand-in `tailscale` and `curl`.
mod common;

use std::path::Path;
use std::process::Output;

use common::{Env, start_daemon, stop_daemon};
use serde_json::Value;

const CONFIG: &str = "[aether]\nmode = \"shared\"\n[remote]\naddress = \"127.0.0.1:0\"\n";

/// A `tailscale` signed in as `localhost`, whose serve config is a file that
/// `serve --bg` writes, and a `curl` that finds the app healthy. Every call
/// is appended to `calls`.
fn stand_ins(dir: &Path, backend_state: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let status = serde_json::json!({
        "BackendState": backend_state,
        "Self": {"DNSName": "localhost."},
        "CurrentTailnet": {"MagicDNSEnabled": true},
        "CertDomains": ["localhost"],
    });
    std::fs::write(dir.join("status.json"), status.to_string()).unwrap();
    let tailscale = r#"#!/bin/sh
dir=$(dirname "$0")
echo "tailscale $*" >> "$dir/calls"
case "$*" in
version) echo 1.90.0 ;;
"status --json") cat "$dir/status.json" ;;
"serve status --json") cat "$dir/serve.json" 2>/dev/null || echo '{}' ;;
"serve --bg --https="*)
    port=${3#--https=}
    printf '{"Web":{"localhost:%s":{"Handlers":{"/":{"Proxy":"%s"}}}}}' "$port" "$4" > "$dir/serve.json"
    ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;
    archon_core::testing::write_script(&dir.join("tailscale"), tailscale);
    let curl =
        "#!/bin/sh\necho \"curl $*\" >> \"$(dirname \"$0\")/calls\"\necho '{\"status\":\"ok\"}'\n";
    archon_core::testing::write_script(&dir.join("curl"), curl);
}

fn wizard(env: &Env, tools: &Path, args: &[&str]) -> Output {
    let path = std::env::join_paths(std::iter::once(tools.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    env.command(args)
        .env("PATH", path)
        .env("ARCHON_TAILSCALE", tools.join("tailscale"))
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `archon --json remote …`, which must succeed.
fn remote_json(env: &Env, args: &[&str]) -> Value {
    let args: Vec<&str> = ["--json", "remote"].iter().chain(args).copied().collect();
    json(&env.command(&args).output().unwrap())
}

#[test]
fn the_listener_is_off_until_enabled_and_comes_back_after_a_restart() {
    let env = Env::with_config(CONFIG);
    let daemon = start_daemon(&env);
    let off = env.run(&["remote"]);
    assert!(String::from_utf8_lossy(&off.stdout).contains("Remote access is off"));
    let set_up = remote_json(&env, &["setup", "--local"]);
    assert_eq!(set_up["remote"]["enabled"], true, "{set_up}");
    assert!(
        set_up["remote"]["listening"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:")
    );
    let code = set_up["pairing"]["code"].as_str().unwrap();
    assert_eq!(code.len(), 9, "{code}");
    assert!(
        set_up["pairing"]["urls"][0]
            .as_str()
            .unwrap()
            .ends_with("/pair")
    );
    let mode = std::fs::metadata(env.path("data/remote.json")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );
    stop_daemon(&env, daemon);
    let daemon = start_daemon(&env);
    let status = remote_json(&env, &["status"]);
    assert!(
        status["listening"].is_string(),
        "re-opened after a restart: {status}"
    );
    assert!(
        status["pairing_expires_at"].is_null(),
        "a pairing never outlives archond"
    );
    let disabled = remote_json(&env, &["disable"]);
    assert_eq!(disabled["enabled"], false);
    assert!(disabled["listening"].is_null());
    let refused = env.command(&["remote", "pair"]).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("remote access is off"));
    let revoke = env
        .command(&["remote", "revoke", "nobody"])
        .output()
        .unwrap();
    assert!(!revoke.status.success(), "an unknown device is an error");
    stop_daemon(&env, daemon);
}

#[test]
fn the_wizard_publishes_on_the_tailnet_once_and_reuses_it_on_a_rerun() {
    let env = Env::with_config(CONFIG);
    let daemon = start_daemon(&env);
    let tools = env.path("tools");
    stand_ins(&tools, "Running");
    let first = json(&wizard(
        &env,
        &tools,
        &["--json", "remote", "setup", "--yes"],
    ));
    assert_eq!(first["origin"], "https://localhost", "{first}");
    assert_eq!(first["remote"]["public_origins"][0], "https://localhost");
    assert_eq!(first["pairing"]["urls"][0], "https://localhost/pair");
    let port = first["remote"]["listening"]
        .as_str()
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .1
        .to_owned();
    let calls = std::fs::read_to_string(tools.join("calls")).unwrap();
    let publish = format!("tailscale serve --bg --https=443 http://127.0.0.1:{port}");
    assert!(calls.contains(&publish), "{calls}");
    assert!(
        calls.contains("curl -fsS --max-time 45 https://localhost/api/health"),
        "{calls}"
    );
    std::fs::remove_file(tools.join("calls")).unwrap();
    let again = wizard(&env, &tools, &["remote", "setup", "--no-pair"]);
    let text = String::from_utf8_lossy(&again.stdout).into_owned();
    assert!(again.status.success(), "{again:?}");
    assert!(
        text.contains("Already published at https://localhost."),
        "{text}"
    );
    let calls = std::fs::read_to_string(tools.join("calls")).unwrap();
    assert!(
        !calls.contains("serve --bg"),
        "nothing is published twice: {calls}"
    );
    let proxied = json(&wizard(
        &env,
        &tools,
        &[
            "--json",
            "remote",
            "setup",
            "--proxy-url",
            "https://proxy.example",
        ],
    ));
    assert_eq!(
        proxied["pairing"]["urls"][0], "https://proxy.example/pair",
        "the QR code opens the address this run checked: {proxied}"
    );
    let local = remote_json(&env, &["setup", "--local", "--no-pair"]);
    assert!(local["origin"].is_null(), "{local}");
    assert_eq!(
        local["remote"]["public_origins"],
        serde_json::json!([]),
        "--local withdraws the tailnet origin: {local}"
    );
    stop_daemon(&env, daemon);
}

#[test]
fn the_wizard_stops_with_the_fix_when_tailscale_is_not_ready_or_not_asked() {
    let env = Env::with_config(CONFIG);
    let daemon = start_daemon(&env);
    let tools = env.path("tools");
    stand_ins(&tools, "Stopped");
    let stopped = wizard(&env, &tools, &["remote", "setup", "--yes"]);
    assert!(!stopped.status.success());
    let errors = String::from_utf8_lossy(&stopped.stderr).into_owned();
    assert!(errors.contains("tailscale up"), "{errors}");
    assert!(
        errors.contains("switched remote access back off"),
        "{errors}"
    );
    let status = remote_json(&env, &["status"]);
    assert_eq!(
        status["enabled"], false,
        "a failed publish opens nothing: {status}"
    );
    assert!(status["listening"].is_null(), "{status}");
    stand_ins(&tools, "Running");
    // No terminal and no --yes: nothing is published without consent.
    let unasked = wizard(&env, &tools, &["remote", "setup"]);
    assert!(!unasked.status.success());
    let errors = String::from_utf8_lossy(&unasked.stderr).into_owned();
    assert!(errors.contains("pass --yes"), "{errors}");
    let calls = std::fs::read_to_string(tools.join("calls")).unwrap();
    assert!(!calls.contains("serve --bg"), "{calls}");
    // Published through a proxy that never answers: switched back off too.
    archon_core::testing::write_script(&tools.join("curl"), "#!/bin/sh\nexit 7\n");
    let unreachable = wizard(
        &env,
        &tools,
        &[
            "remote",
            "setup",
            "--proxy-url",
            "https://proxy.example",
            "--no-pair",
        ],
    );
    assert!(!unreachable.status.success(), "{unreachable:?}");
    let status = remote_json(&env, &["status"]);
    assert_eq!(status["enabled"], false, "{status}");
    stop_daemon(&env, daemon);
}

#[test]
fn a_listener_that_cannot_open_leaves_remote_access_off() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let env = Env::with_config(&format!(
        "[aether]\nmode = \"shared\"\n[remote]\naddress = \"127.0.0.1:{port}\"\n"
    ));
    let daemon = start_daemon(&env);
    let failed = env
        .command(&["remote", "setup", "--local", "--no-pair"])
        .output()
        .unwrap();
    assert!(!failed.status.success(), "{failed:?}");
    let errors = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(errors.contains("the listener did not open"), "{errors}");
    assert!(
        errors.contains("switched remote access back off"),
        "{errors}"
    );
    drop(held);
    stop_daemon(&env, daemon);
    let daemon = start_daemon(&env);
    let status = remote_json(&env, &["status"]);
    assert_eq!(
        status["enabled"], false,
        "a restart opens nothing a failed setup left: {status}"
    );
    assert!(status["listening"].is_null(), "{status}");
    stop_daemon(&env, daemon);
}
