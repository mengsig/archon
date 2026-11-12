//! Conservative Linux flock discovery and pidfd-guarded termination.

use std::io;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct Holder {
    pub(crate) pid: Option<u32>,
    pub(crate) command: Option<String>,
    pub(crate) age: Option<Duration>,
    pub(crate) mode: LockMode,
    #[cfg(target_os = "linux")]
    identity: Option<linux::Identity>,
}

impl Holder {
    pub(crate) fn may_stop(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.mode == LockMode::Shared
                && self
                    .identity
                    .as_ref()
                    .is_some_and(|id| !linux::protected(id))
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

/// Snapshot kernel flock entries for the file; an unknown identity is never signalable.
#[cfg(target_os = "linux")]
pub(crate) fn discover(path: &Path) -> io::Result<Vec<Holder>> {
    linux::discover(path)
}

/// Signal only a still-identical, verified shared holder, using a pidfd.
#[cfg(target_os = "linux")]
pub(crate) fn terminate(path: &Path, holder: &Holder) -> io::Result<()> {
    linux::terminate(path, holder)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn discover(_path: &Path) -> io::Result<Vec<Holder>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "flock holder discovery requires Linux /proc",
    ))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn terminate(_path: &Path, _holder: &Holder) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "guarded termination requires Linux pidfds",
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Holder, LockMode};
    use std::fs;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct Identity {
        start: u64,
        exe: PathBuf,
        args: Vec<u8>,
        dev: u64,
        ino: u64,
    }

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }

    fn refused(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    fn start_ticks(pid: u32) -> io::Result<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let end = stat
            .rfind(')')
            .ok_or_else(|| invalid("invalid process stat"))?;
        stat[end + 1..]
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| invalid("missing process start time"))?
            .parse()
            .map_err(|_| invalid("invalid process start time"))
    }

    fn identity(pid: u32, dev: u64, ino: u64) -> io::Result<Identity> {
        let start = start_ticks(pid)?;
        let exe = fs::read_link(format!("/proc/{pid}/exe"))?;
        let args = fs::read(format!("/proc/{pid}/cmdline"))?;
        if args.is_empty() || !args.ends_with(&[0]) || start != start_ticks(pid)? {
            return Err(invalid("process identity changed or is unavailable"));
        }
        Ok(Identity {
            start,
            exe,
            args,
            dev,
            ino,
        })
    }

    fn escaped_command(args: &[u8]) -> String {
        args.split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| {
                format!(
                    "'{}'",
                    arg.iter()
                        .flat_map(|b| std::ascii::escape_default(*b))
                        .map(char::from)
                        .collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn age(start: u64) -> Option<Duration> {
        let hz = rustix::param::clock_ticks_per_second() as f64;
        let uptime: f64 = fs::read_to_string("/proc/uptime")
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let seconds = uptime - start as f64 / hz;
        if !hz.is_finite() || hz <= 0.0 || !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        Duration::try_from_secs_f64(seconds).ok()
    }

    fn entries(dev: u64, ino: u64) -> io::Result<Vec<(Option<u32>, LockMode)>> {
        let locks = fs::read_to_string("/proc/locks")?;
        let mut result = Vec::new();
        for line in locks.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.get(1) != Some(&"FLOCK") {
                continue;
            }
            if fields.len() < 8 {
                return Err(invalid("malformed /proc/locks flock entry"));
            }
            let mut key = fields[5].split(':');
            let major = key.next().and_then(|s| u32::from_str_radix(s, 16).ok());
            let minor = key.next().and_then(|s| u32::from_str_radix(s, 16).ok());
            let inode = key.next().and_then(|s| s.parse::<u64>().ok());
            if key.next().is_some() || major.is_none() || minor.is_none() || inode.is_none() {
                return Err(invalid("invalid /proc/locks device or inode"));
            }
            if inode != Some(ino) {
                continue;
            }
            // Overlay filesystems can report a backing device in /proc/locks.
            // A mismatched device remains ambiguous until the holder's fd is checked.
            let _same_device =
                major == Some(rustix::fs::major(dev)) && minor == Some(rustix::fs::minor(dev));
            let mode = match fields[3] {
                "READ" => LockMode::Shared,
                "WRITE" => LockMode::Exclusive,
                _ => LockMode::Unknown,
            };
            let pid = fields[4].parse::<u32>().ok().filter(|pid| *pid > 1);
            result.push((pid, mode));
        }
        Ok(result)
    }

    pub(super) fn discover(path: &Path) -> io::Result<Vec<Holder>> {
        let metadata = fs::metadata(path)?;
        let (dev, ino) = (metadata.dev(), metadata.ino());
        entries(dev, ino)?
            .into_iter()
            .map(|(pid, mode)| {
                let id = pid
                    .filter(|pid| has_locked_fd(*pid, dev, ino).unwrap_or(false))
                    .and_then(|pid| identity(pid, dev, ino).ok());
                Ok(Holder {
                    pid: id.as_ref().and(pid),
                    command: id.as_ref().map(|i| escaped_command(&i.args)),
                    age: id.as_ref().and_then(|i| age(i.start)),
                    mode,
                    identity: id,
                })
            })
            .collect()
    }

    pub(super) fn protected(id: &Identity) -> bool {
        let name = id.exe.file_name().and_then(|s| s.to_str());
        let args: Vec<_> = id
            .args
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .collect();
        let arg_name = args
            .first()
            .and_then(|a| std::str::from_utf8(a).ok())
            .and_then(|a| Path::new(a).file_name())
            .and_then(|s| s.to_str());
        name.is_none()
            || arg_name.is_none()
            || name == Some("archond")
            || arg_name == Some("archond")
            || ((name == Some("archon") || arg_name == Some("archon"))
                && !args.get(1).is_some_and(|arg| *arg == b"gc"))
            || args
                .iter()
                .any(|arg| *arg == b"archond" || *arg == b"daemon")
    }

    fn has_locked_fd(pid: u32, dev: u64, ino: u64) -> io::Result<bool> {
        for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
            let entry = entry?;
            let meta = match fs::metadata(entry.path()) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if (meta.dev(), meta.ino()) != (dev, ino) {
                continue;
            }
            let info = fs::read_to_string(format!(
                "/proc/{pid}/fdinfo/{}",
                entry.file_name().to_string_lossy()
            ))?;
            if info.lines().any(|line| {
                line.starts_with("lock:") && line.split_whitespace().any(|part| part == "FLOCK")
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn terminate(path: &Path, holder: &Holder) -> io::Result<()> {
        if holder.mode != LockMode::Shared {
            return Err(refused("only shared holders can be stopped"));
        }
        let pid = holder.pid.ok_or_else(|| refused("holder PID is unknown"))?;
        let old = holder
            .identity
            .as_ref()
            .ok_or_else(|| refused("holder identity is unknown"))?;
        if protected(old) {
            return Err(refused("refusing to stop an Archon process"));
        }
        let raw = i32::try_from(pid).map_err(|_| refused("invalid holder PID"))?;
        let pid =
            rustix::process::Pid::from_raw(raw).ok_or_else(|| refused("invalid holder PID"))?;
        let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .map_err(io::Error::from)?;
        let metadata = fs::metadata(path)?;
        if (metadata.dev(), metadata.ino()) != (old.dev, old.ino) {
            return Err(refused("lock file changed"));
        }
        let current = identity(raw as u32, old.dev, old.ino)?;
        if current != *old || protected(&current) {
            return Err(refused("holder identity changed or is protected"));
        }
        let locks = entries(old.dev, old.ino)?;
        if locks.as_slice() != [(Some(raw as u32), LockMode::Shared)] {
            return Err(refused("lock ownership changed or is ambiguous"));
        }
        if !has_locked_fd(raw as u32, old.dev, old.ino)? {
            return Err(refused("holder no longer has a locked file descriptor"));
        }
        if identity(raw as u32, old.dev, old.ino)? != *old {
            return Err(refused("holder identity changed"));
        }
        rustix::process::pidfd_send_signal(&fd, rustix::process::Signal::TERM)
            .map_err(io::Error::from)
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn protects_archon_daemons_even_when_renamed() {
            let mut id = Identity {
                start: 1,
                exe: PathBuf::from("/tmp/custom-name"),
                args: b"custom-name\0daemon\0run\0".to_vec(),
                dev: 1,
                ino: 1,
            };
            assert!(protected(&id));
            id.args = b"custom-name\0gc\0".to_vec();
            assert!(!protected(&id));
            id.exe = PathBuf::from("/usr/bin/archond");
            assert!(protected(&id));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Instant;

    struct TestChild(Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                self.0.kill().unwrap();
                self.0.wait().unwrap();
            }
        }
    }

    #[test]
    fn lock_child() {
        let Ok(path) = std::env::var("ARCHON_TEST_LOCK_CHILD") else {
            return;
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.try_lock_shared().unwrap();
        fs::write(format!("{path}.ready"), b"ready").unwrap();
        thread::sleep(Duration::from_secs(10));
    }

    fn start_child(path: &Path) -> TestChild {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "store_holders::tests::lock_child", "--nocapture"])
            .env("ARCHON_TEST_LOCK_CHILD", path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = TestChild(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.with_extension("lock.ready").exists() {
            assert!(Instant::now() < deadline, "child failed to acquire lock");
            assert!(child.0.try_wait().unwrap().is_none(), "child exited early");
            thread::sleep(Duration::from_millis(10));
        }
        child
    }

    #[test]
    fn discovers_child_and_refuses_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.lock");
        File::create(&path).unwrap();
        let _child = start_child(&path);
        let holders = discover(&path).unwrap();
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].pid, Some(_child.0.id()));
        assert_eq!(holders[0].mode, LockMode::Shared);
        assert!(holders[0].command.as_ref().unwrap().contains("lock_child"));
        assert!(holders[0].age.is_some());
        fs::rename(&path, dir.path().join("old.lock")).unwrap();
        File::create(&path).unwrap();
        assert!(terminate(&path, &holders[0]).is_err());
        assert!(discover(&path).unwrap().is_empty());
    }

    #[test]
    fn stale_contents_are_ignored_and_signal_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.lock");
        fs::write(&path, b"{\"pid\": 1}").unwrap();
        assert!(discover(&path).unwrap().is_empty());
        let mut child = start_child(&path);
        let holder = discover(&path).unwrap().pop().unwrap();
        terminate(&path, &holder).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(discover(&path).unwrap().is_empty());
        assert!(terminate(&path, &holder).is_err());
    }

    #[test]
    fn refuses_unknown_and_non_shared_holders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.lock");
        let file = File::create(&path).unwrap();
        file.try_lock().unwrap();
        let holder = discover(&path).unwrap().pop().unwrap();
        assert_eq!(holder.mode, LockMode::Exclusive);
        assert!(terminate(&path, &holder).is_err());
        let mut unknown = holder;
        unknown.mode = LockMode::Shared;
        unknown.identity = None;
        assert!(terminate(&path, &unknown).is_err());
    }
}
