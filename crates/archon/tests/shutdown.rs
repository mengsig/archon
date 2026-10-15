//! `archon daemon stop` ends archond within the bench's grace period even
//! while a git it started still runs: blocking work in flight at shutdown
//! is left behind, and the log says so.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::{Daemon, Env, git_repo, wait_until_running};

/// The bench's `SHUTDOWN_GRACE`: an archond still running then is killed.
const GRACE: Duration = Duration::from_secs(20);

/// A `git` that, while `hold` exists, says it started and then waits; the
/// real git runs once `hold` is gone. Removing `hold` on drop frees it.
struct HeldGit {
    bin: PathBuf,
    hold: PathBuf,
    started: PathBuf,
}

impl HeldGit {
    fn install(env: &Env) -> Self {
        let bin = env.path("held-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let (hold, started) = (env.path("git.hold"), env.path("git.started"));
        let script = format!(
            "#!/bin/sh\nif [ -e '{hold}' ]; then\n  touch '{started}'\n  \
             while [ -e '{hold}' ]; do sleep 0.05; done\nfi\nexec '{git}' \"$@\"\n",
            hold = hold.display(),
            started = started.display(),
            git = real_git().display(),
        );
        let git = bin.join("git");
        std::fs::write(&git, script).unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { bin, hold, started }
    }

    fn hold(&self) {
        std::fs::write(&self.hold, "").unwrap();
    }

    fn wait_until_started(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.started.exists() {
            assert!(Instant::now() < deadline, "archond never ran the held git");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `PATH` with this git first.
    fn path(&self) -> String {
        let rest = std::env::var("PATH").unwrap_or_default();
        format!("{}:{rest}", self.bin.display())
    }
}

impl Drop for HeldGit {
    fn drop(&mut self) {
        let _already_gone = std::fs::remove_file(&self.hold);
    }
}

fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|git| git.is_file())
        .expect("git on PATH")
}

/// A CLI request left waiting on archond; killed when the test ends.
struct Pending(Child);

impl Drop for Pending {
    fn drop(&mut self) {
        let _already_gone = self.0.kill();
        let _reaped = self.0.wait();
    }
}

fn start_daemon_with(env: &Env, git: &HeldGit) -> Daemon {
    let log = std::fs::File::create(env.path("daemon.log")).unwrap();
    let child = env
        .command(&["daemon", "run"])
        .env("PATH", git.path())
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    wait_until_running(env, &mut daemon);
    daemon
}

fn exited_within(daemon: &mut Daemon, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if daemon.0.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn daemon_log(env: &Env) -> String {
    std::fs::read_to_string(env.path("daemon.log")).unwrap_or_default()
}

#[test]
fn shutdown_does_not_wait_for_a_git_still_running() {
    let env = Env::new();
    let git = HeldGit::install(&env);
    let mut daemon = start_daemon_with(&env, &git);
    let repo = env.path("demo");
    git_repo(&repo);

    git.hold();
    let adding = env
        .command(&["repo", "add", repo.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _adding = Pending(adding);
    git.wait_until_started();

    let stop = env.command(&["daemon", "stop"]).output().unwrap();
    let stopped = exited_within(&mut daemon, GRACE);
    let log = daemon_log(&env);
    assert!(
        stopped,
        "archond still runs {GRACE:?} after shutdown ({stop:?}):\n{log}"
    );
    assert!(
        log.contains("still running") && log.contains("archond stopped"),
        "the log says what was left behind:\n{log}"
    );
}
