//! A bare `archon`: the terminal UI on a terminal, the line chat with one
//! line saying why where the terminal UI cannot run, and `archon chat`.

mod common;
#[path = "common/tui.rs"]
mod fake;
#[path = "common/terminal.rs"]
mod terminal;

use std::fs::File;
use std::process::Stdio;

use common::{Daemon, Env, start_daemon, stop_daemon, wait_until_running};
use terminal::Terminal;

const CHAT_OPENED: &str = "/help for commands";
const TOKEN: &str = "a-test-token-of-at-least-32-characters";
const PROMPT: &str = "› ";
const NOT_A_TERMINAL: &str = "archon: standard input or output is not a terminal; \
                              opening the line chat (`archon chat`) instead\n";
const TCP_ADDRESS: &str = "archon: [aether] address (TCP) is set, which the terminal UI cannot \
                           use; opening the line chat (`archon chat`) instead\n";

const RECORD_THE_TERMINAL: &str = "on=pipe\n[ -t 0 ] && [ -t 1 ] && on=terminal";

/// An `aether` on `PATH` that lists the `tui` options Archon passes, and
/// records its arguments and whether it was given the terminal in `launched`.
fn with_recording_aether(config: &str) -> Env {
    let help = "printf '      %s\\n' '--agent <AGENT>' '--view-mode <VIEW_MODE>' \
                '--host-commands <PATH>'\nexit 0";
    with_aether(config, help, RECORD_THE_TERMINAL)
}

/// An `aether` on `PATH` that runs `help` for `tui --help`; for anything else
/// it runs `launch`, then records its arguments and `$on` in `launched`.
fn with_aether(config: &str, help: &str, launch: &str) -> Env {
    let env = Env::with_config(config);
    std::fs::create_dir(env.path("bin")).unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"$2\" = --help ]; then\n{help}\nfi\n{launch}\n\
         printf '%s\\n' \"$@\" \"$on\" > '{}'\n[ -z \"$refusal\" ] && exit 0\n\
         echo \"$refusal\" >&2\nexit 2\n",
        env.path("launched").display()
    );
    archon_core::testing::write_script(&env.path("bin/aether"), &script);
    env
}

/// Archon's own Aether, set up, with its aetherd socket bound.
fn dedicated(env: &Env) -> std::os::unix::net::UnixListener {
    std::fs::create_dir_all(env.path("config/aether")).unwrap();
    std::fs::write(env.path("config/aether/config.toml"), "").unwrap();
    std::os::unix::net::UnixListener::bind(env.path("aetherd.sock")).unwrap()
}

/// archond for a TCP `[aether] address`, which it refuses without a token.
fn start_daemon_with_token(env: &Env) -> Daemon {
    let log = File::create(env.path("daemon.log")).unwrap();
    let child = env
        .command(&["daemon", "run"])
        .env("AETHER_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    wait_until_running(env, &mut daemon);
    daemon
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn a_bare_archon_on_a_terminal_opens_the_terminal_ui_as_archon_tui_does() {
    for args in [&[][..], &["tui"]] {
        let env = with_recording_aether("");
        let _aetherd = dedicated(&env);
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let terminal = Terminal::open();
        let out = terminal.run(env.command(args).env("PATH", env.path("bin")));
        assert!(out.status.success(), "archon {args:?}: {out:?}");
        assert_eq!(text(&out.stderr), "", "archon {args:?} says nothing itself");
        let commands = env.path("data/tui-host-commands.json");
        assert_eq!(
            std::fs::read_to_string(env.path("launched")).unwrap(),
            format!(
                "tui\n--agent\nagt_selected\n--view-mode\ncalm\n--host-commands\n{}\nterminal\n",
                commands.display()
            ),
            "archon {args:?}"
        );
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&commands).unwrap()).unwrap();
        let plain = archon_core::operator_commands::welcome_lines(None);
        assert_eq!(
            written,
            archon_core::operator_commands::host_commands_json(Some(&plain)),
            "an archond with no status gets the plain welcome"
        );
        assert_eq!(server.join().unwrap().len(), 3, "hello, attach, status");
    }
}

#[test]
fn an_aether_without_host_commands_opens_the_terminal_ui_without_them_and_says_so() {
    let help = "printf '      %s\\n' '--agent <AGENT>' '--view-mode <VIEW_MODE>'\nexit 0";
    for args in [&[][..], &["tui"]] {
        let env = with_aether("", help, RECORD_THE_TERMINAL);
        let _aetherd = dedicated(&env);
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let terminal = Terminal::open();
        let out = terminal.run(env.command(args).env("PATH", env.path("bin")));
        assert!(out.status.success(), "archon {args:?}: {out:?}");
        let stderr = text(&out.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(stderr.contains("takes no --host-commands"), "{stderr}");
        assert_eq!(
            std::fs::read_to_string(env.path("launched")).unwrap(),
            "tui\n--agent\nagt_selected\n--view-mode\ncalm\nterminal\n",
            "archon {args:?}"
        );
        assert!(!env.path("data/tui-host-commands.json").exists());
        assert_eq!(server.join().unwrap().len(), 3);
    }
}

#[test]
fn a_terminal_ui_that_cannot_open_is_an_error_not_a_fallback() {
    let env = Env::new();
    std::fs::create_dir(env.path("empty-bin")).unwrap();
    let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
    let terminal = Terminal::open();
    let out = terminal.run(env.command(&[]).env("PATH", env.path("empty-bin")));
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = text(&out.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    for said in ["`archon chat`", "`archon doctor`", "is Aether installed?"] {
        assert!(stderr.contains(said), "no {said:?} in: {stderr}");
    }
    assert!(!stderr.contains("opening the line chat"), "{stderr}");
    assert_eq!(server.join().unwrap().len(), 2, "no chat request was made");
}

#[test]
fn an_aether_that_rejects_the_terminal_ui_options_is_an_error_before_the_handoff() {
    let refusal = "error: unexpected argument '--view-mode' found";
    let launch = format!("refusal=\"{refusal}\"");
    let refuses_help = format!("echo \"{refusal}\" >&2\nexit 2");
    let lacks_the_option = "echo '      --agent <AGENT>  Focus this agent on start'\nexit 0";
    for (help, reason) in [
        (refuses_help.as_str(), refusal),
        (lacks_the_option, "lists no --view-mode"),
    ] {
        let env = with_aether("", help, &launch);
        let _aetherd = dedicated(&env);
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let terminal = Terminal::open();
        let out = terminal.run(env.command(&[]).env("PATH", env.path("bin")));
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let stderr = text(&out.stderr);
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        for said in ["`archon chat`", "`archon doctor`", "too old", reason] {
            assert!(stderr.contains(said), "no {said:?} in: {stderr}");
        }
        assert!(!stderr.contains("opening the line chat"), "{stderr}");
        assert!(!env.path("launched").exists(), "Aether must not launch");
        assert_eq!(server.join().unwrap().len(), 3, "no chat request was made");

        // `archon tui` hands over: it warns of the missing / menu, then Aether refuses.
        std::fs::remove_file(env.path("archond.sock")).unwrap();
        let server = fake::serve(&env.path("archond.sock"), 1, vec![Ok(fake::orchestrator())]);
        let out = terminal.run(env.command(&["tui"]).env("PATH", env.path("bin")));
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let stderr = text(&out.stderr);
        let (warning, refused) = stderr.split_once('\n').unwrap();
        assert!(warning.starts_with("Warning: "), "{stderr}");
        assert!(warning.contains("/ menu"), "{stderr}");
        assert_eq!(refused, format!("{refusal}\n"));
        assert!(
            env.path("launched").exists(),
            "`archon tui` launches Aether"
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }
}

#[test]
fn off_a_terminal_a_bare_archon_opens_the_line_chat_and_says_why_in_one_line() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let bare = env.run(&[]);
    assert!(text(&bare.stdout).contains(CHAT_OPENED), "{bare:?}");
    assert_eq!(text(&bare.stderr), NOT_A_TERMINAL);
    assert!(!env.path("launched").exists());
    stop_daemon(&env, daemon);
}

#[test]
fn archon_chat_still_opens_the_line_chat_and_gives_no_reason() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    for args in [&["chat"][..], &["chat", "--progress"]] {
        let chat = env.run(args);
        assert!(text(&chat.stdout).contains(CHAT_OPENED), "{chat:?}");
        assert_eq!(text(&chat.stderr), "", "archon {args:?}");
    }
    let terminal = Terminal::open();
    let mut chat = terminal.start(&mut env.command(&["chat"]));
    chat.wait_for(CHAT_OPENED);
    chat.wait_for(PROMPT);
    chat.end_input();
    let out = chat.finish();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        text(&out.stderr),
        "",
        "on a terminal `archon chat` is the chat too"
    );
    stop_daemon(&env, daemon);
}

#[test]
fn with_a_tcp_aether_address_a_bare_archon_on_a_terminal_opens_the_line_chat() {
    // A local port nothing listens on: the address only has to be configured.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    drop(port);
    let env = with_recording_aether(&format!(
        "[aether]\nmode = \"shared\"\naddress = \"{address}\"\n"
    ));
    let daemon = start_daemon_with_token(&env);
    let terminal = Terminal::open();
    let mut command = env.command(&[]);
    command
        .env("PATH", env.path("bin"))
        .env("AETHER_TOKEN", TOKEN);
    let mut bare = terminal.start(&mut command);
    bare.wait_for(CHAT_OPENED);
    bare.wait_for(PROMPT);
    bare.end_input();
    let out = bare.finish();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(text(&out.stderr), TCP_ADDRESS);
    assert!(!env.path("launched").exists(), "Aether must not launch");
    stop_daemon(&env, daemon);
}

#[test]
fn json_is_refused_for_a_bare_archon_before_anything_opens() {
    let env = with_recording_aether("[aether]\nmode = \"shared\"\n");
    let terminal = Terminal::open();
    for out in [
        env.command(&["--json"]).output().unwrap(),
        terminal.run(env.command(&["--json"]).env("PATH", env.path("bin"))),
    ] {
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let stderr = text(&out.stderr);
        assert!(!stderr.contains("opening the line chat"), "{stderr}");
    }
    let refused = env.command(&["--json"]).output().unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(
        envelope["error"]["message"],
        "invalid argument: --json is not supported here: a bare `archon` opens the terminal UI, \
         or the line chat, and both are interactive"
    );
    assert!(!env.path("launched").exists(), "Aether must not launch");
    assert!(!env.path("archond.sock").exists(), "archond must not start");
}
