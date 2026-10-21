//! The native TUI launcher over the CLI and wire boundary.

mod common;
#[path = "common/tui.rs"]
mod fake;

use archon_core::protocol::{ErrorCode, ErrorReply, Reply, Request};
use common::Env;

fn environment() -> Env {
    environment_with_config("[aether]\nmode = \"shared\"\n")
}

fn environment_with_config(config: &str) -> Env {
    let env = Env::with_config(config);
    std::fs::create_dir(env.path("bin")).unwrap();
    archon_core::testing::write_script(
        &env.path("bin/aether"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf 'NO_COLOR=%s\\nAETHER_THEME=%s\\nAETHER_ASCII=%s\\nAETHER_REDUCED_MOTION=%s\\n' \"$NO_COLOR\" \"$AETHER_THEME\" \"$AETHER_ASCII\" \"$AETHER_REDUCED_MOTION\"\n",
    );
    env
}

fn record_endpoint(env: &Env) {
    archon_core::testing::write_script(
        &env.path("bin/aether"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf 'AETHER_SOCKET=%s\\n' \"$AETHER_SOCKET\"\n",
    );
}

#[test]
fn configured_shared_socket_wins_over_inherited_or_unset_socket() {
    for inherited in [Some("/unrelated/operator-aether.sock"), None] {
        let env = environment_with_config(
            "[aether]\nmode = \"shared\"\nsocket = \"configured-aether.sock\"\n",
        );
        record_endpoint(&env);
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let mut command = env.command(&["tui"]);
        command.env("PATH", env.path("bin"));
        if let Some(socket) = inherited {
            command.env("AETHER_SOCKET", socket);
        } else {
            command.env_remove("AETHER_SOCKET");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "tui\n--agent\nagt_selected\nAETHER_SOCKET={}\n",
                env.path("config/configured-aether.sock").display()
            )
        );
        assert!(output.stderr.is_empty());
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[test]
fn shared_tui_without_a_configured_socket_preserves_inherited_socket() {
    let env = environment();
    record_endpoint(&env);
    let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
    let output = env
        .command(&["tui"])
        .env("PATH", env.path("bin"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "tui\n--agent\nagt_selected\nAETHER_SOCKET={}\n",
            env.path("no-aether.sock").display()
        )
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn shared_tcp_tui_is_rejected_before_attachment_or_exec() {
    for socket in ["", "socket = \"configured-aether.sock\"\n"] {
        let env = environment_with_config(&format!(
            "[aether]\nmode = \"shared\"\naddress = \"127.0.0.1:7777\"\n{socket}"
        ));
        let listener = std::os::unix::net::UnixListener::bind(env.path("archond.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let output = env
            .command(&["tui"])
            .env("PATH", env.path("bin"))
            .env_remove("AETHER_TOKEN")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            output.stdout.is_empty(),
            "Aether must not launch: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("does not support [aether] address (TCP)"),
            "{output:?}"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "archond must not be contacted"
        );
    }
}

#[test]
fn raw_shared_aether_passthrough_keeps_native_endpoint_selection() {
    for endpoint in [
        "socket = \"configured-aether.sock\"",
        "address = \"127.0.0.1:7777\"",
    ] {
        let env = environment_with_config(&format!("[aether]\nmode = \"shared\"\n{endpoint}\n"));
        record_endpoint(&env);
        let output = env
            .command(&["aether", "agents", "--json"])
            .env("PATH", env.path("bin"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "agents\n--json\nAETHER_SOCKET={}\n",
                env.path("no-aether.sock").display()
            )
        );
        assert!(output.stderr.is_empty());
        assert!(!env.path("archond.sock").exists());
    }
}

#[test]
fn tui_focuses_the_resolved_orchestrator_and_forwards_native_options_and_environment() {
    for no_color in ["", "1"] {
        let env = environment();
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let output = env
            .command(&[
                "tui",
                "--theme",
                "light",
                "--ascii",
                "--reduced-motion",
                "--plain",
            ])
            .env("PATH", env.path("bin"))
            .env("NO_COLOR", no_color)
            .env("AETHER_THEME", "nord")
            .env("AETHER_ASCII", "1")
            .env("AETHER_REDUCED_MOTION", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "tui\n--agent\nagt_selected\n--theme\nlight\n--ascii\n--reduced-motion\n--plain\nNO_COLOR={no_color}\nAETHER_THEME=nord\nAETHER_ASCII=1\nAETHER_REDUCED_MOTION=1\n"
            )
        );
        assert!(output.stderr.is_empty());
        let requests = server.join().unwrap();
        assert!(matches!(
            requests.as_slice(),
            [Request::Hello { .. }, Request::OrchestratorAttach]
        ));
    }
}

#[test]
fn startup_link_unavailability_is_retried_without_a_user_message() {
    let env = environment();
    let server = fake::serve(
        &env.path("archond.sock"),
        1,
        vec![
            Err(ErrorReply {
                code: ErrorCode::Unavailable,
                message: "not connected to aetherd".into(),
            }),
            Ok(fake::orchestrator()),
        ],
    );
    let output = env
        .command(&["tui"])
        .env("PATH", env.path("bin"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("tui\n--agent\nagt_selected\nNO_COLOR=")
    );
    assert!(matches!(
        server.join().unwrap().as_slice(),
        [
            Request::Hello { .. },
            Request::OrchestratorAttach,
            Request::OrchestratorAttach
        ]
    ));
}

#[test]
fn attachment_failure_or_wrong_reply_stops_before_exec() {
    for reply in [
        Err(ErrorReply {
            code: ErrorCode::Forbidden,
            message: "attachment denied".into(),
        }),
        Ok(Reply::Ok),
    ] {
        let env = environment();
        let server = fake::serve(&env.path("archond.sock"), 1, vec![reply]);
        let output = env
            .command(&["tui"])
            .env("PATH", env.path("bin"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            output.stdout.is_empty(),
            "Aether must not launch: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("attaching to Archon's orchestrator")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[test]
fn a_missing_aether_executable_is_reported_after_attachment() {
    let env = Env::new();
    std::fs::create_dir(env.path("empty-bin")).unwrap();
    let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
    let output = env
        .command(&["tui"])
        .env("PATH", env.path("empty-bin"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("is Aether installed?"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn dedicated_tui_seeds_calm_and_overrides_inherited_mode() {
    for (options, mode) in [(vec!["tui"], "calm"), (vec!["tui", "--normal"], "standard")] {
        let env = environment_with_config("");
        let _aetherd = std::os::unix::net::UnixListener::bind(env.path("aetherd.sock")).unwrap();
        std::fs::create_dir_all(env.path("config/aether")).unwrap();
        std::fs::write(env.path("config/aether/config.toml"), "").unwrap();
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let output = env
            .command(&options)
            .env("PATH", env.path("bin"))
            .env("AETHER_VIEW_MODE", "focus")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .starts_with(&format!(
                    "tui\n--agent\nagt_selected\n--view-mode\n{mode}\n"
                ))
        );
        let definition =
            std::fs::read_to_string(env.path("config/aether/modes/calm.toml")).unwrap();
        assert!(definition.contains("role = \"archon-orchestrator\""));
        assert!(!env.path("aether/modes/calm.toml").exists());
        assert_eq!(server.join().unwrap().len(), 3);
    }
}

#[test]
fn shared_normal_is_explicit_and_does_not_seed_operator_config() {
    let env = environment();
    let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
    let output = env
        .command(&["tui", "--normal"])
        .env("PATH", env.path("bin"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("tui\n--agent\nagt_selected\n--view-mode\nstandard\n")
    );
    assert!(!env.path("aether/modes/calm.toml").exists());
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn dedicated_tui_falls_back_visibly_for_unusable_calm_definitions() {
    for definition in [Some("version = 99\nshow = []"), Some("invalid = ["), None] {
        let env = environment_with_config("");
        let _aetherd = std::os::unix::net::UnixListener::bind(env.path("aetherd.sock")).unwrap();
        std::fs::create_dir_all(env.path("config/aether/modes")).unwrap();
        std::fs::write(env.path("config/aether/config.toml"), "").unwrap();
        let path = env.path("config/aether/modes/calm.toml");
        if let Some(text) = definition {
            std::fs::write(&path, text).unwrap();
        } else {
            std::os::unix::fs::symlink("/missing/calm.toml", &path).unwrap();
        }
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let out = env
            .command(&["tui"])
            .env("PATH", env.path("bin"))
            .env("AETHER_VIEW_MODE", "focus")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .starts_with("tui\n--agent\nagt_selected\n--view-mode\nstandard\n")
        );
        assert!(
            String::from_utf8(out.stderr)
                .unwrap()
                .contains("calm view unavailable")
        );
        if let Some(text) = definition {
            assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        } else {
            assert!(std::fs::symlink_metadata(path).unwrap().is_symlink());
        }
        server.join().unwrap();
    }
}
