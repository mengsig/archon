//! The built binary against a private archond: directories, a daemon that
//! is killed if a test fails first, and a git repository to register.
#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub struct Env {
    home: tempfile::TempDir,
}

impl Env {
    /// Shared mode, so no command starts an Aether of its own.
    pub fn new() -> Self {
        Self::with_config("[aether]\nmode = \"shared\"\n")
    }

    /// Archon's `config.toml` is `config`.
    pub fn with_config(config: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        Self { home }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_archon"));
        command
            .args(args)
            .env("ARCHON_DATA_DIR", self.path("data"))
            .env("ARCHON_CONFIG_DIR", self.path("config"))
            .env("ARCHON_SOCKET", self.path("archond.sock"))
            .env("AETHER_SOCKET", self.path("no-aether.sock"))
            .env("AETHER_CONFIG_DIR", self.path("aether"))
            .env("NO_COLOR", "1");
        if args.contains(&"doctor") {
            let bin = self.path("offline-gh");
            std::fs::create_dir_all(&bin).unwrap();
            archon_core::testing::write_script(
                &bin.join("gh"),
                "#!/bin/sh\necho offline >&2\nexit 1\n",
            );
            let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )))
            .unwrap();
            command.env("PATH", path);
        }
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let out = self.command(args).output().expect("running archon");
        assert!(out.status.success(), "archon {args:?} failed: {out:?}");
        out
    }
}

impl Drop for Env {
    /// archond leaves the orchestrator's mirrors read-only, and the temporary
    /// directory's removal cannot enter them: open them first.
    fn drop(&mut self) {
        let mirrors = self.path("data/orchestrator/repos");
        if !mirrors.exists() || std::thread::panicking() {
            return;
        }
        let opened = Command::new("chmod")
            .arg("-R")
            .arg("u+w")
            .arg(&mirrors)
            .status();
        assert!(
            opened.is_ok_and(|status| status.success()),
            "chmod -R u+w {}",
            mirrors.display()
        );
    }
}

/// Kills the daemon if the test fails before stopping it.
pub struct Daemon(pub Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _already_gone = self.0.kill();
            let _reaped = self.0.wait();
        }
    }
}

/// How long a test waits for the archond it started to answer: generous,
/// because a loaded host may take many seconds to load a debug binary. Only
/// an upper bound, so a healthy start is not slowed down.
pub const START_WAIT: Duration = Duration::from_secs(60);

pub fn wait_until_running(env: &Env, daemon: &mut Daemon) {
    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            let log = std::fs::read_to_string(env.path("daemon.log")).unwrap_or_default();
            panic!("archond exited early ({status}):\n{log}");
        }
        if env
            .command(&["daemon", "status"])
            .output()
            .unwrap()
            .status
            .success()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let log = std::fs::read_to_string(env.path("daemon.log")).unwrap_or_default();
    panic!("archond did not start listening within {START_WAIT:?}:\n{log}");
}

pub fn git_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(&args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

pub fn start_daemon(env: &Env) -> Daemon {
    let log = std::fs::File::create(env.path("daemon.log")).unwrap();
    let child = env
        .command(&["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    wait_until_running(env, &mut daemon);
    daemon
}

pub fn stop_daemon(env: &Env, daemon: Daemon) {
    env.run(&["daemon", "stop"]);
    wait_for_exit(daemon);
}

/// Wait until archond, asked to stop, has exited.
pub fn wait_for_exit(mut daemon: Daemon) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "archond did not exit after daemon stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
