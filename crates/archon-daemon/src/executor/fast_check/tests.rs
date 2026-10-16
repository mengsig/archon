use std::path::PathBuf;
use std::process::Command as Std;
use std::time::Instant;

use archon_core::repo::{Attribution, Checks, ShipMode};

use super::*;

fn dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

const LIMIT: Duration = Duration::from_secs(20);

/// Whether `pid` still runs: neither gone nor a zombie awaiting its reaper.
pub(crate) fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')),
        Err(_) => rustix::process::Pid::from_raw(pid)
            .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok()),
    }
}

/// Wait up to five seconds for `pid` to stop running.
pub(crate) async fn ended(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    !alive(pid)
}

#[tokio::test]
async fn a_run_reports_how_it_ended_with_its_output_and_environment() {
    let (_keep, tree) = dir();
    let ran = run("echo out; echo err >&2", &tree, &[], LIMIT)
        .await
        .unwrap();
    assert_eq!(ran.ended, Ended::Passed);
    assert_eq!(
        (ran.stdout.as_str(), ran.stderr.as_str()),
        ("out\n", "err\n")
    );
    let failed = run("echo the-test-broke; exit 3", &tree, &[], LIMIT)
        .await
        .unwrap();
    let Ended::Failed(why) = &failed.ended else {
        panic!("{failed:?}")
    };
    assert!(why.contains('3'), "{why}");
    assert!(
        failed.shown().contains("the-test-broke"),
        "{}",
        failed.shown()
    );
    let target = tree.join("target");
    let envs = [("CARGO_TARGET_DIR", target.as_path())];
    let ran = run(
        "printf %s \"$CARGO_TARGET_DIR\"; pwd >&2",
        &tree,
        &envs,
        LIMIT,
    )
    .await
    .unwrap();
    assert_eq!(ran.stdout, target.to_str().unwrap());
    assert_eq!(
        ran.stderr.trim(),
        tree.to_str().unwrap(),
        "it runs in the tree"
    );
}

#[tokio::test]
async fn an_overrunning_run_is_killed_with_all_it_started() {
    let (_keep, tree) = dir();
    let started = Instant::now();
    let command = "sleep 30 & echo $! > pid; wait";
    // Room for a loaded host's shell to start the child and record it.
    let ran = run(command, &tree, &[], Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        ran.ended,
        Ended::Failed("it did not finish within 3 seconds".into())
    );
    let pid: i32 = std::fs::read_to_string(tree.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(ended(pid).await, "the check's own child {pid} was killed");
}

#[tokio::test]
async fn a_run_keeps_only_the_end_of_its_output() {
    let (_keep, tree) = dir();
    let ran = run("seq 1 60000", &tree, &[], LIMIT).await.unwrap();
    assert!(ran.stdout.len() <= KEPT_BYTES, "{}", ran.stdout.len());
    assert!(ran.stdout.ends_with("\n60000\n"), "the end is kept");
    let shown = ran.shown();
    assert!(shown.lines().count() <= 2 * SHOWN_LINES + 2, "{shown}");
    assert!(shown.contains("60000"), "{shown}");
    let long = run("head -c 20000 /dev/zero | tr '\\0' x", &tree, &[], LIMIT)
        .await
        .unwrap();
    assert!(
        long.shown().len() <= 2 * SHOWN_BYTES + 100,
        "one long line is cut too"
    );
}

fn repo(path: &Path, fast: Option<&str>, confirmed: Option<&str>) -> Repo {
    Repo {
        id: archon_core::id::RepoId::new(),
        name: "r".into(),
        path: path.to_owned(),
        ship: ShipMode::Local,
        base_branch: None,
        default_pipeline: None,
        autonomy_max: Default::default(),
        grant_merge: false,
        attribution: Attribution::None,
        tiers: RepoTiers {
            checks: Checks {
                fast: fast.map(str::to_owned),
                full: None,
            },
            ..RepoTiers::default()
        },
        confirmed_fast_check: confirmed.map(str::to_owned),
        confirmed_full_check: None,
        added_at: archon_core::now(),
    }
}

#[test]
fn only_the_registry_s_or_a_confirmed_committed_check_is_trusted() {
    let (_keep, root) = dir();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &root,
        &["commit", "--quiet", "--allow-empty", "-m", "start"],
    );
    write(
        &root,
        ".archon/repo.toml",
        "[checks]\nfast = \"make quick\"\n",
    );
    let registry = repo(&root, Some("make mine"), None);
    assert_eq!(trusted(&registry).unwrap().as_deref(), Some("make mine"));
    let confirmed = repo(&root, None, Some("make quick"));
    assert_eq!(trusted(&confirmed).unwrap(), None, "an untracked file");
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "check"]);
    assert_eq!(
        trusted(&repo(&root, None, None)).unwrap(),
        None,
        "unconfirmed"
    );
    assert_eq!(trusted(&confirmed).unwrap().as_deref(), Some("make quick"));
    write(&root, ".archon/repo.toml", "[checks]\nfast = \"true\"\n");
    assert_eq!(
        trusted(&confirmed).unwrap().as_deref(),
        Some("make quick"),
        "an uncommitted edit counts for nothing"
    );
    let edited = repo(&root, None, Some("true"));
    assert_eq!(trusted(&edited).unwrap(), None, "nor confirms an edit");
    git(&root, &["commit", "--quiet", "-am", "changed"]);
    assert_eq!(trusted(&confirmed).unwrap(), None, "the commit changed it");
    std::fs::write(root.join(".git/config"), "[broken\n").unwrap();
    let unreadable = repo(&root, None, Some("true"));
    assert!(trusted(&unreadable).is_err(), "a failed read is no absence");
}

/// The full check is trusted as the fast one is, by its own confirmation.
#[test]
fn only_the_registry_s_or_a_confirmed_committed_full_check_is_trusted() {
    let (_keep, root) = dir();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    let file = "[checks]\nfast = \"make quick\"\nfull = \"make ci\"\n";
    write(&root, ".archon/repo.toml", file);
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "checks"]);
    let full = |registry: Option<&str>, confirmed: Option<&str>| {
        let mut repo = repo(&root, None, Some("make ci"));
        repo.tiers.checks.full = registry.map(str::to_owned);
        repo.confirmed_full_check = confirmed.map(str::to_owned);
        trusted_full(&repo).unwrap()
    };
    assert_eq!(full(Some("make all"), None).as_deref(), Some("make all"));
    assert_eq!(
        full(None, None),
        None,
        "the fast confirmation is not the full one"
    );
    assert_eq!(full(None, Some("make ci")).as_deref(), Some("make ci"));
    write(&root, ".archon/repo.toml", "[checks]\nfull = \"true\"\n");
    assert_eq!(
        full(None, Some("true")),
        None,
        "an uncommitted edit confirms nothing"
    );
    assert_eq!(full(None, Some("make ci")).as_deref(), Some("make ci"));
    git(&root, &["commit", "--quiet", "-am", "changed"]);
    assert_eq!(full(None, Some("make ci")), None, "the commit changed it");
}

fn git(tree: &Path, args: &[&str]) {
    let status = Std::new("git")
        .arg("-C")
        .arg(tree)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

#[test]
fn a_change_is_what_its_commits_add_to_their_base() {
    let (_keep, tree) = dir();
    git(&tree, &["init", "--quiet", "--initial-branch=main"]);
    write(
        &tree,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    for (name, deps) in [
        ("a", ""),
        ("b", "[dependencies]\na = { path = \"../a\" }\n"),
        ("c", ""),
    ] {
        let manifest = format!("[package]\nname = \"{name}\"\n\n{deps}");
        write(&tree, &format!("crates/{name}/Cargo.toml"), &manifest);
        write(&tree, &format!("crates/{name}/src/lib.rs"), "");
    }
    git(&tree, &["add", "."]);
    git(&tree, &["commit", "--quiet", "-m", "base"]);
    git(&tree, &["checkout", "--quiet", "-b", "change"]);
    write(&tree, "crates/a/src/lib.rs", "pub fn a() {}\n");
    git(&tree, &["commit", "--quiet", "-am", "change a"]);
    write(&tree, "crates/c/src/lib.rs", "uncommitted\n");

    let affected = affected_by(&tree, "main").unwrap();
    assert_eq!(affected.units, ["a", "b"], "committed changes only");
    assert_eq!(affected.whole, None);
    let command = affected::expand("cargo test {packages:-p}", &affected.units).unwrap();
    assert_eq!(command.as_deref(), Some("cargo test -p a -p b"));
    let missing = affected_by(&tree, "nowhere").unwrap_err().to_string();
    assert!(missing.contains("nowhere"), "{missing}");
}
