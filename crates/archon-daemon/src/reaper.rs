//! Reaping the processes a stage leaves under its worktree (Linux only).
//!
//! A tool process that double-forks or runs under `nohup` escapes the
//! engine's process group, so a stage's end, `archon gc` and item cleanup
//! look for processes whose working directory or executable lies under the
//! worktree and end them: SIGTERM, a grace period, then SIGKILL.
//!
//! Selection is conservative: same user, never pid 1, never this process,
//! Archon's or Aether's daemons, their ancestors or their descendants. A
//! live engine and its tools descend from Aether, which reaps them itself;
//! what escaped was reparented away. Off Linux nothing is scanned or killed.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use archon_core::error::ArchonError;

/// How long SIGTERM'd processes get to exit before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How long SIGKILL'd processes get to vanish before they count as survivors.
const KILL_GRACE: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(50);

/// Executable names of the daemons, spared with their ancestors and
/// descendants; Aether also runs from versioned copies (`aether-1.2.4-…`).
const DAEMON_NAMES: [&str; 4] = ["archon", "archond", "aether", "aetherd"];

/// Bounds the ancestor walk, so a cyclic fake /proc cannot hang it.
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
    /// Processes that were running when the reap began.
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
    /// A handle on whatever process holds `pid` now.
    fn open(&self, pid: i32) -> Option<Self::Handle>;
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
    uid: u32,
    me: i32,
}

/// End every process of the current user whose cwd or executable lies under
/// `worktree`, as described in the module docs.
#[cfg(target_os = "linux")]
pub fn reap(worktree: &Path) -> Result<Report, ArchonError> {
    let uid = rustix::process::getuid().as_raw();
    let proc_root = Path::new("/proc");
    reap_with(proc_root, worktree, uid, TERM_GRACE, KILL_GRACE, &PidFds)
}

/// Nothing is scanned or killed off Linux.
#[cfg(not(target_os = "linux"))]
pub fn reap(_worktree: &Path) -> Result<Report, ArchonError> {
    Ok(Report::default())
}

/// [`reap`] before `worktree` is removed: what was running is ended and
/// logged, and a process that survives SIGKILL refuses the removal.
pub fn clear_for_removal(worktree: &Path) -> Result<(), ArchonError> {
    let report = reap(worktree)?;
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

#[cfg(target_os = "linux")]
struct PidFds;

#[cfg(target_os = "linux")]
impl Signaller for PidFds {
    type Handle = std::os::fd::OwnedFd;

    fn open(&self, pid: i32) -> Option<Self::Handle> {
        let pid = rustix::process::Pid::from_raw(pid)?;
        rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()
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
        rustix::event::poll(&mut fds, Some(&now)).map_or(true, |ready| ready > 0)
    }

    fn signal(&self, handle: &Self::Handle, kill: bool) {
        let signal = if kill {
            rustix::process::Signal::KILL
        } else {
            rustix::process::Signal::TERM
        };
        // ESRCH: it exited since the scan, which is what was wanted.
        let _ = rustix::process::pidfd_send_signal(handle, signal);
    }
}

fn reap_with<S: Signaller>(
    proc_root: &Path,
    worktree: &Path,
    uid: u32,
    term_grace: Duration,
    kill_grace: Duration,
    signaller: &S,
) -> Result<Report, ArchonError> {
    assert!(worktree.is_absolute(), "worktree paths are absolute");
    assert!(
        worktree.components().count() > 2,
        "refusing to reap under {}",
        worktree.display()
    );
    let scan = Scan {
        proc_root,
        worktree: worktree
            .canonicalize()
            .unwrap_or_else(|_| worktree.to_path_buf()),
        uid,
        me: std::process::id() as i32,
    };
    let found = scan.find(signaller);
    if found.is_empty() {
        return Ok(Report::default());
    }
    for p in &found {
        signaller.signal(&p.handle, false);
    }
    let remaining = wait_exit(signaller, found.iter().collect(), term_grace);
    for p in &remaining {
        signaller.signal(&p.handle, true);
    }
    let survivors = wait_exit(signaller, remaining.clone(), kill_grace);
    let names = |held: &[&Held<S::Handle>]| held.iter().map(|p| p.found.clone()).collect();
    let report = Report {
        found: found.iter().map(|p| p.found.clone()).collect(),
        killed: names(&remaining),
        survivors: names(&survivors),
    };
    assert!(
        report.killed.len() <= report.found.len(),
        "kills are a subset"
    );
    assert!(
        report.survivors.len() <= report.killed.len(),
        "survivors were killed"
    );
    Ok(report)
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
    fn find<S: Signaller>(&self, signaller: &S) -> Vec<Held<S::Handle>> {
        let spared = self.ancestors_of_roots();
        let mut found = Vec::new();
        for pid in pids(self.proc_root) {
            if pid <= 1 || spared.contains(&pid) || self.matches(pid).is_none() {
                continue;
            }
            let Some(handle) = signaller.open(pid) else {
                continue;
            };
            // Checked again under the handle, and the lineage as it is now:
            // a live engine's tool forked a moment ago is spared too.
            let Some(name) = self.matches(pid) else {
                continue;
            };
            if self.descends_from_root(pid) || signaller.exited(&handle) {
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
        found
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

    /// Whether `pid` is this process or a daemon, or descends from one.
    fn descends_from_root(&self, pid: i32) -> bool {
        self.lineage(pid).iter().any(|&p| self.is_root(p))
    }

    /// The ancestors of this process and of the daemons, roots included.
    fn ancestors_of_roots(&self) -> HashSet<i32> {
        let mut spared = HashSet::new();
        for pid in pids(self.proc_root) {
            if self.is_root(pid) {
                spared.extend(self.lineage(pid));
            }
        }
        spared.extend(self.lineage(self.me));
        assert!(spared.contains(&self.me), "this process is spared");
        spared
    }

    /// `pid` and its ancestors, nearest first, as /proc reads now.
    fn lineage(&self, pid: i32) -> Vec<i32> {
        let mut chain = Vec::new();
        let mut pid = pid;
        for _ in 0..MAX_DEPTH {
            if pid <= 0 || chain.contains(&pid) {
                break;
            }
            chain.push(pid);
            let dir = self.proc_root.join(pid.to_string());
            pid = status_field(&dir, "PPid:")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
        chain
    }
}

fn is_daemon(name: &str) -> bool {
    DAEMON_NAMES.contains(&name)
        || name
            .strip_prefix("aether-")
            .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit()))
}

fn pids(proc_root: &Path) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
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
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        proc: PathBuf,
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
        Fixture {
            _tmp: tmp,
            proc,
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

        fn open(&self, pid: i32) -> Option<i32> {
            (self.on_open)(pid);
            self.proc.join(pid.to_string()).exists().then_some(pid)
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
        let scan = Scan {
            proc_root: &f.proc,
            worktree: f.tree.clone(),
            uid: UID,
            me: std::process::id() as i32,
        };
        let found = scan.find(fake);
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
        fake_proc(&f.proc, 1, UID, 0, &f.tree, &f.tree);
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
        let report = reap_with(&f.proc, &f.tree, UID, none, none, &fake).unwrap();
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
    fn nothing_found_sends_nothing() {
        let f = fixture();
        fake_proc(&f.proc, 700, UID, 1, &f.outside, &f.outside);
        let fake = Fake::new(&f.proc);
        let report = reap_with(&f.proc, &f.tree, UID, Duration::ZERO, Duration::ZERO, &fake);
        assert!(report.unwrap().is_empty());
        assert!(fake.sent.borrow().is_empty(), "no signal sent");
    }

    #[cfg(target_os = "linux")]
    mod live {
        use super::*;
        use std::process::Command;

        /// A process this test started and orphaned, as `nohup cmd &` from a
        /// shell that then exits; killed on drop should a test fail first.
        struct Orphan(i32);

        impl Orphan {
            fn start(script: &str, cwd: &Path) -> Orphan {
                let line = format!("sh -c \"{script}\" </dev/null >/dev/null 2>&1 & echo $!");
                let out = Command::new("sh")
                    .args(["-c", &line])
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

            fn alive(&self) -> bool {
                Path::new(&format!("/proc/{}/cwd", self.0))
                    .read_link()
                    .is_ok()
            }

            /// Wait until its shell has exec'd `comm`, past any `trap`.
            fn wait_exec(&self, comm: &str) {
                let deadline = Instant::now() + Duration::from_secs(5);
                let path = format!("/proc/{}/comm", self.0);
                while std::fs::read_to_string(&path).unwrap_or_default().trim() != comm {
                    assert!(Instant::now() < deadline, "{comm} never started");
                    std::thread::sleep(POLL);
                }
            }

            /// Whether it is gone within a few seconds, once init reaped it.
            fn gone(&self) -> bool {
                let deadline = Instant::now() + Duration::from_secs(5);
                while self.alive() && Instant::now() < deadline {
                    std::thread::sleep(POLL);
                }
                !self.alive()
            }
        }

        impl Drop for Orphan {
            fn drop(&mut self) {
                if let Some(handle) = PidFds.open(self.0) {
                    PidFds.signal(&handle, true);
                }
            }
        }

        fn real_reap(tree: &Path, term_grace: Duration) -> Report {
            let uid = rustix::process::getuid().as_raw();
            let kill_grace = Duration::from_secs(5);
            reap_with(
                Path::new("/proc"),
                tree,
                uid,
                term_grace,
                kill_grace,
                &PidFds,
            )
            .unwrap()
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
