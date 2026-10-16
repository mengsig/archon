//! A repository's full check, run in one build slot per host: a lock file
//! every archond of this user takes, so two full suites never overlap. It
//! runs at the lowest CPU priority, its output kept in a log file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use archon_core::config::IntegrationConfig;
use archon_core::error::ArchonError;
use tokio::sync::{Mutex, MutexGuard};

/// Taken before the host's lock, so one daemon's runs queue without a
/// blocked thread each.
static IN_PROCESS: Mutex<()> = Mutex::const_new(());
/// Bytes read from the end of a log for its excerpt.
const TAIL_BYTES: u64 = 64 * 1024;
/// Lines and characters of a failing run's output kept for its fix item.
const EXCERPT_LINES: usize = 60;
const EXCERPT_CHARS: usize = 6_000;

/// The host's one build slot for full checks, held until dropped.
pub(crate) struct Slot {
    _host: File,
    _process: MutexGuard<'static, ()>,
}

impl Slot {
    /// Wait for the slot: first this daemon's turn, then the host's lock at `path`.
    pub async fn acquire(path: &Path) -> Result<Self, ArchonError> {
        assert!(path.is_absolute(), "validated: the slot's lock is absolute");
        let process = IN_PROCESS.lock().await;
        let path = path.to_owned();
        let host = tokio::task::spawn_blocking(move || lock_file(&path))
            .await
            .map_err(|e| ArchonError::io("the build slot task", std::io::Error::other(e)))??;
        Ok(Self {
            _host: host,
            _process: process,
        })
    }
}

/// `[integration] slot_lock`, else `/tmp/archon-full-check-<uid>.lock`: the
/// same file for every data directory and every `TMPDIR` of this user.
pub(crate) fn slot_lock(config: &IntegrationConfig) -> PathBuf {
    config.slot_lock.clone().unwrap_or_else(|| {
        let uid = rustix::process::getuid().as_raw();
        PathBuf::from(format!("/tmp/archon-full-check-{uid}.lock"))
    })
}

/// Open `path` and take its exclusive lock, waiting while another holds it.
fn lock_file(path: &Path) -> Result<File, ArchonError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| ArchonError::io(format!("opening {}", path.display()), e))?;
    file.lock()
        .map_err(|e| ArchonError::io(format!("locking {}", path.display()), e))?;
    Ok(file)
}

/// How one full run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FullRun {
    pub passed: bool,
    /// The end of its output, and why it ended when it did not exit.
    pub excerpt: String,
}

/// Run `command` with `sh -c` in `work`, its output in `log`, ended with
/// everything it started after `limit`. The caller holds the [`Slot`].
pub(crate) async fn run(
    _slot: &Slot,
    command: &str,
    work: &Path,
    log: &Path,
    limit: Duration,
) -> Result<FullRun, ArchonError> {
    assert!(
        work.is_absolute() && log.is_absolute(),
        "daemon paths are absolute"
    );
    assert!(
        !command.trim().is_empty(),
        "validated: a full check is a command"
    );
    let output =
        File::create(log).map_err(|e| ArchonError::io(format!("creating {}", log.display()), e))?;
    let errors = output
        .try_clone()
        .map_err(|e| ArchonError::io(format!("sharing {}", log.display()), e))?;
    let mut child = command_in(work, command)
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors))
        .spawn()
        .map_err(|e| ArchonError::io(format!("running the full check `{command}`"), e))?;
    let group = child
        .id()
        .and_then(|pid| rustix::process::Pid::from_raw(pid as i32));
    let waited = tokio::time::timeout(limit, child.wait()).await;
    let ended = match waited {
        Ok(status) => Ended::Exited(status.map_err(|e| ArchonError::io("the full check", e))?),
        Err(_) => {
            if let Some(group) = group {
                // Gone already is fine: kill_on_drop reaps what is left.
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            Ended::TimedOut(limit)
        }
    };
    let tail = blocking_tail(log).await?;
    Ok(ended.into_run(tail))
}

enum Ended {
    Exited(ExitStatus),
    TimedOut(Duration),
}

impl Ended {
    fn into_run(self, tail: String) -> FullRun {
        match self {
            Self::Exited(status) if status.success() => FullRun {
                passed: true,
                excerpt: tail,
            },
            Self::Exited(status) => FullRun {
                passed: false,
                excerpt: format!("{tail}\n[the full check failed: {status}]"),
            },
            Self::TimedOut(limit) => FullRun {
                passed: false,
                excerpt: format!(
                    "{tail}\n[the full check was ended after {}s]",
                    limit.as_secs()
                ),
            },
        }
    }
}

/// `nice -n 19 sh -c <command>` in its own process group, with none of the
/// daemon's `GIT_*` environment.
fn command_in(work: &Path, command: &str) -> tokio::process::Command {
    let mut process = tokio::process::Command::new("nice");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            process.env_remove(key);
        }
    }
    process
        .args(["-n", "19", "sh", "-c", command])
        .current_dir(work)
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    process
}

async fn blocking_tail(log: &Path) -> Result<String, ArchonError> {
    let log = log.to_owned();
    tokio::task::spawn_blocking(move || tail(&log))
        .await
        .map_err(|e| ArchonError::io("reading the full check's log", std::io::Error::other(e)))?
}

/// The last [`EXCERPT_LINES`] lines of `log`, at most [`EXCERPT_CHARS`].
fn tail(log: &Path) -> Result<String, ArchonError> {
    let reading = |e| ArchonError::io(format!("reading {}", log.display()), e);
    let mut file = File::open(log).map_err(reading)?;
    let len = file.metadata().map_err(reading)?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .map_err(reading)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(reading)?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(EXCERPT_LINES)..].join("\n");
    let skip = kept.chars().count().saturating_sub(EXCERPT_CHARS);
    let excerpt: String = kept.chars().skip(skip).collect();
    assert!(
        excerpt.chars().count() <= EXCERPT_CHARS,
        "the excerpt is bounded"
    );
    Ok(excerpt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(20);

    async fn full(command: &str, dir: &Path, name: &str) -> FullRun {
        let slot = Slot::acquire(&dir.join("slot.lock")).await.unwrap();
        run(&slot, command, dir, &dir.join(name), LIMIT)
            .await
            .unwrap()
    }

    #[test]
    fn the_slot_is_one_file_per_user_unless_configured() {
        let default = slot_lock(&IntegrationConfig::default());
        let uid = rustix::process::getuid().as_raw();
        assert_eq!(
            default,
            PathBuf::from(format!("/tmp/archon-full-check-{uid}.lock"))
        );
        let config = IntegrationConfig {
            slot_lock: Some("/run/archon.lock".into()),
            ..IntegrationConfig::default()
        };
        assert_eq!(slot_lock(&config), PathBuf::from("/run/archon.lock"));
    }

    #[tokio::test]
    async fn a_run_passes_or_fails_with_the_end_of_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let passed = full("echo built", &dir, "pass.log").await;
        assert_eq!(passed.excerpt, "built");
        assert!(passed.passed);
        let failed = full("seq 1 200; echo broken >&2; exit 3", &dir, "fail.log").await;
        assert!(!failed.passed);
        assert!(
            failed
                .excerpt
                .contains("200\nbroken\n[the full check failed"),
            "{}",
            failed.excerpt
        );
        assert!(!failed.excerpt.contains("\n140\n"), "only the last lines");
    }

    #[tokio::test]
    async fn a_run_past_its_limit_is_ended_and_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let slot = Slot::acquire(&dir.join("slot.lock")).await.unwrap();
        let log = dir.join("slow.log");
        let started = std::time::Instant::now();
        let ran = run(
            &slot,
            "echo started; sleep 30",
            &dir,
            &log,
            Duration::from_millis(300),
        );
        let ran = ran.await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "ended at its limit"
        );
        assert!(!ran.passed);
        assert!(
            ran.excerpt
                .starts_with("started\n[the full check was ended"),
            "{}",
            ran.excerpt
        );
    }

    /// Two daemons' worth of runs at once, each holding the slot through
    /// its own run: no start ever follows another start.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn full_runs_never_overlap_on_one_host() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let trace = dir.join("trace");
        let command = format!(
            "echo start >> {0}; sleep 0.2; echo end >> {0}",
            trace.display()
        );
        let runs = (0..3).map(|n| {
            let (command, dir) = (command.clone(), dir.clone());
            tokio::spawn(async move { full(&command, &dir, &format!("{n}.log")).await })
        });
        // A separate lock on the host file stands for another daemon.
        let other = {
            let path = dir.join("slot.lock");
            let trace = trace.clone();
            tokio::task::spawn_blocking(move || {
                let _held = lock_file(&path).unwrap();
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&trace)
                    .unwrap();
                use std::io::Write;
                writeln!(file, "start").unwrap();
                std::thread::sleep(Duration::from_millis(200));
                writeln!(file, "end").unwrap();
            })
        };
        let all = async {
            for run in runs.collect::<Vec<_>>() {
                assert!(run.await.unwrap().passed);
            }
            other.await.unwrap();
        };
        tokio::time::timeout(LIMIT, all)
            .await
            .expect("the runs finish");
        let trace = std::fs::read_to_string(&trace).unwrap();
        let lines: Vec<&str> = trace.lines().collect();
        assert_eq!(lines.len(), 8, "{trace}");
        for pair in lines.chunks(2) {
            assert_eq!(pair, ["start", "end"], "overlapping runs: {trace}");
        }
    }
}
