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

use std::collections::{BTreeSet, HashMap, HashSet};
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

/// Where to look and whom to spare.
struct Scan<'a> {
    proc_root: &'a Path,
    worktree: PathBuf,
    uid: u32,
    protected: HashSet<i32>,
}

/// End every process of the current user whose cwd or executable lies under
/// `worktree`, as described in the module docs. A no-op off Linux.
pub fn reap(worktree: &Path) -> Result<Report, ArchonError> {
    if !cfg!(target_os = "linux") {
        return Ok(Report::default());
    }
    let uid = rustix::process::getuid().as_raw();
    reap_with(
        Path::new("/proc"),
        worktree,
        uid,
        TERM_GRACE,
        KILL_GRACE,
        &|pid, kill| send(pid, kill),
    )
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

fn send(pid: i32, kill: bool) {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return;
    };
    let signal = if kill {
        rustix::process::Signal::KILL
    } else {
        rustix::process::Signal::TERM
    };
    // The process may have exited since the scan; nothing to report then.
    let _ = rustix::process::kill_process(pid, signal);
}

fn reap_with(
    proc_root: &Path,
    worktree: &Path,
    uid: u32,
    term_grace: Duration,
    kill_grace: Duration,
    signal: &dyn Fn(i32, bool),
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
        protected: protected_pids(proc_root),
    };
    let found = scan.find();
    if found.is_empty() {
        return Ok(Report::default());
    }
    for p in &found {
        signal(p.pid, false);
    }
    let remaining = scan.wait_gone(&found, term_grace);
    for p in &remaining {
        signal(p.pid, true);
    }
    let survivors = scan.wait_gone(&remaining, kill_grace);
    let report = Report {
        found,
        killed: remaining,
        survivors,
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

impl Scan<'_> {
    /// The processes under the worktree right now, by pid.
    fn find(&self) -> Vec<Found> {
        let mut found: BTreeSet<Found> = BTreeSet::new();
        for pid in pids(self.proc_root) {
            if pid <= 1 || self.protected.contains(&pid) {
                continue;
            }
            if let Some(name) = self.matches(pid) {
                found.insert(Found { pid, name });
            }
        }
        found.into_iter().collect()
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

    /// The members of `watched` still found after waiting up to `grace`.
    fn wait_gone(&self, watched: &[Found], grace: Duration) -> Vec<Found> {
        let deadline = Instant::now() + grace;
        loop {
            let now: Vec<Found> = self
                .find()
                .into_iter()
                .filter(|p| watched.contains(p))
                .collect();
            if now.is_empty() || Instant::now() >= deadline {
                return now;
            }
            std::thread::sleep(POLL);
        }
    }
}

fn is_daemon(name: &str) -> bool {
    DAEMON_NAMES.contains(&name)
        || name
            .strip_prefix("aether-")
            .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit()))
}

/// This process and the daemons, with all their ancestors and descendants.
fn protected_pids(proc_root: &Path) -> HashSet<i32> {
    let mut parent_of = HashMap::new();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut roots = vec![std::process::id() as i32];
    for pid in pids(proc_root) {
        let dir = proc_root.join(pid.to_string());
        let ppid: i32 = status_field(&dir, "PPid:")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        parent_of.insert(pid, ppid);
        children.entry(ppid).or_default().push(pid);
        if is_daemon(&name_of(&dir)) {
            roots.push(pid);
        }
    }
    let mut protected = HashSet::new();
    for &root in &roots {
        let mut pid = root;
        for _ in 0..MAX_DEPTH {
            if pid <= 0 || !protected.insert(pid) {
                break;
            }
            pid = parent_of.get(&pid).copied().unwrap_or(0);
        }
    }
    let mut below = roots.clone();
    let mut seen = HashSet::new();
    while let Some(pid) = below.pop() {
        if seen.insert(pid) {
            protected.insert(pid);
            below.extend(children.get(&pid).into_iter().flatten());
        }
    }
    assert!(
        roots.iter().all(|r| protected.contains(r)),
        "every root is spared"
    );
    protected
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

    fn selected(f: &Fixture) -> Vec<i32> {
        let scan = Scan {
            proc_root: &f.proc,
            worktree: f.tree.clone(),
            uid: UID,
            protected: protected_pids(&f.proc),
        };
        scan.find().into_iter().map(|p| p.pid).collect()
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
    fn terminates_then_kills_what_ignores_term_and_reports_survivors() {
        let f = fixture();
        fake_proc(&f.proc, 600, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 601, UID, 1, &f.tree, &f.tree);
        fake_proc(&f.proc, 602, UID, 1, &f.tree, &f.tree);
        let sent = RefCell::new(Vec::new());
        let proc = f.proc.clone();
        // 600 exits on TERM, 601 on KILL, 602 never.
        let signal = |pid: i32, kill: bool| {
            sent.borrow_mut().push((pid, kill));
            if pid == 600 || (pid == 601 && kill) {
                std::fs::remove_dir_all(proc.join(pid.to_string())).unwrap();
            }
        };
        let none = Duration::from_millis(1);
        let report = reap_with(&f.proc, &f.tree, UID, none, none, &signal).unwrap();
        let pids = |v: &[Found]| v.iter().map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(&report.found), vec![600, 601, 602]);
        assert_eq!(pids(&report.killed), vec![601, 602]);
        assert_eq!(pids(&report.survivors), vec![602]);
        assert_eq!(
            *sent.borrow(),
            vec![
                (600, false),
                (601, false),
                (602, false),
                (601, true),
                (602, true)
            ]
        );
        assert!(
            report
                .describe(&f.tree)
                .contains("Still running after SIGKILL: 602")
        );
    }

    #[test]
    fn nothing_found_sends_nothing() {
        let f = fixture();
        fake_proc(&f.proc, 700, UID, 1, &f.outside, &f.outside);
        let signal = |_: i32, _: bool| panic!("no signal expected");
        let report = reap_with(
            &f.proc,
            &f.tree,
            UID,
            Duration::ZERO,
            Duration::ZERO,
            &signal,
        )
        .unwrap();
        assert!(report.is_empty());
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
                if self.alive() {
                    send(self.0, true);
                }
            }
        }

        fn real_reap(tree: &Path, term_grace: Duration) -> Report {
            let uid = rustix::process::getuid().as_raw();
            let kill_grace = Duration::from_secs(5);
            reap_with(Path::new("/proc"), tree, uid, term_grace, kill_grace, &send).unwrap()
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
