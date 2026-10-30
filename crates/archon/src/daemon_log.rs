//! Detached daemon tracing, with one owner of the active file and its archives.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use archon_core::config::StorageConfig;
use tokio::sync::Notify;
use tracing_subscriber::fmt::MakeWriter;

const DAY: u64 = 24 * 60 * 60;
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Logger {
    state: Arc<Mutex<State>>,
    failed: Arc<Notify>,
}

#[derive(Debug)]
struct State {
    log: RotatingLog,
    failure: Option<String>,
}

impl Logger {
    pub fn install_panic_hook(&self) {
        let logger = self.clone();
        std::panic::set_hook(Box::new(move |info| {
            let message = format!("archon panicked: {info}\n");
            // A panic inside logging invokes the hook before its mutex guard
            // unwinds. Waiting for that guard would deadlock the process.
            let result = match logger.state.try_lock() {
                Ok(mut state) => state.log.write_bytes(message.as_bytes(), SystemTime::now()),
                Err(error) => Err(anyhow::anyhow!(
                    "daemon log unavailable during panic: {error}"
                )),
            };
            if let Err(error) = result {
                // A closed startup pipe must not trigger a second panic.
                // Returning the write result avoids eprintln!'s panic on EPIPE.
                let diagnostic = format!("archon: recording panic failed: {error}; {info}\n");
                let _closed_startup_pipe = std::io::stderr().write_all(diagnostic.as_bytes());
            }
        }));
    }

    pub fn open(path: &Path, config: &StorageConfig) -> anyhow::Result<Self> {
        let mut log = RotatingLog::open(path, config)?;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        log.write_bytes(
            format!("{} at {now} ---\n", archon_core::paths::STARTUP_LOG_MARK).as_bytes(),
            SystemTime::now(),
        )?;
        Ok(Self {
            state: Arc::new(Mutex::new(State { log, failure: None })),
            failed: Arc::new(Notify::new()),
        })
    }

    pub fn write_message(&self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("daemon log lock poisoned"))?;
        if let Some(failure) = &state.failure {
            return Err(io::Error::other(failure.clone()));
        }
        match state.log.write_bytes(bytes, SystemTime::now()) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!("writing daemon log: {error:#}");
                state.failure = Some(message.clone());
                self.failed.notify_one();
                Err(io::Error::other(message))
            }
        }
    }

    /// Expire archives even while idle, and surface formatter write errors to
    /// the daemon entry point instead of trusting tracing's stderr fallback.
    pub async fn monitor(&self) -> anyhow::Result<std::convert::Infallible> {
        let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {},
                _ = self.failed.notified() => {},
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("daemon log lock poisoned"))?;
            if let Some(failure) = &state.failure {
                anyhow::bail!("{failure}");
            }
            state.log.maintain(SystemTime::now())?;
        }
    }
}

#[derive(Debug)]
pub struct LogWriter<'a>(&'a Logger);

impl<'a> MakeWriter<'a> for Logger {
    type Writer = LogWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self)
    }
}

impl Write for LogWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write_message(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Files are unbuffered; a successful write already reached the OS.
        Ok(())
    }
}

#[derive(Debug)]
struct RotatingLog {
    path: PathBuf,
    file: File,
    _lock: File,
    size: u64,
    max_bytes: u64,
    max_files: u32,
    retention: Option<Duration>,
    started: SystemTime,
    last_cleanup: Option<SystemTime>,
}

impl RotatingLog {
    fn open(path: &Path, config: &StorageConfig) -> anyhow::Result<Self> {
        let lock_path = path.with_extension("log.lock");
        let lock = private_file(&lock_path)?;
        lock.try_lock().with_context(|| {
            format!(
                "locking {} (another daemon may own the log)",
                lock_path.display()
            )
        })?;
        let file = private_file(path)?;
        let meta = file.metadata()?;
        let mut log = Self {
            path: path.to_owned(),
            file,
            _lock: lock,
            size: meta.len(),
            max_bytes: config
                .log_max_mb
                .checked_mul(1024 * 1024)
                .context("daemon log size overflows bytes")?,
            max_files: config.log_max_files,
            retention: (config.retention_days != 0)
                .then(|| Duration::from_secs(u64::from(config.retention_days) * DAY)),
            started: meta.modified()?,
            last_cleanup: None,
        };
        log.maintain(SystemTime::now())?;
        Ok(log)
    }

    fn write_bytes(&mut self, mut bytes: &[u8], now: SystemTime) -> anyhow::Result<()> {
        self.maintain(now)?;
        while !bytes.is_empty() {
            if self.max_bytes != 0 && self.size >= self.max_bytes {
                self.rotate(now)?;
            }
            // Split an unusually large event too: each newly written file is
            // bounded, even when a single formatted event exceeds the cap.
            let count = if self.max_bytes == 0 {
                bytes.len()
            } else {
                bytes
                    .len()
                    .min(usize::try_from(self.max_bytes - self.size).unwrap_or(usize::MAX))
            };
            self.file
                .write_all(&bytes[..count])
                .with_context(|| format!("writing {}", self.path.display()))?;
            self.size += count as u64;
            bytes = &bytes[count..];
        }
        Ok(())
    }

    fn maintain(&mut self, now: SystemTime) -> anyhow::Result<()> {
        if self.size != 0
            && (self.max_bytes != 0 && self.size >= self.max_bytes
                || self.expired(self.started, now))
        {
            self.rotate(now)?;
        }
        if self.last_cleanup.is_some_and(|previous| {
            now.duration_since(previous)
                .is_ok_and(|elapsed| elapsed < MAINTENANCE_INTERVAL)
        }) {
            return Ok(());
        }
        for (index, path, modified) in self.archives()? {
            if self.max_files != 0 && index > u64::from(self.max_files)
                || self.expired(modified, now)
            {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
        self.last_cleanup = Some(now);
        Ok(())
    }

    fn expired(&self, time: SystemTime, now: SystemTime) -> bool {
        self.retention
            .is_some_and(|limit| now.duration_since(time).is_ok_and(|age| age >= limit))
    }

    fn archive_path(&self, index: u64) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self, now: SystemTime) -> anyhow::Result<()> {
        let mut archives = self.archives()?;
        archives.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        for (index, path, modified) in archives {
            if self.max_files != 0 && index >= u64::from(self.max_files)
                || self.expired(modified, now)
            {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            } else {
                let next = index
                    .checked_add(1)
                    .context("daemon log archive index overflow")?;
                std::fs::rename(&path, self.archive_path(next))
                    .with_context(|| format!("rotating {}", path.display()))?;
            }
        }
        std::fs::rename(&self.path, self.archive_path(1))
            .with_context(|| format!("rotating {}", self.path.display()))?;
        self.file = private_file(&self.path)?;
        self.size = 0;
        self.started = now;
        Ok(())
    }

    fn archives(&self) -> anyhow::Result<Vec<(u64, PathBuf, SystemTime)>> {
        let parent = self
            .path
            .parent()
            .context("daemon log has no parent directory")?;
        let prefix = format!(
            "{}.",
            self.path
                .file_name()
                .context("daemon log has no file name")?
                .to_string_lossy()
        );
        let mut archives = Vec::new();
        for entry in
            std::fs::read_dir(parent).with_context(|| format!("listing {}", parent.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
                continue;
            };
            let Ok(index) = suffix.parse::<u64>() else {
                continue;
            };
            if index == 0 || index.to_string() != suffix {
                continue;
            }
            let meta = std::fs::symlink_metadata(entry.path())?;
            anyhow::ensure!(
                meta.is_file(),
                "daemon log archive {} is not a regular file",
                entry.path().display()
            );
            archives.push((index, entry.path(), meta.modified()?));
        }
        Ok(archives)
    }
}

fn private_file(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as _)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> StorageConfig {
        StorageConfig {
            retention_days: 0,
            log_max_mb: 0,
            log_max_files: 0,
        }
    }

    fn read(path: impl AsRef<Path>) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn age_file(path: &Path, now: SystemTime, days: u64) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(now - Duration::from_secs(days * DAY)),
            )
            .unwrap();
    }

    #[test]
    fn startup_appends_one_marker_and_keeps_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        std::fs::write(&path, "earlier session\n").unwrap();
        let logger = Logger::open(&path, &config()).unwrap();
        logger.write_message(b"new output\n").unwrap();
        let text = read(&path);
        assert!(text.starts_with("earlier session\n--- archond starting at "));
        assert_eq!(
            text.matches(archon_core::paths::STARTUP_LOG_MARK).count(),
            1
        );
        assert!(text.ends_with("new output\n"));
    }

    #[test]
    fn size_and_archive_count_stay_bounded_during_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let mut settings = config();
        settings.log_max_files = 2;
        let mut log = RotatingLog::open(&path, &settings).unwrap();
        log.max_bytes = 4;
        let now = SystemTime::now();
        log.write_bytes(b"aaaabbbbccccdddd", now).unwrap();
        assert_eq!(read(&path), "dddd");
        assert_eq!(read(log.archive_path(1)), "cccc");
        assert_eq!(read(log.archive_path(2)), "bbbb");
        assert!(!log.archive_path(3).exists());
        log.write_bytes(b"e", now).unwrap();
        assert_eq!(read(&path), "e");
        assert_eq!(read(log.archive_path(1)), "dddd");
    }

    #[test]
    fn zero_count_keeps_archives_while_size_rotation_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let mut log = RotatingLog::open(&path, &config()).unwrap();
        log.max_bytes = 2;
        log.write_bytes(b"aabbccddee", SystemTime::now()).unwrap();
        assert_eq!(log.archives().unwrap().len(), 4);
        assert_eq!(read(log.archive_path(4)), "aa");
        assert_eq!(read(&path), "ee");
    }

    #[test]
    fn zero_size_disables_rotation_even_with_an_archive_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let mut settings = config();
        settings.log_max_files = 1;
        let mut log = RotatingLog::open(&path, &settings).unwrap();
        let data = vec![b'x'; 2 * 1024 * 1024];
        log.write_bytes(&data, SystemTime::now()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), data.len() as u64);
        assert!(log.archives().unwrap().is_empty());
    }

    #[test]
    fn old_archives_expire_and_idle_active_file_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let old = dir.path().join("archond.log.1");
        let unrelated = dir.path().join("archond.log.backup");
        let now = SystemTime::now();
        std::fs::write(&path, "old active").unwrap();
        std::fs::write(&old, "expired archive").unwrap();
        std::fs::write(&unrelated, "keep").unwrap();
        age_file(&old, now, 3);
        age_file(&path, now, 3);
        let mut settings = config();
        settings.retention_days = 2;
        let log = RotatingLog::open(&path, &settings).unwrap();
        assert_eq!(read(&path), "");
        assert!(log.archives().unwrap().is_empty());
        assert_eq!(read(unrelated), "keep");
    }

    #[test]
    fn maintenance_expires_archives_without_more_log_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let mut settings = config();
        settings.retention_days = 1;
        let mut log = RotatingLog::open(&path, &settings).unwrap();
        let archive = log.archive_path(1);
        std::fs::write(&archive, "archive").unwrap();
        log.maintain(SystemTime::now() + Duration::from_secs(2 * DAY))
            .unwrap();
        assert!(!archive.exists());
        assert!(path.is_file());
    }

    #[test]
    fn zero_age_preserves_old_active_and_archived_logs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let archive = dir.path().join("archond.log.1");
        let now = SystemTime::now();
        std::fs::write(&path, "old active").unwrap();
        std::fs::write(&archive, "old archive").unwrap();
        age_file(&path, now, 30);
        age_file(&archive, now, 30);
        let _log = RotatingLog::open(&path, &config()).unwrap();
        assert_eq!(read(&path), "old active");
        assert_eq!(read(&archive), "old archive");
    }

    #[test]
    fn startup_prunes_archives_above_new_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        for index in [1, 2, 19] {
            std::fs::write(dir.path().join(format!("archond.log.{index}")), "archive").unwrap();
        }
        let mut settings = config();
        settings.log_max_files = 1;
        let log = RotatingLog::open(&path, &settings).unwrap();
        assert_eq!(log.archives().unwrap().len(), 1);
        assert_eq!(read(log.archive_path(1)), "archive");
    }

    #[test]
    fn second_owner_cannot_mutate_the_live_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let _owner = Logger::open(&path, &config()).unwrap();
        let before = read(&path);
        assert!(Logger::open(&path, &config()).is_err());
        assert_eq!(read(&path), before);
    }

    #[test]
    fn symlinked_logs_and_archives_are_refused_without_touching_targets() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let path = dir.path().join("archond.log");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(Logger::open(&path, &config()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("archond.log.1")).unwrap();
        assert!(Logger::open(&path, &config()).is_err());
        assert_eq!(read(outside), "keep");
    }

    #[test]
    fn special_log_files_are_refused_without_waiting_for_a_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(Logger::open(&path, &config()).is_err()).unwrap());
        assert!(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("special files must not block")
        );
    }

    #[test]
    fn concurrent_formatters_keep_complete_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let logger = Logger::open(&path, &config()).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let logger = &logger;
                scope.spawn(move || {
                    for _ in 0..100 {
                        logger.write_message(b"one complete event\n").unwrap();
                    }
                });
            }
        });
        let text = read(path);
        assert_eq!(text.lines().skip(1).count(), 400);
        assert!(
            text.lines()
                .skip(1)
                .all(|line| line == "one complete event")
        );
    }

    #[tokio::test]
    async fn rotation_failure_reaches_the_daemon_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let logger = Logger::open(&path, &config()).unwrap();
        logger.state.lock().unwrap().log.max_bytes = 1;
        // Force rename to fail, rather than relying on permissions (which a
        // root test runner could bypass).
        std::fs::remove_file(&path).unwrap();
        let error = logger.write_message(b"output").unwrap_err();
        assert!(error.to_string().contains("rotating"));
        let error = logger.monitor().await.unwrap_err();
        assert!(error.to_string().contains("writing daemon log"));
    }

    #[test]
    fn detached_panics_are_recorded_in_the_log() {
        const CHILD_PATH: &str = "ARCHON_LOG_PANIC_TEST_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let logger = Logger::open(Path::new(&path), &config()).unwrap();
            logger.install_panic_hook();
            panic!("test detached panic");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archond.log");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon_log::tests::detached_panics_are_recorded_in_the_log",
            ])
            .env(CHILD_PATH, &path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let text = read(&path);
        assert!(text.contains("archon panicked:"), "{text}");
        assert!(text.contains("test detached panic"), "{text}");
    }
}
