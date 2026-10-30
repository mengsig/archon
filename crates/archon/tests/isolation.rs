//! One archond per data directory, reads that never start one, and no autostart where the
//! config dir, data dir and socket mix moved and default locations.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Env, start_daemon, stop_daemon};

const NOT_RUNNING: &str = "archond is not running (start it with `archon daemon start`)";

/// The output of `child`, which must exit within `limit`; killed otherwise.
fn exit_within(mut child: Child, limit: Duration) -> Output {
    let deadline = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _already_gone = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!("still running after {limit:?}: {out:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_second_archond_on_the_same_data_dir_is_refused_by_name() {
    let env = Env::new();
    let first = start_daemon(&env);
    let second = env
        .command(&["daemon", "run"])
        .env("ARCHON_SOCKET", env.path("second.sock"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = exit_within(second, common::START_WAIT);
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "the second daemon ran: {out:?}");
    let holder = format!(
        "another archond (pid {}, socket {})",
        first.0.id(),
        env.path("archond.sock").display()
    );
    assert!(stderr.contains(&holder), "{stderr}");
    assert!(
        stderr.contains(&env.path("data").display().to_string()),
        "{stderr}"
    );
    assert!(!env.path("second.sock").exists(), "it never listened");
    env.run(&["daemon", "status"]);
    stop_daemon(&env, first);
}

#[test]
fn reads_with_no_daemon_start_nothing() {
    let env = Env::new();
    let reads: [&[&str]; 14] = [
        &["queue", "list"],
        &["queue", "show", "qi_x"],
        &["queue", "caps"],
        &["status"],
        &["--json", "status"],
        &["questions"],
        &["log"],
        &["cost"],
        &["reports"],
        &["repo", "list"],
        &["autonomy"],
        &["initiatives"],
        &["orchestrator", "status"],
        &["github-watcher"],
    ];
    for args in reads {
        let out = env.command(args).output().unwrap();
        // Under --json the error is printed on stdout.
        let said = text(&out.stderr) + &text(&out.stdout);
        assert!(!out.status.success(), "archon {args:?} succeeded: {out:?}");
        assert!(said.contains(NOT_RUNNING), "archon {args:?}: {said}");
        assert!(!said.contains("starting it"), "archon {args:?}: {said}");
    }
    let lessons = env.run(&["lessons"]);
    assert!(text(&lessons.stderr).is_empty(), "{lessons:?}");
    assert_nothing_started(&env.path("archond.sock"), &env.path("data"));
}

fn assert_nothing_started(socket: &Path, data: &Path) {
    assert!(!socket.exists(), "a socket at {}", socket.display());
    assert!(
        !socket.with_extension("log").exists(),
        "a detached start's log beside {}",
        socket.display()
    );
    assert!(
        !data.join("archon.db").exists(),
        "a store in {}",
        data.display()
    );
}

/// `env`'s commands with the defaults moved into its temp dir.
fn sandboxed(env: &Env, args: &[&str]) -> Command {
    let mut command = env.command(args);
    command
        .env("HOME", env.path("home"))
        .env("XDG_CONFIG_HOME", env.path("home/.config"))
        .env("XDG_DATA_HOME", env.path("home/.local/share"))
        .env("XDG_RUNTIME_DIR", env.path("run"));
    command
}

/// Only `ARCHON_CONFIG_DIR` and `ARCHON_SOCKET` set: the data dir is the default.
fn mixed(env: &Env, args: &[&str]) -> Command {
    let mut command = sandboxed(env, args);
    command.env_remove("ARCHON_DATA_DIR");
    command
}

/// No `ARCHON_*` variable set, so every location is the sandbox's default.
fn at_defaults(env: &Env, args: &[&str]) -> Command {
    let mut command = sandboxed(env, args);
    for variable in ["ARCHON_CONFIG_DIR", "ARCHON_DATA_DIR", "ARCHON_SOCKET"] {
        command.env_remove(variable);
    }
    command
}

/// The sandbox's default config dir, holding `env`'s shared-mode config.
fn default_config(env: &Env) -> PathBuf {
    let dir = env.path("home/.config/archon");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(env.path("config/config.toml"), dir.join("config.toml")).unwrap();
    dir
}

#[test]
fn a_mixed_environment_warns_and_starts_nothing() {
    let env = Env::new();
    let default_data = env.path("home/.local/share/archon");
    for args in [&["say", "hi"][..], &["daemon", "start"], &["repo", "list"]] {
        let out = mixed(&env, args).output().unwrap();
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "archon {args:?} succeeded: {out:?}");
        let kept = format!(
            "ARCHON_DATA_DIR ({}) is still the default",
            default_data.display()
        );
        assert!(stderr.contains(&kept), "archon {args:?}: {stderr}");
        assert!(
            stderr.contains("ARCHON_SOCKET ("),
            "archon {args:?}: {stderr}"
        );
    }
    let refused = mixed(&env, &["say", "hi"]).output().unwrap();
    assert!(
        text(&refused.stderr).contains("it is not started for you"),
        "{refused:?}"
    );
    let read = mixed(&env, &["queue", "list"]).output().unwrap();
    assert!(
        text(&read.stderr).contains(&format!("{NOT_RUNNING}. Note: ")),
        "{read:?}"
    );

    let doctor = mixed(&env, &["--json", "doctor"]).output().unwrap();
    let checks: Vec<serde_json::Value> = serde_json::from_slice(&doctor.stdout).unwrap();
    let locations = checks.iter().find(|c| c["check"] == "locations").unwrap();
    assert_eq!(locations["level"], "warn", "{checks:#?}");
    assert_nothing_started(&env.path("archond.sock"), &default_data);
    assert!(
        !env.path("run/archon").exists(),
        "nothing at the default socket"
    );
}

/// Only `ARCHON_CONFIG_DIR` set: the data dir and the socket are the sandbox's
/// defaults, where the operator's own archond and aetherd would listen.
fn mixed_at_default_socket(env: &Env, args: &[&str]) -> Command {
    let mut command = mixed(env, args);
    command.env_remove("ARCHON_SOCKET");
    command
}

/// A socket at `path` that no command may connect to.
fn untouched_listener(path: &Path) -> std::os::unix::net::UnixListener {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

#[test]
fn a_mixed_environment_stops_and_restarts_nothing() {
    let env = Env::new();
    let archond = untouched_listener(&env.path("run/archon/archond.sock"));
    let aetherd = untouched_listener(&env.path("run/archon/aetherd.sock"));
    let tools = env.path("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let ran = env.path("aether-ran");
    let script = format!("#!/bin/sh\n: > '{}'\n", ran.display());
    archon_core::testing::write_script(&tools.join("aether"), &script);
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    for args in [
        &["daemon", "stop"][..],
        &["daemon", "stop", "--force"],
        &["daemon", "restart"],
        &["aether", "stop"],
    ] {
        let child = mixed_at_default_socket(&env, args)
            .env("PATH", &path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = exit_within(child, Duration::from_secs(10));
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "archon {args:?} succeeded: {out:?}");
        assert!(
            stderr.contains("stops and restarts nothing here")
                && stderr.contains("ARCHON_DATA_DIR (")
                && stderr.contains("are still the default"),
            "archon {args:?}: {stderr}"
        );
    }
    for (name, listener) in [("archond", &archond), ("aetherd", &aetherd)] {
        let accepted = listener.accept().map(drop).unwrap_err();
        assert_eq!(
            accepted.kind(),
            std::io::ErrorKind::WouldBlock,
            "{name} was reached"
        );
    }
    assert!(!ran.exists(), "`aether` was run");
}

/// `archon mcp`, run as `command`, after one `list_repos` call and the end of its input.
fn mcp_list_repos(mut command: Command) -> Output {
    let mut proxy = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_repos","arguments":{}}}"#;
    let mut stdin = proxy.stdin.take().unwrap();
    std::io::Write::write_all(&mut stdin, format!("{call}\n").as_bytes()).unwrap();
    drop(stdin);
    exit_within(proxy, common::START_WAIT)
}

/// The tool error with which `archon mcp` answered a read: archond is not running.
fn refused_read(out: &Output) -> String {
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    let said = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(said.contains("archond is not running"), "{said}");
    said.to_owned()
}

/// Aether spawns `archon mcp` with its own environment, which may set only
/// `ARCHON_SOCKET`: the proxy then connects but never starts archond.
#[test]
fn the_mcp_proxy_does_not_start_archond_in_a_mixed_environment() {
    let env = Env::new();
    let said = refused_read(&mcp_list_repos(mixed(&env, &["mcp"])));
    assert!(said.contains("it is not started for you"), "{said}");
    assert!(said.contains("ARCHON_DATA_DIR ("), "{said}");
    assert_nothing_started(
        &env.path("archond.sock"),
        &env.path("home/.local/share/archon"),
    );
}

/// A read-only tool call never starts archond, wherever the locations are.
#[test]
fn the_mcp_proxy_starts_nothing_for_a_read() {
    let env = Env::new();
    let moved = mcp_list_repos(env.command(&["mcp"]));
    // Stopped before any assertion, so a failed one leaves no daemon behind.
    let _stopped_if_started = env.command(&["daemon", "stop"]).output().unwrap();
    let moved = refused_read(&moved);
    assert!(
        moved.contains("a read-only call never starts it"),
        "{moved}"
    );
    assert_nothing_started(&env.path("archond.sock"), &env.path("data"));

    default_config(&env);
    let default = mcp_list_repos(at_defaults(&env, &["mcp"]));
    let _stopped_if_started = at_defaults(&env, &["daemon", "stop"]).output().unwrap();
    let default = refused_read(&default);
    assert!(
        default.contains("a read-only call never starts it"),
        "{default}"
    );
    assert_nothing_started(
        &env.path("run/archon/archond.sock"),
        &env.path("home/.local/share/archon"),
    );
}

/// A symlink or `..` leading to the default data dir keeps it the default,
/// so a private config and socket over it are still refused a start.
#[test]
fn another_way_to_the_default_data_dir_still_starts_nothing() {
    let env = Env::new();
    let default_data = env.path("home/.local/share/archon");
    std::fs::create_dir_all(&default_data).unwrap();
    std::os::unix::fs::symlink(&default_data, env.path("data-link")).unwrap();
    for data in [env.path("data-link"), default_data.join("../archon")] {
        let out = mixed(&env, &["pause"])
            .env("ARCHON_DATA_DIR", &data)
            .output()
            .unwrap();
        // Stopped before any assertion, so a failed one leaves no daemon behind.
        let _stopped_if_started = mixed(&env, &["daemon", "stop"])
            .env("ARCHON_DATA_DIR", &data)
            .output()
            .unwrap();
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{} started: {out:?}", data.display());
        assert!(stderr.contains("it is not started for you"), "{stderr}");
        assert!(stderr.contains("is still the default"), "{stderr}");
    }
    assert_nothing_started(&env.path("archond.sock"), &default_data);
}

/// A config dir symlinked to the default one is the default, so with the
/// default data dir and socket `archon daemon start` is no mix.
#[test]
fn a_symlink_to_the_default_config_dir_still_starts_archond() {
    let env = Env::new();
    let link = env.path("config-link");
    std::os::unix::fs::symlink(default_config(&env), &link).unwrap();
    let started = at_defaults(&env, &["daemon", "start"])
        .env("ARCHON_CONFIG_DIR", &link)
        .output()
        .unwrap();
    // Stopped before any assertion, so a failed one leaves no daemon behind.
    let stopped = at_defaults(&env, &["daemon", "stop"])
        .env("ARCHON_CONFIG_DIR", &link)
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    assert!(stopped.status.success(), "{stopped:?}");
    let stderr = text(&started.stderr);
    assert!(stderr.contains("starting it"), "{stderr}");
    assert!(
        env.path("home/.local/share/archon/archon.db").exists(),
        "archond used the sandbox's default store"
    );
}

#[test]
fn a_write_with_every_location_moved_starts_archond_and_says_so() {
    let env = Env::new();
    let paused = env.command(&["pause"]).output().unwrap();
    // Stopped before any assertion, so a failed one leaves no daemon behind.
    let stopped = env.command(&["daemon", "stop"]).output().unwrap();
    assert!(paused.status.success(), "{paused:?}");
    assert!(stopped.status.success(), "{stopped:?}");
    let stderr = text(&paused.stderr);
    assert!(
        stderr.contains("archond is not running; starting it"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&env.path("data").display().to_string()),
        "{stderr}"
    );
}
