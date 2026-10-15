//! A fake installer and restart helper: it records what an upgrade asks of
//! the machine and runs nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_daemon::ops::{InstallStep, OpsPort, OpsState, Program, RestartPlan};

/// How often a test's drain counts what still runs.
pub const TEST_DRAIN_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
pub struct FakeOps {
    pub state: Mutex<FakeOpsState>,
}

#[derive(Debug, Default, Clone)]
pub struct FakeOpsState {
    /// Where each program is installed; absent is not installed.
    pub binaries: BTreeMap<Program, PathBuf>,
    pub cargo_bin: Option<PathBuf>,
    pub installed: BTreeMap<Program, String>,
    pub latest: BTreeMap<Program, String>,
    /// Why every install fails; `None` lets them pass.
    pub install_fails: Option<String>,
    pub restart_fails: bool,
    pub installs: Vec<InstallStep>,
    pub restarts: Vec<RestartPlan>,
    pub latest_checks: u32,
    pub probe_barrier: Option<Arc<tokio::sync::Barrier>>,
}

impl FakeOps {
    pub fn with(state: FakeOpsState) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> FakeOpsState {
        self.state.lock().expect("fake ops").clone()
    }

    pub fn ops_state(self: &Arc<Self>) -> OpsState {
        OpsState::new(self.clone(), TEST_DRAIN_POLL)
    }
}

#[async_trait::async_trait]
impl OpsPort for FakeOps {
    fn locate(&self, program: Program) -> Option<PathBuf> {
        self.snapshot().binaries.get(&program).cloned()
    }

    fn cargo_bin(&self) -> Option<PathBuf> {
        self.snapshot().cargo_bin
    }

    async fn install(&self, step: &InstallStep) -> Result<(), ArchonError> {
        let mut state = self.state.lock().expect("fake ops");
        state.installs.push(step.clone());
        match &state.install_fails {
            Some(why) => Err(ArchonError::Unavailable(why.clone())),
            None => Ok(()),
        }
    }

    async fn installed_version(&self, program: Program) -> Result<Option<String>, ArchonError> {
        let snapshot = self.snapshot();
        if let Some(barrier) = snapshot.probe_barrier {
            barrier.wait().await;
        }
        Ok(snapshot.installed.get(&program).cloned())
    }

    async fn latest_release(&self, program: Program) -> Result<Option<String>, ArchonError> {
        let snapshot = {
            let mut state = self.state.lock().expect("fake ops");
            state.latest_checks += 1;
            state.clone()
        };
        if let Some(barrier) = snapshot.probe_barrier {
            barrier.wait().await;
        }
        Ok(snapshot.latest.get(&program).cloned())
    }

    fn spawn_restart(&self, plan: &RestartPlan) -> Result<(), ArchonError> {
        let mut state = self.state.lock().expect("fake ops");
        if state.restart_fails {
            return Err(ArchonError::Unavailable("the helper did not start".into()));
        }
        state.restarts.push(plan.clone());
        Ok(())
    }
}
