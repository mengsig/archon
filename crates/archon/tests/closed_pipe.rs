//! A reader that closes stdout early (`archon ... | head -1`) ends the
//! command quietly instead of with a panic.

use std::process::{Command, Stdio};

#[test]
fn a_closed_stdout_pipe_prints_no_panic() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_archon"))
        .args(["pipelines", "list"])
        .env("ARCHON_CONFIG_DIR", home.path().join("config"))
        .env("ARCHON_DATA_DIR", home.path().join("data"))
        .env("ARCHON_SOCKET", home.path().join("archond.sock"))
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running the archon binary");
    // The command is still starting up, so its first write meets the closed pipe.
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(141), "{stderr}");
}
