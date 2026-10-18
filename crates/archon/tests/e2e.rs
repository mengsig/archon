//! End to end: the built binary runs `archond` in a private data dir while
//! Aether is unreachable, and the CLI still works against it.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use archon_core::protocol::{PROTOCOL_VERSION, Reply, ServerFrame, Welcome};
use common::{Env, git_repo, start_daemon, stop_daemon};

#[test]
fn daemon_serves_the_cli_while_aether_is_down() {
    let env = Env::new();
    let daemon = start_daemon(&env);

    let status = String::from_utf8(env.run(&["status"]).stdout).unwrap();
    assert!(status.contains("aether: disconnected"), "{status}");

    let repo = env.path("demo");
    git_repo(&repo);
    let added = String::from_utf8(
        env.run(&["repo", "add", repo.to_str().unwrap(), "--ship", "local"])
            .stdout,
    )
    .unwrap();
    assert!(added.contains("demo"), "{added}");
    let repos = String::from_utf8(env.run(&["repo", "list"]).stdout).unwrap();
    assert!(repos.contains("demo"), "{repos}");

    let queue = String::from_utf8(env.run(&["queue", "list"]).stdout).unwrap();
    assert!(queue.contains("The queue is empty."), "{queue}");
    let items: serde_json::Value =
        serde_json::from_slice(&env.run(&["--json", "queue", "list"]).stdout).unwrap();
    assert_eq!(items, serde_json::json!([]));

    let orchestrator = String::from_utf8(env.run(&["orchestrator", "status"]).stdout).unwrap();
    assert_eq!(orchestrator.trim(), "orchestrator: not running");

    stop_daemon(&env, daemon);
}

#[test]
fn restart_with_an_invalid_config_is_refused_and_archond_keeps_running() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let config = env.path("config").join("config.toml");
    let valid = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{valid}bogus_key = 1\n")).unwrap();

    let restart = env.command(&["daemon", "restart"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&restart.stderr);
    assert!(!restart.status.success(), "{restart:?}");
    assert!(stderr.contains("bogus_key"), "{stderr}");
    assert!(
        env.command(&["daemon", "status"])
            .output()
            .unwrap()
            .status
            .success(),
        "archond stopped after a refused restart"
    );

    std::fs::write(&config, valid).unwrap();
    stop_daemon(&env, daemon);
}

/// A running `archon chat` with piped stdin, its stdout collected.
struct Chat {
    child: Child,
    out: Arc<Mutex<String>>,
}

impl Chat {
    fn start(env: &Env) -> Self {
        let mut child = env
            .command(&["chat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = Arc::new(Mutex::new(String::new()));
        let (stdout, sink) = (child.stdout.take().unwrap(), out.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let mut all = sink.lock().unwrap();
                all.push_str(&line);
                all.push('\n');
            }
        });
        Self { child, out }
    }

    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.out.lock().unwrap().contains(text) {
            let exited = self.child.try_wait().unwrap();
            assert!(
                exited.is_none(),
                "the chat exited ({exited:?}) before {text:?}"
            );
            assert!(
                Instant::now() < deadline,
                "no {text:?} in the chat:\n{}",
                self.out.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn count(&self, text: &str) -> usize {
        self.out.lock().unwrap().matches(text).count()
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        let _already_gone = self.child.kill();
        let _reaped = self.child.wait();
    }
}

/// `archon say` is refused while Aether is down, but archond still records
/// the line in the feed, which is all these tests need.
fn say(env: &Env, text: &str) {
    env.command(&["say", text]).output().unwrap();
}

#[test]
fn chat_follows_a_daemon_restart_without_restarting_it_itself() {
    let env = Env::new();
    let daemon = start_daemon(&env);
    let mut chat = Chat::start(&env);
    chat.wait_for("/help for commands");
    say(&env, "before the restart");
    chat.wait_for("before the restart");

    stop_daemon(&env, daemon);
    chat.wait_for("lost the connection to archond");
    chat.wait_for("archond is not running");
    let status = env.command(&["daemon", "status"]).output().unwrap();
    assert_eq!(
        status.status.code(),
        Some(3),
        "the chat left archond stopped"
    );

    let daemon = start_daemon(&env);
    chat.wait_for("reconnected to archond");
    say(&env, "after the restart");
    chat.wait_for("after the restart");
    drop(chat.child.stdin.take());
    let exit = chat.child.wait().unwrap();
    assert!(exit.success(), "end of input ends the chat cleanly");
    assert_eq!(chat.count("before the restart"), 1, "no line twice");
    assert_eq!(chat.count("after the restart"), 1);
    stop_daemon(&env, daemon);
}

#[test]
fn a_chat_that_cannot_open_exits_instead_of_waiting_for_input() {
    let env = Env::new();
    let listener = UnixListener::bind(env.path("archond.sock")).unwrap();
    // A daemon that welcomes the chat and then hangs up.
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut hello = String::new();
        BufReader::new(&stream).read_line(&mut hello).unwrap();
        let welcome = Welcome {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "fake".into(),
            pid: 1,
        };
        let frame = ServerFrame::Reply {
            id: 1,
            result: Ok(Reply::Welcome(welcome)),
        };
        let line = format!("{}\n", serde_json::to_string(&frame).unwrap());
        (&stream).write_all(line.as_bytes()).unwrap();
    });
    let mut chat = Chat::start(&env);
    server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(exit) = chat.child.try_wait().unwrap() {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "the chat still waits for input after failing to open"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!exit.success(), "a chat that could not open fails");
}
