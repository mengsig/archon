//! The host-load hold (ARCHITECTURE §7.3): while the 1-minute load average
//! per CPU is above `[limits] max_load_per_cpu`, or `MemAvailable` below
//! `min_available_memory_gb`, nothing new starts. Running agents finish.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use archon_core::error::ArchonError;
use archon_core::host_load::{
    self, HostLimits, HostLoadHold, HostSample, parse_loadavg, parse_meminfo_available_gb,
};
use archon_core::protocol::HostLoadStatus;

const LOADAVG: &str = "/proc/loadavg";
const MEMINFO: &str = "/proc/meminfo";

/// Why the host could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFailure {
    /// The platform has no `/proc`: the hold cannot work here, and does not hold.
    NoProc(String),
    /// `/proc` exists but a read or parse failed: the hold stays as it was.
    Failed(String),
}

pub type HostProbe = Arc<dyn Fn() -> Result<HostSample, ReadFailure> + Send + Sync>;

/// The last judgment, shared by the dispatcher and `status`.
#[derive(Debug, Default)]
struct WatchState {
    hold: Option<HostLoadHold>,
    sample: Option<HostSample>,
    unreadable: Option<String>,
}

pub struct HostWatch {
    probe: HostProbe,
    state: Mutex<WatchState>,
}

impl std::fmt::Debug for HostWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostWatch")
            .field("state", &*self.lock())
            .finish_non_exhaustive()
    }
}

impl HostWatch {
    pub fn new(probe: HostProbe) -> Self {
        Self {
            probe,
            state: Mutex::new(WatchState::default()),
        }
    }

    pub fn system() -> Self {
        Self::new(Arc::new(|| read_system(Path::new("/proc"))))
    }

    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().expect("host watch lock")
    }

    /// The hold the planner sees: the last judgment.
    pub fn hold(&self) -> Option<HostLoadHold> {
        self.lock().hold.clone()
    }

    /// Read the host and judge it. With both limits unset nothing is read
    /// and any earlier hold ends.
    pub fn check(&self, limits: HostLimits) {
        let mut state = self.lock();
        if limits.max_load_per_cpu.is_none() && limits.min_available_memory_gb.is_none() {
            *state = WatchState::default();
            return;
        }
        match (self.probe)() {
            Ok(sample) => {
                let hold = host_load::evaluate(state.hold.as_ref(), sample, limits);
                log_change(state.hold.as_ref(), hold.as_ref());
                *state = WatchState {
                    hold,
                    sample: Some(sample),
                    unreadable: None,
                };
            }
            Err(ReadFailure::NoProc(why)) => {
                note_unreadable(&state, &why, "the host-load hold cannot work here");
                *state = WatchState {
                    unreadable: Some(why),
                    ..WatchState::default()
                };
            }
            Err(ReadFailure::Failed(why)) => {
                note_unreadable(&state, &why, "the host-load hold stays as it was");
                state.sample = None;
                state.unreadable = Some(why);
            }
        }
    }

    /// What `archon status` shows; `None` while both limits are unset.
    pub fn status(&self, limits: HostLimits) -> Option<HostLoadStatus> {
        if limits.max_load_per_cpu.is_none() && limits.min_available_memory_gb.is_none() {
            return None;
        }
        let state = self.lock();
        assert!(
            state.sample.is_none() || state.unreadable.is_none(),
            "a reading and a failure are exclusive"
        );
        Some(HostLoadStatus {
            sample: state.sample,
            max_load_per_cpu: limits.max_load_per_cpu,
            min_available_memory_gb: limits.min_available_memory_gb,
            hold: state.hold.clone(),
            unreadable: state.unreadable.clone(),
        })
    }
}

fn note_unreadable(state: &WatchState, why: &str, effect: &str) {
    if state.unreadable.as_deref() != Some(why) {
        tracing::warn!(error = %why, "host load unknown; {effect}");
    }
}

fn log_change(before: Option<&HostLoadHold>, after: Option<&HostLoadHold>) {
    match (before, after) {
        (None, Some(hold)) => tracing::info!("{}", hold.describe()),
        (Some(_), None) => {
            tracing::info!("host load and memory are back within limits; dispatch resumes")
        }
        _ => {}
    }
}

/// Read both files under `proc` (a fixture directory in tests).
pub fn read_system(proc: &Path) -> Result<HostSample, ReadFailure> {
    let loadavg = read_file(proc, "loadavg")?;
    let meminfo = read_file(proc, "meminfo")?;
    let cpus = std::thread::available_parallelism()
        .map_err(|e| ReadFailure::Failed(format!("cannot count the CPUs: {e}")))?
        .get() as f64;
    let judged = |e: ArchonError| ReadFailure::Failed(e.to_string());
    let sample = HostSample {
        load_per_cpu: parse_loadavg(&loadavg).map_err(judged)? / cpus,
        available_gb: parse_meminfo_available_gb(&meminfo).map_err(judged)?,
    };
    assert!(sample.load_per_cpu >= 0.0 && sample.available_gb >= 0.0);
    Ok(sample)
}

fn read_file(proc: &Path, name: &str) -> Result<String, ReadFailure> {
    let path = proc.join(name);
    std::fs::read_to_string(&path).map_err(|e| {
        let why = format!("cannot read {}: {e}", path.display());
        if proc.exists() {
            ReadFailure::Failed(why)
        } else {
            ReadFailure::NoProc(format!(
                "{why}; this platform has no {LOADAVG} or {MEMINFO}"
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: HostLimits = HostLimits {
        max_load_per_cpu: Some(1.5),
        min_available_memory_gb: Some(2.0),
    };

    /// A probe that returns the next scripted reading.
    fn scripted(readings: Vec<Result<HostSample, ReadFailure>>) -> HostWatch {
        let queue = Mutex::new(readings.into_iter());
        HostWatch::new(Arc::new(move || {
            queue.lock().unwrap().next().expect("scripted reading")
        }))
    }

    fn ok(load_per_cpu: f64, available_gb: f64) -> Result<HostSample, ReadFailure> {
        Ok(HostSample {
            load_per_cpu,
            available_gb,
        })
    }

    #[test]
    fn dispatch_holds_over_the_limit_and_resumes_only_with_margin() {
        let watch = scripted(vec![ok(1.0, 8.0), ok(1.6, 8.0), ok(1.4, 8.0), ok(1.3, 8.0)]);
        let held = |w: &HostWatch| {
            w.check(LIMITS);
            w.hold().is_some()
        };
        assert!(!held(&watch));
        assert!(held(&watch), "1.6 is over 1.5");
        assert!(held(&watch), "1.4 is not below 1.35");
        assert!(!held(&watch), "1.3 is");
    }

    #[test]
    fn memory_hold_has_its_own_margin() {
        let watch = scripted(vec![ok(0.1, 1.0), ok(0.1, 2.1), ok(0.1, 2.3)]);
        watch.check(LIMITS);
        assert!(watch.hold().is_some());
        watch.check(LIMITS);
        assert!(watch.hold().is_some(), "2.1 is not above 2.2");
        watch.check(LIMITS);
        assert!(watch.hold().is_none());
    }

    #[test]
    fn a_failed_read_keeps_the_hold_and_says_why_in_status() {
        let watch = scripted(vec![ok(3.0, 8.0), Err(ReadFailure::Failed("boom".into()))]);
        watch.check(LIMITS);
        watch.check(LIMITS);
        assert!(
            watch.hold().is_some(),
            "an unreadable host is not a release"
        );
        let status = watch.status(LIMITS).unwrap();
        assert!(status.sample.is_none());
        assert_eq!(status.unreadable.as_deref(), Some("boom"));
    }

    #[test]
    fn no_proc_does_not_hold_and_status_says_so() {
        let watch = scripted(vec![
            ok(3.0, 8.0),
            Err(ReadFailure::NoProc("no /proc".into())),
        ]);
        watch.check(LIMITS);
        watch.check(LIMITS);
        assert!(watch.hold().is_none());
        assert_eq!(
            watch.status(LIMITS).unwrap().unreadable.as_deref(),
            Some("no /proc")
        );
    }

    #[test]
    fn unset_limits_read_nothing_and_show_nothing() {
        let watch = scripted(vec![]);
        let off = HostLimits {
            max_load_per_cpu: None,
            min_available_memory_gb: None,
        };
        watch.check(off);
        assert!(watch.hold().is_none() && watch.status(off).is_none());
    }

    #[test]
    fn proc_fixtures_are_read_and_a_missing_proc_is_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loadavg"), "0.00 0.00 0.00 1/1 1\n").unwrap();
        std::fs::write(dir.path().join("meminfo"), "MemAvailable: 1048576 kB\n").unwrap();
        let sample = read_system(dir.path()).unwrap();
        assert_eq!((sample.load_per_cpu, sample.available_gb), (0.0, 1.0));

        std::fs::write(dir.path().join("meminfo"), "MemTotal: 1 kB\n").unwrap();
        assert!(matches!(
            read_system(dir.path()),
            Err(ReadFailure::Failed(_))
        ));

        let gone = dir.path().join("absent");
        assert!(matches!(read_system(&gone), Err(ReadFailure::NoProc(_))));
    }
}
