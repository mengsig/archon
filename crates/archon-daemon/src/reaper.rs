//! Reaping the processes a stage leaves under its worktree (Linux only).
//!
//! A tool process that double-forks or runs under `nohup` escapes the
//! engine's process group, so a stage's end, `archon gc` and item cleanup
//! look for processes whose working directory or executable lies under the
//! worktree and end them: SIGTERM, a grace period, then SIGKILL.
//!
//! Selection is conservative: same user, started for an agent (Aether's
//! engine tag in its environment), never pid 1, never this process,
//! Archon's or Aether's daemons, their ancestors or their descendants, nor
//! at a stage end the services the run declared ([`services`]). A
//! live engine and its tools descend from Aether, which reaps them itself;
//! what escaped was reparented away. An operator's shell or editor in a
//! worktree carries no tag and is left alone. Off Linux nothing is scanned
//! or killed.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;

pub mod services;

/// How long SIGTERM'd processes get to exit before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How long SIGKILL'd processes get to vanish before they count as survivors.
const KILL_GRACE: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(50);

/// How many times the tree is scanned and its processes ended before what
/// still appears counts as surviving.
const ROUNDS: usize = 3;

/// Executable names of the daemons, spared with their ancestors and
/// descendants; Aether also runs from versioned copies (`aether-1.2.4-…`).
const DAEMON_NAMES: [&str; 4] = ["archon", "archond", "aether", "aetherd"];

/// What Aether exports to each engine, and so to everything it starts
/// (`aether_engine::orphans`, since Aether 1.2.3); an older Aether's tools
/// carry none, so nothing is reaped under it.
const ENGINE_TAG: &[u8] = b"AETHER_ENGINE_RUN=";

/// Bounds the ancestor walk; a deeper chain is spared as untraceable.
const MAX_DEPTH: usize = 256;

/// One process found under a worktree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Found {
    pub pid: i32,
    pub name: String,
}

/// What a reap did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Processes found running, by the first scan or a later one.
    pub found: Vec<Found>,
    /// Those that needed SIGKILL after the grace period.
    pub killed: Vec<Found>,
    /// Those still running after SIGKILL.
    pub survivors: Vec<Found>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.found.is_empty()
    }

    /// One sentence for the feed and the stage record.
    pub fn describe(&self, worktree: &Path) -> String {
        let list = |procs: &[Found]| {
            procs
                .iter()
                .map(|p| format!("{} ({})", p.pid, p.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut text = format!(
            "Ended {} process(es) left under {}: {}.",
            self.found.len(),
            worktree.display(),
            list(&self.found)
        );
        if !self.killed.is_empty() {
            text.push_str(&format!(
                " Killed after the grace period: {}.",
                list(&self.killed)
            ));
        }
        if !self.survivors.is_empty() {
            text.push_str(&format!(
                " Still running after SIGKILL: {}.",
                list(&self.survivors)
            ));
        }
        text
    }
}

/// How the reaper holds and signals a process: by pidfd on Linux, so a pid
/// reused by another process is never signalled; faked in tests.
trait Signaller {
    type Handle;
    /// A handle on whatever process holds `pid` now; `None` once it is gone.
    fn open(&self, pid: i32) -> Result<Option<Self::Handle>, ArchonError>;
    /// Only a confirmed exit is true: a process that cannot be observed is
    /// kept, so it is killed and reported rather than taken for gone.
    fn exited(&self, handle: &Self::Handle) -> bool;
    /// SIGTERM, or SIGKILL when `kill`; a process already gone is ignored.
    fn signal(&self, handle: &Self::Handle, kill: bool);
}

/// One selected process and the handle it is signalled through.
struct Held<H> {
    found: Found,
    handle: H,
}

/// Where to look and whom to spare.
struct Scan<'a> {
    proc_root: &'a Path,
    worktree: PathBuf,
    /// Declared services: these PIDs, their process groups and descendants.
    kept: &'a BTreeSet<i32>,
    uid: u32,
    me: i32,
}

/// End every process of the current user whose cwd or executable lies under
/// `worktree`, a tree below the worktree `root`, as the module docs describe,
/// but the services `kept`.
#[cfg(target_os = "linux")]
pub fn reap(root: &Path, worktree: &Path, kept: &BTreeSet<i32>) -> Result<Report, ArchonError> {
    let uid = rustix::process::getuid().as_raw();
    let proc_root = Path::new("/proc");
    let graces = (TERM_GRACE, KILL_GRACE);
    reap_with(proc_root, root, worktree, kept, uid, graces, &PidFds)
}

/// Nothing is scanned or killed off Linux.
#[cfg(not(target_os = "linux"))]
pub fn reap(_root: &Path, _worktree: &Path, _kept: &BTreeSet<i32>) -> Result<Report, ArchonError> {
    Ok(Report::default())
}

/// [`reap`] before `worktree` is removed: what was running is ended and
/// logged, and a process that survives SIGKILL refuses the removal.
pub fn clear_for_removal(root: &Path, worktree: &Path) -> Result<(), ArchonError> {
    let report = reap(root, worktree, &BTreeSet::new())?;
    if report.is_empty() {
        return Ok(());
    }
    tracing::warn!("{}", report.describe(worktree));
    if report.survivors.is_empty() {
        return Ok(());
    }
    Err(ArchonError::Invalid(format!(
        "{} is kept: processes still run in it after SIGKILL ({})",
        worktree.display(),
        report
            .survivors
            .iter()
            .map(|p| format!("{} {}", p.pid, p.name))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// `worktree` as the kernel names it, refused when it is not strictly below
/// `root` or a symbolic link below `root` leads it elsewhere.
fn resolve_within(root: &Path, worktree: &Path) -> Result<PathBuf, ArchonError> {
    let refuse = |why: String| {
        Err(ArchonError::Invalid(format!(
            "{} {why}, so nothing is reaped",
            worktree.display()
        )))
    };
    let real_root = root
        .canonicalize()
        .map_err(|e| ArchonError::io(format!("resolving {}", root.display()), e))?;
    let Ok(below) = worktree
        .strip_prefix(root)
        .or_else(|_| worktree.strip_prefix(&real_root))
    else {
        return refuse(format!("is outside the worktree root {}", root.display()));
    };
    let plain = below
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if below.as_os_str().is_empty() || !plain {
        return refuse(format!("is not a tree below {}", root.display()));
    }
    let expected = real_root.join(below);
    match worktree.canonicalize() {
        Ok(real) if real == expected => Ok(real),
        Ok(real) => refuse(format!("leads to {} by a symbolic link", real.display())),
        // Nothing redirects a path that does not exist.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(expected),
        Err(e) => Err(ArchonError::io(
            format!("resolving {}", worktree.display()),
            e,
        )),
    }
}

#[cfg(target_os = "linux")]
struct PidFds;

#[cfg(target_os = "linux")]
impl Signaller for PidFds {
    type Handle = std::os::fd::OwnedFd;

    fn open(&self, raw: i32) -> Result<Option<Self::Handle>, ArchonError> {
        assert!(raw > 1, "pid 1 is never opened");
        let pid = rustix::process::Pid::from_raw(raw).expect("a positive pid");
        match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
            Ok(fd) => Ok(Some(fd)),
            Err(rustix::io::Errno::SRCH) => Ok(None),
            Err(e) => Err(ArchonError::io(
                format!("opening a pidfd for process {raw}"),
                e.into(),
            )),
        }
    }

    fn exited(&self, handle: &Self::Handle) -> bool {
        // A pidfd polls readable once its process has exited.
        let mut fds = [rustix::event::PollFd::new(
            handle,
            rustix::event::PollFlags::IN,
        )];
        let now = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            match rustix::event::poll(&mut fds, Some(&now)) {
                Ok(ready) => return ready > 0,
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => {
                    tracing::warn!(%error, "cannot poll a process handle; it counts as running");
                    return false;
                }
            }
        }
    }

    fn signal(&self, handle: &Self::Handle, kill: bool) {
        let signal = if kill {
            rustix::process::Signal::KILL
        } else {
            rustix::process::Signal::TERM
        };
        match rustix::process::pidfd_send_signal(handle, signal) {
            // ESRCH: it exited since the scan, which is what was wanted.
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            // It stays running, so it is killed or reported as a survivor.
            Err(error) => tracing::warn!(%error, ?signal, "cannot signal a process"),
        }
    }
}

fn reap_with<S: Signaller>(
    proc_root: &Path,
    root: &Path,
    worktree: &Path,
    kept: &BTreeSet<i32>,
    uid: u32,
    graces: (Duration, Duration),
    signaller: &S,
) -> Result<Report, ArchonError> {
    assert!(worktree.is_absolute(), "worktree paths are absolute");
    assert!(
        worktree.components().count() > 2,
        "refusing to reap under {}",
        worktree.display()
    );
    assert!(kept.iter().all(|&pid| pid > 1), "pid 1 is no service");
    let scan = Scan {
        proc_root,
        worktree: resolve_within(root, worktree)?,
        kept,
        uid,
        me: std::process::id() as i32,
    };
    let mut report = Report::default();
    // Rescanned: a process may fork a child as it ends, or a supervisor
    // restart one, and the tree is clear only once a scan finds nothing.
    for _ in 0..ROUNDS {
        let found = scan.find(signaller)?;
        if found.is_empty() {
            return Ok(report);
        }
        end_round(signaller, &found, graces, &mut report);
        if !report.survivors.is_empty() {
            return Ok(report);
        }
    }
    let respawned: Vec<Found> = scan.find(signaller)?.into_iter().map(|p| p.found).collect();
    report.found.extend(respawned.iter().cloned());
    report.survivors = respawned;
    assert!(
        report.killed.len() <= report.found.len(),
        "kills are a subset"
    );
    assert!(
        report.survivors.len() <= report.found.len(),
        "survivors were found"
    );
    Ok(report)
}

/// SIGTERM `found`, SIGKILL what outlasts the term grace, and add to `report`
/// what was found, killed and survived the kill grace.
fn end_round<S: Signaller>(
    signaller: &S,
    found: &[Held<S::Handle>],
    (term_grace, kill_grace): (Duration, Duration),
    report: &mut Report,
) {
    assert!(!found.is_empty(), "a round ends something");
    assert!(report.survivors.is_empty(), "no round follows a survivor");
    for p in found {
        signaller.signal(&p.handle, false);
    }
    let remaining = wait_exit(signaller, found.iter().collect(), term_grace);
    for p in &remaining {
        signaller.signal(&p.handle, true);
    }
    let survivors = wait_exit(signaller, remaining.clone(), kill_grace);
    report.found.extend(found.iter().map(|p| p.found.clone()));
    report
        .killed
        .extend(remaining.iter().map(|p| p.found.clone()));
    report
        .survivors
        .extend(survivors.iter().map(|p| p.found.clone()));
    assert!(
        report.survivors.len() <= remaining.len(),
        "survivors were killed"
    );
}

/// The members of `watched` that have not exited after waiting up to `grace`.
fn wait_exit<'h, S: Signaller>(
    signaller: &S,
    watched: Vec<&'h Held<S::Handle>>,
    grace: Duration,
) -> Vec<&'h Held<S::Handle>> {
    let deadline = Instant::now() + grace;
    let mut left = watched;
    loop {
        left.retain(|p| !signaller.exited(&p.handle));
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(POLL);
    }
}

impl Scan<'_> {
    /// The processes under the worktree now, each held by a handle opened
    /// before it was checked, so the checks are of the process it signals.
    fn find<S: Signaller>(&self, signaller: &S) -> Result<Vec<Held<S::Handle>>, ArchonError> {
        let spared = self.ancestors_of_roots()?;
        let mut found = Vec::new();
        for pid in pids(self.proc_root)? {
            if pid <= 1 || spared.contains(&pid) || self.matches(pid).is_none() {
                continue;
            }
            let Some(handle) = signaller.open(pid)? else {
                continue;
            };
            // Checked again under the handle, and the lineage as it is now:
            // a live engine's tool forked a moment ago is spared too.
            let Some(name) = self.matches(pid) else {
                continue;
            };
            if self.descends_from_root(pid) || self.serves(pid) || signaller.exited(&handle) {
                continue;
            }
            found.push(Held {
                found: Found { pid, name },
                handle,
            });
        }
        found.sort_by_key(|p| p.found.pid);
        assert!(
            found
                .iter()
                .all(|p| p.found.pid > 1 && p.found.pid != self.me),
            "pid 1 and this process are never selected"
        );
        Ok(found)
    }

    fn matches(&self, pid: i32) -> Option<String> {
        let dir = self.proc_root.join(pid.to_string());
        if status_field(&dir, "Uid:")?.split_whitespace().next()? != self.uid.to_string() {
            return None;
        }
        let under = |link: &str| {
            std::fs::read_link(dir.join(link))
                .ok()
                .is_some_and(|target| self.under_worktree(&target))
        };
        if !under("cwd") && !under("exe") {
            return None;
        }
        if !started_for_an_agent(&dir) {
            return None;
        }
        Some(name_of(&dir))
    }

    fn under_worktree(&self, target: &Path) -> bool {
        // The kernel appends " (deleted)" to the link of a removed path.
        let text = target.to_string_lossy();
        let text = text.strip_suffix(" (deleted)").unwrap_or(&text);
        let target = Path::new(text);
        let resolved = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());
        resolved.starts_with(&self.worktree)
    }

    fn is_root(&self, pid: i32) -> bool {
        pid == self.me || is_daemon(&name_of(&self.proc_root.join(pid.to_string())))
    }

    /// Whether `pid` is this process or a daemon, or descends from one; an
    /// ancestry that cannot be traced to the top counts as protected.
    fn descends_from_root(&self, pid: i32) -> bool {
        self.lineage(pid)
            .is_none_or(|chain| chain.iter().any(|&p| self.is_root(p)))
    }

    /// Whether `pid` is a kept service, below one or in the group one leads.
    /// Not a group it only joined: `cmd &` leaves it in its shell's, often
    /// the engine's, which holds every stray the stage left.
    fn serves(&self, pid: i32) -> bool {
        if self.kept.is_empty() {
            return false;
        }
        let dir = self.proc_root.join(pid.to_string());
        let group = status_field(&dir, "NSpgid:")
            .and_then(|ids| ids.split_whitespace().next()?.parse::<i32>().ok());
        if group.is_some_and(|group| self.kept.contains(&group)) {
            return true;
        }
        // An untraceable lineage is spared as protected already.
        self.lineage(pid)
            .is_some_and(|chain| chain.iter().any(|p| self.kept.contains(p)))
    }

    /// The ancestors of this process and of the daemons, roots included.
    fn ancestors_of_roots(&self) -> Result<HashSet<i32>, ArchonError> {
        let mut roots = vec![self.me];
        for pid in pids(self.proc_root)? {
            if pid != self.me && self.is_root(pid) {
                roots.push(pid);
            }
        }
        let mut spared = HashSet::from([self.me]);
        for root in roots {
            match self.lineage(root) {
                Some(chain) => spared.extend(chain),
                // It exited during the scan, so it has no ancestors to spare.
                None if !self.proc_root.join(root.to_string()).exists() => {}
                None => {
                    return Err(ArchonError::Invalid(format!(
                        "cannot trace the ancestors of process {root}, so nothing is reaped"
                    )));
                }
            }
        }
        assert!(spared.contains(&self.me), "this process is spared");
        assert!(spared.iter().all(|&p| p > 0), "only real pids are spared");
        Ok(spared)
    }

    /// `pid` and its ancestors, nearest first, as /proc reads now; `None`
    /// when an unreadable link, a cycle or the depth bound stops the walk.
    fn lineage(&self, pid: i32) -> Option<Vec<i32>> {
        assert!(pid > 0, "a lineage starts at a real pid");
        let mut chain = Vec::new();
        let mut pid = pid;
        while pid > 0 {
            if chain.len() >= MAX_DEPTH || chain.contains(&pid) {
                return None;
            }
            chain.push(pid);
            let dir = self.proc_root.join(pid.to_string());
            pid = status_field(&dir, "PPid:")?.parse().ok()?;
        }
        Some(chain)
    }
}

/// Whether the process's environment carries Aether's engine tag; one that
/// cannot be read does not.
fn started_for_an_agent(dir: &Path) -> bool {
    std::fs::read(dir.join("environ")).is_ok_and(|environ| {
        environ
            .split(|&byte| byte == 0)
            .any(|var| var.starts_with(ENGINE_TAG))
    })
}

fn is_daemon(name: &str) -> bool {
    DAEMON_NAMES.contains(&name)
        || name
            .strip_prefix("aether-")
            .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit()))
}

/// Every pid listed, so an unreadable /proc fails rather than finds nothing.
fn pids(proc_root: &Path) -> Result<Vec<i32>, ArchonError> {
    let context = || format!("listing {}", proc_root.display());
    let mut pids = Vec::new();
    for entry in std::fs::read_dir(proc_root).map_err(|e| ArchonError::io(context(), e))? {
        let entry = entry.map_err(|e| ArchonError::io(context(), e))?;
        if let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

fn status_field(dir: &Path, key: &str) -> Option<String> {
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .map(|v| v.trim().to_string())
}

/// The executable's file name, falling back to `comm`.
fn name_of(dir: &Path) -> String {
    let exe = std::fs::read_link(dir.join("exe")).ok().and_then(|p| {
        let text = p.to_string_lossy().into_owned();
        let text = text.strip_suffix(" (deleted)").unwrap_or(&text).to_string();
        Path::new(&text)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    });
    exe.or_else(|| {
        std::fs::read_to_string(dir.join("comm"))
            .ok()
            .map(|c| c.trim().to_string())
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    const UID: u32 = 4242;

    fn fake_proc(proc_root: &Path, pid: i32, uid: u32, ppid: i32, cwd: &Path, exe: &Path) {
        let dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("status"),
            format!("Name:\tx\nPPid:\t{ppid}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        symlink(cwd, dir.join("cwd")).unwrap();
        symlink(exe, dir.join("exe")).unwrap();
        std::fs::write(dir.join("environ"), b"HOME=/h\0AETHER_ENGINE_RUN=01X\0").unwrap();
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        proc: PathBuf,
        root: PathBuf,
        tree: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let tree = base.join("worktrees/repo/item");
        let outside = base.join("elsewhere");
        for d in [&tree, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        let proc = base.join("proc");
        std::fs::create_dir_all(&proc).unwrap();
        // Init, where every walk ends; never selected, even under the tree.
        fake_proc(&proc, 1, UID, 0, &tree, &tree);
        Fixture {
            _tmp: tmp,
            proc,
            root: base.join("worktrees"),
            tree,
            outside,
        }
    }

    /// Handles are pids, and a process has exited once its fake /proc
    /// entry is gone; `on_open` and `on_signal` stage what happens meanwhile.
    struct Fake<'a> {
        proc: &'a Path,
        sent: RefCell<Vec<(i32, bool)>>,
        on_open: &'a dyn Fn(i32),
        on_signal: &'a dyn Fn(i32, bool),
    }

    impl<'a> Fake<'a> {
        fn new(proc: &'a Path) -> Fake<'a> {
            Fake {
                proc,
                sent: RefCell::new(Vec::new()),
                on_open: &|_| {},
                on_signal: &|_, _| {},
            }
        }
    }

    impl Signaller for Fake<'_> {
        type Handle = i32;

        fn open(&self, pid: i32) -> Result<Option<i32>, ArchonError> {
            (self.on_open)(pid);
            if self.proc.join(pid.to_string()).join("unopenable").exists() {
                return Err(ArchonError::Invalid(format!("no handle on {pid}")));
            }
            Ok(self.proc.join(pid.to_string()).exists().then_some(pid))
        }

        fn exited(&self, handle: &i32) -> bool {
            !self.proc.join(handle.to_string()).exists()
        }

        fn signal(&self, handle: &i32, kill: bool) {
            self.sent.borrow_mut().push((*handle, kill));
            (self.on_signal)(*handle, kill);
        }
    }

    fn selected_with(f: &Fixture, fake: &Fake) -> Vec<i32> {
        selected_keeping(f, fake, &BTreeSet::new())
    }

    fn selected_keeping(f: &Fixture, fake: &Fake, kept: &BTreeSet<i32>) -> Vec<i32> {
        let scan = Scan {
            proc_root: &f.proc,
            worktree: f.tree.clone(),
            kept,
            uid: UID,
            me: std::process::id() as i32,
        };
        let found = scan.find(fake).unwrap();
        found.iter().map(|p| p.found.pid).collect()
    }

    fn selected(f: &Fixture) -> Vec<i32> {
        selected_with(f, &Fake::new(&f.proc))
    }

    #[test]
    fn selects_by_cwd_or_exe_prefix_of_the_worktree() {
        let f = fixture();
        fake_proc(&f.proc, 100, UID, 1, &f.tree.join("sub"), &f.outside);
        fake_proc(
            &f.proc,
            101,
            UID,
            1,
            &f.outside,
            &f.tree.join(".venv/bin/python"),
        );
        fake_proc(&f.proc, 102, UID, 1, &f.outside, &f.outside);
        let sibling = f.tree.with_file_name("item-other");
        fake_proc(&f.proc, 103, UID, 1, &sibling, &sibling);
        assert_eq!(selected(&f), vec![100, 101]);
    }

    #[test]
    fn spares_other_users_pid_one_and_the_daemons_with_their_kin() {
        let f = fixture();
        fake_proc(&f.proc, 200, UID + 1, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 300, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 301, UID, 300, &f.tree, &f.tree.join("archon"));
        fake_proc(&f.proc, 302, UID, 1, &f.outside, &f.outside.join("aether"));
        // A live engine and its tool, which Aether reaps itself.
        fake_proc(&f.proc, 303, UID, 302, &f.tree, &f.tree);
        fake_proc(&f.proc, 304, UID, 303, &f.tree, &f.tree);
        let bridge = f.outside.join("aether-1.2.4-01ABC");
        fake_proc(&f.proc, 305, UID, 1, &f.outside, &bridge);
        fake_proc(&f.proc, 306, UID, 305, &f.tree, &f.tree);
        // Escaped: reparented to pid 1. Not a daemon by name alone.
        fake_proc(&f.proc, 400, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 401, UID, 1, &f.tree, &f.tree.join("aether-cli"));
        assert_eq!(selected(&f), vec![400, 401]);
    }

    /// A server setup started for the next stage outlives setup's end.
    #[test]
    fn spares_declared_services_with_their_groups_and_descendants() {
        let f = fixture();
        fake_proc(&f.proc, 500, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 501, UID, 500, &f.tree, &f.tree);
        fake_proc(&f.proc, 502, UID, 1, &f.tree, &f.tree);
        let status = f.proc.join("502/status");
        let grouped = std::fs::read_to_string(&status).unwrap() + "NSpgid:\t500\n";
        std::fs::write(&status, grouped).unwrap();
        fake_proc(&f.proc, 503, UID, 1, &f.tree, &f.tree);
        let fake = Fake::new(&f.proc);
        assert_eq!(selected(&f), vec![500, 501, 502, 503], "nothing declared");
        let kept = BTreeSet::from([500]);
        assert_eq!(selected_keeping(&f, &fake, &kept), vec![503]);
        // A group the service only joined, its shell's, is not kept.
        let led = std::fs::read_to_string(&status).unwrap();
        std::fs::write(&status, led.replace("NSpgid:\t500", "NSpgid:\t490")).unwrap();
        let shell = std::fs::read_to_string(f.proc.join("500/status")).unwrap();
        std::fs::write(f.proc.join("500/status"), shell + "NSpgid:\t490\n").unwrap();
        assert_eq!(selected_keeping(&f, &fake, &kept), vec![502, 503]);
        let stale = BTreeSet::from([9999]);
        assert_eq!(
            selected_keeping(&f, &fake, &stale),
            vec![500, 501, 502, 503]
        );
    }

    #[test]
    fn spares_what_no_engine_started_such_as_an_operator_shell() {
        let f = fixture();
        // The operator's shell in the worktree, under their terminal.
        fake_proc(
            &f.proc,
            600,
            UID,
            1,
            &f.outside,
            &f.outside.join("alacritty"),
        );
        fake_proc(&f.proc, 601, UID, 600, &f.tree, &f.outside.join("bash"));
        let shell_env = b"HOME=/h\0TERM=xterm\0NOT_AETHER_ENGINE_RUN=1\0";
        std::fs::write(f.proc.join("601/environ"), shell_env).unwrap();
        fake_proc(&f.proc, 602, UID, 1, &f.tree, &f.tree);
        std::fs::remove_file(f.proc.join("602/environ")).unwrap();
        // A tool that escaped its engine keeps the engine's environment.
        fake_proc(&f.proc, 603, UID, 1, &f.tree, &f.outside.join("node"));
        assert_eq!(selected(&f), vec![603]);
    }

    #[test]
    fn a_deleted_worktree_still_matches_and_a_missing_link_does_not() {
        let f = fixture();
        let gone = f.tree.join("gone (deleted)");
        fake_proc(&f.proc, 500, UID, 1, &gone, &f.outside);
        std::fs::create_dir_all(f.proc.join("501")).unwrap();
        std::fs::write(
            f.proc.join("501/status"),
            format!("PPid:\t1\nUid:\t{UID}\n"),
        )
        .unwrap();
        assert_eq!(selected(&f), vec![500]);
    }

    #[test]
    fn a_pid_taken_over_before_its_handle_opens_is_rechecked() {
        let f = fixture();
        fake_proc(&f.proc, 302, UID, 1, &f.outside, &f.outside.join("aether"));
        fake_proc(&f.proc, 800, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 801, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 802, UID, 1, &f.tree, &f.tree);
        let proc = f.proc.clone();
        let outside = f.outside.clone();
        // Between the first check and the handle, 800 exits and its pid goes
        // to an unrelated process, and 801 to a tool the engine just forked.
        let on_open = move |pid: i32| {
            let dir = proc.join(pid.to_string());
            if pid == 800 {
                std::fs::remove_file(dir.join("cwd")).unwrap();
                std::fs::remove_file(dir.join("exe")).unwrap();
                symlink(&outside, dir.join("cwd")).unwrap();
                symlink(&outside, dir.join("exe")).unwrap();
            }
            if pid == 801 {
                let status = format!("PPid:\t302\nUid:\t{UID}\n");
                std::fs::write(dir.join("status"), status).unwrap();
            }
        };
        let fake = Fake {
            on_open: &on_open,
            ..Fake::new(&f.proc)
        };
        assert_eq!(selected_with(&f, &fake), vec![802]);
    }

    #[test]
    fn terminates_then_kills_what_ignores_term_and_reports_survivors() {
        let f = fixture();
        fake_proc(&f.proc, 600, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 601, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 602, UID, 1, &f.tree, &f.tree);
        let proc = f.proc.clone();
        // 600 exits on TERM, 601 on KILL, 602 never.
        let on_signal = move |pid: i32, kill: bool| {
            if pid == 600 || (pid == 601 && kill) {
                std::fs::remove_dir_all(proc.join(pid.to_string())).unwrap();
            }
        };
        let fake = Fake {
            on_signal: &on_signal,
            ..Fake::new(&f.proc)
        };
        let none = Duration::from_millis(1);
        let report = reap_with(
            &f.proc,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        )
        .unwrap();
        let pids = |v: &[Found]| v.iter().map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(&report.found), vec![600, 601, 602]);
        assert_eq!(pids(&report.killed), vec![601, 602]);
        assert_eq!(pids(&report.survivors), vec![602]);
        let expected = [
            (600, false),
            (601, false),
            (602, false),
            (601, true),
            (602, true),
        ];
        assert_eq!(*fake.sent.borrow(), expected);
        let text = report.describe(&f.tree);
        assert!(text.contains("Still running after SIGKILL: 602"));
    }

    #[test]
    fn an_ancestry_that_cannot_be_traced_is_spared() {
        let f = fixture();
        fake_proc(&f.proc, 2, UID, 1, &f.outside, &f.outside.join("aether"));
        // A tool deeper below the daemon than the walk reaches.
        let deepest = 3 + MAX_DEPTH as i32;
        for pid in 3..=deepest {
            fake_proc(&f.proc, pid, UID, pid - 1, &f.tree, &f.tree);
        }
        // A cycle, and a parent whose status does not parse.
        fake_proc(&f.proc, 5000, UID, 5001, &f.tree, &f.tree);
        fake_proc(&f.proc, 5001, UID, 5000, &f.tree, &f.tree);
        fake_proc(&f.proc, 5002, UID, 5003, &f.tree, &f.tree);
        std::fs::create_dir_all(f.proc.join("5003")).unwrap();
        std::fs::write(f.proc.join("5003/status"), "PPid:\tnope\n").unwrap();
        fake_proc(&f.proc, 5004, UID, 1, &f.tree, &f.tree);
        assert_eq!(selected(&f), vec![5004]);
    }

    #[test]
    fn a_daemon_whose_ancestry_cannot_be_traced_fails_the_reap() {
        let f = fixture();
        fake_proc(&f.proc, 10, UID, 11, &f.outside, &f.outside.join("archon"));
        fake_proc(&f.proc, 11, UID, 10, &f.outside, &f.outside);
        fake_proc(&f.proc, 12, UID, 1, &f.tree, &f.tree);
        let fake = Fake::new(&f.proc);
        let none = Duration::ZERO;
        let reaped = reap_with(
            &f.proc,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        );
        assert!(
            reaped
                .unwrap_err()
                .to_string()
                .contains("ancestors of process 10")
        );
        assert!(fake.sent.borrow().is_empty(), "no signal sent");
    }

    #[test]
    fn a_process_that_cannot_be_held_or_listed_fails_the_reap() {
        let f = fixture();
        fake_proc(&f.proc, 900, UID, 1, &f.tree, &f.tree);
        std::fs::write(f.proc.join("900/unopenable"), "").unwrap();
        let fake = Fake::new(&f.proc);
        let none = Duration::ZERO;
        let held = reap_with(
            &f.proc,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        );
        assert!(held.unwrap_err().to_string().contains("no handle on 900"));
        let missing = f.proc.join("missing");
        let listed = reap_with(
            &missing,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        );
        assert!(listed.is_err(), "an unreadable /proc is no empty one");
        assert!(fake.sent.borrow().is_empty(), "no signal sent");
    }

    #[test]
    fn nothing_found_sends_nothing() {
        let f = fixture();
        fake_proc(&f.proc, 700, UID, 1, &f.outside, &f.outside);
        let fake = Fake::new(&f.proc);
        let none = Duration::ZERO;
        let report = reap_with(
            &f.proc,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        );
        assert!(report.unwrap().is_empty());
        assert!(fake.sent.borrow().is_empty(), "no signal sent");
    }

    #[test]
    fn a_tree_redirected_by_a_symbolic_link_or_outside_the_root_is_refused() {
        let f = fixture();
        fake_proc(&f.proc, 950, UID, 1, &f.outside, &f.outside);
        let moved = f.tree.with_file_name("moved");
        std::fs::rename(&f.tree, &moved).unwrap();
        symlink(&f.outside, &f.tree).unwrap();
        let fake = Fake::new(&f.proc);
        let none = Duration::ZERO;
        let reap = |tree: &Path| {
            reap_with(
                &f.proc,
                &f.root,
                tree,
                &BTreeSet::new(),
                UID,
                (none, none),
                &fake,
            )
        };
        let redirected = reap(&f.tree).unwrap_err().to_string();
        assert!(redirected.contains("by a symbolic link"), "{redirected}");
        let outside = reap(&f.outside).unwrap_err().to_string();
        assert!(outside.contains("outside the worktree root"), "{outside}");
        let whole_root = reap(&f.root.join("repo/..")).unwrap_err().to_string();
        assert!(whole_root.contains("not a tree below"), "{whole_root}");
        assert!(fake.sent.borrow().is_empty(), "no signal sent");
        assert!(reap(&moved).unwrap().is_empty(), "the real tree is reaped");
    }

    #[test]
    fn a_child_forked_while_ending_is_found_by_the_next_scan() {
        let f = fixture();
        fake_proc(&f.proc, 960, UID, 1, &f.tree, &f.tree);
        let (proc, tree) = (f.proc.clone(), f.tree.clone());
        // 960 forks 961 into the tree as it exits on TERM; 961 exits on TERM.
        let on_signal = move |pid: i32, _kill: bool| {
            std::fs::remove_dir_all(proc.join(pid.to_string())).unwrap();
            if pid == 960 {
                fake_proc(&proc, 961, UID, 1, &tree, &tree);
            }
        };
        let fake = Fake {
            on_signal: &on_signal,
            ..Fake::new(&f.proc)
        };
        let none = Duration::ZERO;
        let report = reap_with(
            &f.proc,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        )
        .unwrap();
        let pids: Vec<i32> = report.found.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![960, 961]);
        assert!(report.survivors.is_empty(), "the tree is clear");
        assert_eq!(*fake.sent.borrow(), [(960, false), (961, false)]);
    }

    #[test]
    fn a_tree_still_respawning_after_every_round_reports_a_survivor() {
        let f = fixture();
        fake_proc(&f.proc, 970, UID, 1, &f.tree, &f.tree);
        let (proc, tree) = (f.proc.clone(), f.tree.clone());
        // A supervisor outside the scan restarts each child as it ends.
        let on_signal = move |pid: i32, _kill: bool| {
            std::fs::remove_dir_all(proc.join(pid.to_string())).unwrap();
            fake_proc(&proc, pid + 1, UID, 1, &tree, &tree);
        };
        let fake = Fake {
            on_signal: &on_signal,
            ..Fake::new(&f.proc)
        };
        let none = Duration::ZERO;
        let report = reap_with(
            &f.proc,
            &f.root,
            &f.tree,
            &BTreeSet::new(),
            UID,
            (none, none),
            &fake,
        )
        .unwrap();
        let last = 970 + ROUNDS as i32;
        let survivors: Vec<i32> = report.survivors.iter().map(|p| p.pid).collect();
        assert_eq!(survivors, vec![last]);
        assert_eq!(fake.sent.borrow().len(), ROUNDS, "one TERM a round");
        assert!(
            report
                .describe(&f.tree)
                .contains("Still running after SIGKILL")
        );
    }

    #[cfg(target_os = "linux")]
    mod live {
        use super::*;
        use crate::reaper::orphan::Orphan;
        use std::process::Command;

        fn real_reap(tree: &Path, term_grace: Duration) -> Report {
            real_reap_keeping(tree, term_grace, &BTreeSet::new())
        }

        fn real_reap_keeping(tree: &Path, term_grace: Duration, kept: &BTreeSet<i32>) -> Report {
            let uid = rustix::process::getuid().as_raw();
            let kill_grace = Duration::from_secs(5);
            let root = tree.parent().unwrap();
            let graces = (term_grace, kill_grace);
            reap_with(Path::new("/proc"), root, tree, kept, uid, graces, &PidFds).unwrap()
        }

        fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
            let tmp = tempfile::tempdir().unwrap();
            let base = tmp.path().canonicalize().unwrap();
            let (tree, other) = (base.join("worktrees/item"), base.join("other"));
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::create_dir_all(&other).unwrap();
            (tmp, tree, other)
        }

        #[test]
        fn a_detached_child_under_the_worktree_is_reaped_and_a_bystander_is_not() {
            let (_tmp, tree, other) = dirs();
            let inside = Orphan::start("exec sleep 300", &tree);
            let bystander = Orphan::start("exec sleep 300", &other);
            let report = real_reap(&tree, Duration::from_secs(5));
            let pids: Vec<i32> = report.found.iter().map(|p| p.pid).collect();
            assert_eq!(pids, vec![inside.0]);
            assert!(report.killed.is_empty(), "sleep ends on SIGTERM");
            assert!(inside.gone());
            assert!(bystander.alive(), "left alone");
        }

        #[test]
        fn a_declared_service_survives_until_a_reap_that_keeps_nothing() {
            let (_tmp, tree, _other) = dirs();
            let server = Orphan::start("exec sleep 300", &tree);
            let stray = Orphan::start("exec sleep 300", &tree);
            let kept = BTreeSet::from([server.0]);
            let report = real_reap_keeping(&tree, Duration::from_secs(5), &kept);
            let pids: Vec<i32> = report.found.iter().map(|p| p.pid).collect();
            assert_eq!(pids, vec![stray.0], "only the undeclared one");
            assert!(server.alive() && stray.gone());
            let report = real_reap(&tree, Duration::from_secs(5));
            assert_eq!(report.found.len(), 1, "{report:?}");
            assert!(server.gone(), "the run's end stops it");
        }

        #[test]
        fn a_child_ignoring_term_is_killed_after_the_grace_period() {
            let (_tmp, tree, _other) = dirs();
            // An ignored signal stays ignored across exec.
            let stubborn = Orphan::start("trap '' TERM; exec sleep 300", &tree);
            stubborn.wait_exec("sleep");
            let report = real_reap(&tree, Duration::from_millis(200));
            assert_eq!(report.killed.len(), 1);
            assert!(report.survivors.is_empty());
            assert!(stubborn.gone());
        }

        #[test]
        fn a_descendant_of_this_process_is_spared() {
            let (_tmp, tree, _other) = dirs();
            let mut child = Command::new("sleep")
                .arg("300")
                .current_dir(&tree)
                .spawn()
                .unwrap();
            let report = real_reap(&tree, Duration::from_millis(200));
            let spared = child.try_wait().unwrap().is_none();
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(report.is_empty());
            assert!(spared);
        }
    }
}

/// Processes tests start and orphan, for the reaper's own live tests and
/// those of the removals it guards.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod orphan {
    use super::{POLL, PidFds, Signaller};
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A process this test started and orphaned, as `nohup cmd &` from a
    /// shell that then exits; killed on drop should a test fail first.
    pub(crate) struct Orphan(pub(crate) i32);

    impl Orphan {
        pub(crate) fn start(script: &str, cwd: &Path) -> Orphan {
            let line = format!("sh -c \"{script}\" </dev/null >/dev/null 2>&1 & echo $!");
            let out = Command::new("sh")
                .args(["-c", &line])
                .env("AETHER_ENGINE_RUN", "archon-test")
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(out.status.success(), "the launching shell exits");
            let pid: i32 = String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(pid > 1, "a real pid");
            Orphan(pid)
        }

        pub(crate) fn alive(&self) -> bool {
            Path::new(&format!("/proc/{}/cwd", self.0))
                .read_link()
                .is_ok()
        }

        /// Wait until its shell has exec'd `comm`, past any `trap`.
        pub(crate) fn wait_exec(&self, comm: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            let path = format!("/proc/{}/comm", self.0);
            while std::fs::read_to_string(&path).unwrap_or_default().trim() != comm {
                assert!(Instant::now() < deadline, "{comm} never started");
                std::thread::sleep(POLL);
            }
        }

        /// Whether it is gone within a few seconds, once init reaped it.
        pub(crate) fn gone(&self) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.alive() && Instant::now() < deadline {
                std::thread::sleep(POLL);
            }
            !self.alive()
        }
    }

    /// Wait until `path` exists, as a started script's sign it is ready.
    pub(crate) fn wait_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(POLL);
        }
    }

    impl Drop for Orphan {
        fn drop(&mut self) {
            if let Ok(Some(handle)) = PidFds.open(self.0) {
                PidFds.signal(&handle, true);
            }
        }
    }
}
