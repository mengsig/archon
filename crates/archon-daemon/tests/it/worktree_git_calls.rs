//! The git the daemon runs in a role's worktree, counted through a `git`
//! on `PATH` that logs each call: proving a worktree is the one git
//! registered there takes one `rev-parse`.

use crate::common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::team_worktree::team_run;

const INNER: &str = "a_retried_agent_s_worktree_under_a_counting_git";
const LOG_ENV: &str = "ARCHON_TEST_GIT_LOG";
const MARK: &str = "--- ensure_worker";
const DONE: &str = "--- done";

/// The `git` the tests would run, found on `PATH`.
fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|git| git.is_file())
        .expect("git is on PATH")
}

/// A `git` in `dir` that logs its arguments to `$ARCHON_TEST_GIT_LOG`,
/// then runs the real one.
fn counting_git(dir: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${LOG_ENV}\"\nexec '{}' \"$@\"\n",
        real_git().display()
    );
    let shim = dir.join("git");
    archon_core::testing::write_script(&shim, &script);
}

#[test]
fn a_retried_agent_s_worktree_is_proven_with_one_rev_parse() {
    let dir = tempfile::tempdir().expect("tempdir");
    counting_git(dir.path());
    let log = dir.path().join("git.log");
    let path = std::env::var_os("PATH").expect("PATH is set");
    let mut paths = vec![dir.path().to_owned()];
    paths.extend(std::env::split_paths(&path));
    let status = Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            &common::exact_test_name(module_path!(), INNER),
            "--exact",
            "--ignored",
            "--quiet",
        ])
        .env("PATH", std::env::join_paths(paths).expect("PATH"))
        .env(LOG_ENV, &log)
        .status()
        .expect("the inner test runs");
    assert!(status.success(), "the inner test failed");

    let logged = std::fs::read_to_string(&log).expect("git was logged");
    let (_, calls) = logged
        .split_once(MARK)
        .expect("the inner test marked its call");
    let (calls, _) = calls.split_once(DONE).expect("the call returned");
    let rev_parses: Vec<&str> = calls
        .lines()
        .filter(|l| l.contains(" rev-parse "))
        .collect();
    let located: Vec<&&str> = rev_parses
        .iter()
        .filter(|l| l.contains("--show-toplevel"))
        .collect();
    assert_eq!(located.len(), 1, "{rev_parses:#?}");
    assert!(
        located[0].ends_with("--show-toplevel --git-common-dir --git-dir"),
        "{located:?}"
    );
    // The other finds the repository's common git directory.
    assert_eq!(rev_parses.len(), 2, "{rev_parses:#?}");
}

/// Run only by the test above, with the counting `git` first on `PATH`.
#[test]
#[ignore = "run by a_retried_agent_s_worktree_is_proven_with_one_rev_parse"]
fn a_retried_agent_s_worktree_under_a_counting_git() {
    let log = std::env::var_os(LOG_ENV).expect("run with a counting git");
    let r = team_run();
    r.worker("api");
    let mark = |text: &str| {
        let mut file = std::fs::File::options()
            .append(true)
            .open(&log)
            .expect("the log");
        writeln!(file, "{text}").expect("mark the log");
    };
    mark(MARK);
    assert_eq!(r.ensure_kept("api", 1).expect("the same attempt"), None);
    mark(DONE);
}
