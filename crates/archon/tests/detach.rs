//! `archon daemon run --detach`: the daemon runs in the data directory, not
//! the caller's, relative directory overrides keep the caller's meaning, and
//! the startup log keeps earlier sessions.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Stops the detached daemon even when the test fails first.
struct Detached {
    caller: PathBuf,
    pid: Option<u32>,
}

impl Detached {
    fn archon(&self, args: &[&str]) -> Output {
        // Relative on purpose: they mean something only from `caller`.
        Command::new(env!("CARGO_BIN_EXE_archon"))
            .args(args)
            .current_dir(&self.caller)
            .env("ARCHON_DATA_DIR", "data")
            .env("ARCHON_CONFIG_DIR", "../config")
            .env("ARCHON_SOCKET", "../run/archond.sock")
            .env("AETHER_SOCKET", self.caller.join("no-aether.sock"))
            .env("NO_COLOR", "1")
            .output()
            .expect("running archon")
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        let Some(pid) = self.pid else { return };
        let stopped = self.archon(&["daemon", "stop"]);
        if !stopped.status.success() {
            let _already_gone = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

fn wait_until_listening(detached: &Detached) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !detached.archon(&["daemon", "status"]).status.success() {
        assert!(Instant::now() < deadline, "archond never listened");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn cwd_of(pid: u32) -> PathBuf {
    std::fs::read_link(Path::new("/proc").join(pid.to_string()).join("cwd")).unwrap()
}

#[test]
fn the_detached_daemon_runs_in_the_data_dir_the_caller_named() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let config = home.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[aether]\nmode = \"shared\"\n").unwrap();
    let caller = home.join("caller");
    std::fs::create_dir_all(&caller).unwrap();
    let run = home.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("archond.log"), "an earlier session's last line\n").unwrap();
    let mut detached = Detached {
        caller: caller.clone(),
        pid: None,
    };

    let out = detached.archon(&["--json", "daemon", "run", "--detach"]);
    assert!(out.status.success(), "detach failed: {out:?}");
    let started: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pid = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    detached.pid = Some(pid);
    wait_until_listening(&detached);

    let data = caller.join("data");
    assert_eq!(
        cwd_of(pid),
        data,
        "the daemon holds the data dir, not the caller's"
    );
    assert!(
        data.join("archon.db").is_file(),
        "the store is where the caller said"
    );
    assert!(
        !data.join("data").exists(),
        "a relative ARCHON_DATA_DIR is not resolved twice"
    );
    assert!(home.join("run/archond.sock").exists());
    let log = std::fs::read_to_string(run.join("archond.log")).unwrap();
    assert!(
        log.starts_with("an earlier session's last line\n"),
        "a start appends to the log: {log}"
    );
    assert_eq!(
        log.matches(archon_core::paths::STARTUP_LOG_MARK).count(),
        1,
        "each start marks where its output begins: {log}"
    );
}
