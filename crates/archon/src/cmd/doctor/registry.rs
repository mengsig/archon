//! The checks that read archond's registry: `gh` is logged in when work
//! ships through GitHub, and every registered repository is a work tree.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::repo::Repo;

use super::{Check, Level, check};
use crate::views;

/// Bounds every external command doctor runs, so a hung one cannot stall it.
pub(super) const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// What archond knows that these checks need; an error says why it is unknown.
pub(super) struct Registry {
    repos: Result<Vec<Repo>, String>,
}

impl Registry {
    /// The registered repositories, or why they are unknown.
    pub(super) fn repos(&self) -> Result<&[Repo], &str> {
        self.repos.as_deref().map_err(String::as_str)
    }

    pub(super) async fn read(archond: &anyhow::Result<Option<ArchonClient>>) -> Self {
        let client = match archond {
            Ok(Some(client)) => client,
            Ok(None) => return Self::unknown("archond is not running".to_owned()),
            Err(e) => return Self::unknown(format!("archond is unreachable: {e:#}")),
        };
        let repos = views::fetch_repos(client)
            .await
            .map_err(|e| format!("{e:#}"));
        Self { repos }
    }

    fn unknown(why: String) -> Self {
        Self { repos: Err(why) }
    }
}

pub(super) async fn check_gh(binary: &Path) -> Check {
    use archon_core::github::{GH_INSTALL_HINT, MIN_GH, gh_version};
    let version = match run_bounded(binary, &["--version"], None, COMMAND_TIMEOUT).await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => {
            return check(
                "gh",
                Level::Fail,
                format!(
                    "`{} --version` exited with {}; {GH_INSTALL_HINT}",
                    binary.display(),
                    out.status
                ),
            );
        }
        Err(error) => return check("gh", Level::Fail, format!("{error}; {GH_INSTALL_HINT}")),
    };
    if !gh_version(&version).is_some_and(|v| v >= MIN_GH) {
        let (major, minor, patch) = MIN_GH;
        return check(
            "gh",
            Level::Fail,
            format!(
                "{version:?}: Archon needs gh {major}.{minor}.{patch} or later; {GH_INSTALL_HINT}"
            ),
        );
    }
    match gh_logged_in(binary).await {
        Ok(()) => check("gh", Level::Ok, format!("{version}; logged in")),
        Err(problem) => check("gh", Level::Fail, problem),
    }
}

/// `gh auth status`. Its output is never shown: it can carry a token.
async fn gh_logged_in(binary: &Path) -> Result<(), String> {
    let output = run_bounded(binary, &["auth", "status"], None, COMMAND_TIMEOUT).await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`{} auth status` exited with {}; run it to see why, or `gh auth login`",
        binary.display(),
        output.status
    ))
}

pub(super) async fn check_repos(registry: &Registry) -> Check {
    const NAME: &str = "repositories";
    let repos = match &registry.repos {
        Ok(repos) => repos,
        Err(why) => return check(NAME, Level::Warn, format!("not checked: {why}")),
    };
    let mut broken = Vec::new();
    for repo in repos {
        if let Some(problem) = work_tree_problem(&repo.path).await {
            broken.push(format!("{} ({}) {problem}", repo.name, repo.path.display()));
        }
    }
    if broken.is_empty() {
        let detail = format!("{} registered, all git work trees", repos.len());
        return check(NAME, Level::Ok, detail);
    }
    let detail = format!(
        "{}; restore them or `archon repo remove <name>`",
        broken.join("; ")
    );
    check(NAME, Level::Fail, detail)
}

/// Why `path` is not the top level of a git work tree, or `None` when it is.
async fn work_tree_problem(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return Some("is not an absolute path".to_owned());
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Some("is not a directory".to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some("is missing".to_owned()),
        Err(e) => return Some(format!("cannot be read: {e}")),
    }
    let args = ["rev-parse", "--show-toplevel"];
    let out = match run_bounded(Path::new("git"), &args, Some(path), COMMAND_TIMEOUT).await {
        Ok(out) if out.status.success() => out,
        Ok(_) => return Some("is not a git work tree".to_owned()),
        Err(e) => return Some(e),
    };
    // A directory inside some other repository is not a registered work tree.
    let top = String::from_utf8_lossy(out.stdout.trim_ascii()).into_owned();
    match (std::fs::canonicalize(path), std::fs::canonicalize(&top)) {
        (Ok(own), Ok(top)) if own == top => None,
        (Ok(_), Ok(top)) => Some(format!("is not a git work tree (inside {})", top.display())),
        (Err(e), _) | (_, Err(e)) => Some(format!("cannot be resolved: {e}")),
    }
}

/// Runs `program args` in `dir`, killed once `timeout` passes.
pub(in crate::cmd) async fn run_bounded(
    program: &Path,
    args: &[&str],
    dir: Option<&Path>,
    timeout: Duration,
) -> Result<Output, String> {
    run_bounded_with(program, args, dir, timeout, |_| {}).await
}

/// [`run_bounded`], with the command's environment changed by `env` first.
pub(super) async fn run_bounded_with(
    program: &Path,
    args: &[&str],
    dir: Option<&Path>,
    timeout: Duration,
    env: impl FnOnce(&mut tokio::process::Command),
) -> Result<Output, String> {
    assert!(
        !timeout.is_zero(),
        "a zero timeout never lets a command run"
    );
    let shown = format!("`{} {}`", program.display(), args.join(" "));
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .kill_on_drop(true);
    if let Some(dir) = dir {
        assert!(dir.is_absolute(), "{} is not absolute", dir.display());
        command.current_dir(dir);
    }
    env(&mut command);
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("{shown} could not run: {e}")),
        Err(_) => Err(format!("{shown} did not finish within {timeout:?}")),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use archon_core::id::RepoId;
    use archon_core::repo::ShipMode;
    use archon_core::testing::write_script;

    use super::*;

    pub(in crate::cmd::doctor) fn repo(name: &str, path: &Path, ship: ShipMode) -> Repo {
        Repo {
            id: RepoId::new(),
            name: name.to_owned(),
            path: path.to_path_buf(),
            ship,
            base_branch: None,
            default_pipeline: None,
            autonomy_max: Default::default(),
            grant_merge: false,
            merge_waive_checks: Vec::new(),
            attribution: Default::default(),
            gh_user: None,
            tiers: Default::default(),
            confirmed_fast_check: None,
            confirmed_full_check: None,
            confirmed_release_check: None,
            added_at: archon_core::now(),
        }
    }

    pub(in crate::cmd::doctor) fn registry(repos: Vec<Repo>, _watcher_on: bool) -> Registry {
        Registry { repos: Ok(repos) }
    }

    #[tokio::test]
    async fn gh_check_requires_supported_version_and_login_without_exposing_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good");
        write_script(
            &good,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'gh version 2.63.0 (2024-05-29)'; else exit 0; fi\n",
        );
        assert_eq!(check_gh(&good).await.level, Level::Ok);
        let old = dir.path().join("old");
        write_script(&old, "#!/bin/sh\necho 'gh version 2.45.0 (Ubuntu)'\n");
        let fail = check_gh(&old).await;
        assert_eq!(fail.level, Level::Fail);
        assert!(fail.detail.contains("~/.local/bin"));
        let logged_out = dir.path().join("logged-out");
        write_script(
            &logged_out,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'gh version 2.63.0'; else echo gho_SECRET >&2; exit 1; fi\n",
        );
        let fail = check_gh(&logged_out).await;
        assert_eq!(fail.level, Level::Fail);
        assert!(!fail.detail.contains("gho_SECRET"));
        assert_eq!(
            check_gh(&dir.path().join("missing")).await.level,
            Level::Fail
        );
    }

    #[tokio::test]
    async fn missing_and_non_git_repositories_are_named_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let (git, plain) = (dir.path().join("git"), dir.path().join("plain"));
        std::fs::create_dir(&plain).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&git)
            .status();
        assert!(init.unwrap().success());
        let ok = registry(vec![repo("good", &git, ShipMode::Pr)], false);
        let passed = check_repos(&ok).await;
        assert_eq!(passed.level, Level::Ok, "{}", passed.detail);
        let gone = dir.path().join("gone");
        let nested = git.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let bad = registry(
            vec![
                repo("good", &git, ShipMode::Pr),
                repo("gone", &gone, ShipMode::Pr),
                repo("plain", &plain, ShipMode::Local),
                repo("nested", &nested, ShipMode::Local),
            ],
            false,
        );
        let failed = check_repos(&bad).await;
        assert_eq!(failed.level, Level::Fail);
        assert!(
            failed
                .detail
                .contains(&format!("gone ({}) is missing", gone.display()))
        );
        assert!(failed.detail.contains("plain ("), "{}", failed.detail);
        assert!(
            failed.detail.contains("not a git work tree"),
            "{}",
            failed.detail
        );
        assert!(!failed.detail.contains("good ("), "{}", failed.detail);
        assert!(failed.detail.contains("nested ("), "{}", failed.detail);
        assert!(failed.detail.contains("(inside "), "{}", failed.detail);
        assert!(
            failed.detail.contains("archon repo remove"),
            "{}",
            failed.detail
        );
        let unknown = Registry::unknown("archond is not running".into());
        assert_eq!(check_repos(&unknown).await.level, Level::Warn);
    }

    #[tokio::test]
    async fn a_hung_command_is_cut_off_at_its_timeout() {
        let started = std::time::Instant::now();
        let limit = Duration::from_millis(200);
        let result = run_bounded(Path::new("sleep"), &["30"], None, limit).await;
        let error = result.expect_err("sleep 30 outlives the timeout");
        assert!(error.contains("did not finish"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
}
