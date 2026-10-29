//! A server a stage declares in `.archon/services/` outlives that stage's
//! end, for the next stage to use, and goes when the run ends.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use archon_core::protocol::Request;

use crate::common::{self, Harness};

/// `sleep 300` orphaned in `cwd`, as an agent's `cmd &` leaves it.
fn orphan(cwd: &Path) -> i32 {
    let out = Command::new("sh")
        .args(["-c", "sleep 300 </dev/null >/dev/null 2>&1 & echo $!"])
        .env("AETHER_ENGINE_RUN", "archon-test")
        .current_dir(cwd)
        .output()
        .expect("sh runs");
    assert!(out.status.success(), "the launching shell exits");
    let pid = String::from_utf8(out.stdout).expect("a pid").trim().parse();
    pid.expect("the shell printed the pid")
}

/// Whether `pid` runs: listed, and neither a zombie nor dead.
fn runs(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next());
    !matches!(state, Some('Z' | 'X'))
}

/// Whether `pid` ends within a few seconds.
fn ends(pid: i32) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if !runs(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Kills what a failed test leaves running.
struct Started(Vec<i32>);

impl Drop for Started {
    fn drop(&mut self) {
        for pid in self.0.iter().filter(|&&pid| runs(pid)) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_declared_service_outlives_its_stage_and_ends_with_the_run() {
    let h = Harness::new().await;
    let item = h.enqueue("serve the app for review").await;
    h.dispatch().await;
    let tree = h.run(&item.id).await.workspace;
    let (server, stray) = (orphan(&tree), orphan(&tree));
    let _started = Started(vec![server, stray]);
    let services = tree.join(".archon/services");
    std::fs::create_dir_all(&services).unwrap();
    std::fs::write(services.join("web.pid"), format!("{server}\n")).unwrap();

    h.settle_running(|_| common::completed("the app serves on :8080"))
        .await;
    h.dispatch().await;
    assert!(ends(stray), "the stage end reaps what nobody declared");
    assert!(runs(server), "the declared server serves the next stage");
    assert!(!h.running().await.is_empty(), "the run went on to review");

    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    assert!(ends(server), "the run's end stops the service");
}
