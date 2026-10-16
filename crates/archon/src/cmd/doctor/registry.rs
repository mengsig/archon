//! The checks that read archond's registry: `gh` is logged in when work
//! ships through GitHub, and every registered repository is a work tree.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use archon_client::ArchonClient;
use archon_core::config::AetherMode;
use archon_core::protocol::{Reply, Request};
use archon_core::repo::{Repo, ShipMode};

use super::{Check, Level, check};
use crate::connect::unexpected;
use crate::views;

/// Bounds every external command doctor runs, so a hung one cannot stall it.
pub(super) const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// What archond knows that these checks need; an error says why it is unknown.
pub(super) struct Registry {
    repos: Result<Vec<Repo>, String>,
    /// Whether Archon's GitHub watcher delivers requests for any repository.
    watcher_on: Result<bool, String>,
}

impl Registry {
    pub(super) async fn read(
        archond: &anyhow::Result<Option<ArchonClient>>,
        mode: AetherMode,
    ) -> Self {
        let client = match archond {
            Ok(Some(client)) => client,
            Ok(None) => return Self::unknown("archond is not running".to_owned()),
            Err(e) => return Self::unknown(format!("archond is unreachable: {e:#}")),
        };
        let repos = views::fetch_repos(client)
            .await
            .map_err(|e| format!("{e:#}"));
        let watcher_on = match mode {
            AetherMode::Shared => Ok(false),
            AetherMode::Dedicated => watcher_on(client).await.map_err(|e| format!("{e:#}")),
        };
        Self { repos, watcher_on }
    }

    fn unknown(why: String) -> Self {
        Self {
            repos: Err(why.clone()),
            watcher_on: Err(why),
        }
    }
}

async fn watcher_on(client: &ArchonClient) -> anyhow::Result<bool> {
    let view = match client.request(Request::GithubWatcher).await? {
        Reply::GithubWatcher(view) => view,
        other => return Err(unexpected(&other, "github_watcher")),
    };
    Ok(view.enabled && view.repositories.iter().any(|r| r.enabled))
}

/// Whether anything Archon runs needs `gh`, and what does.
#[derive(Debug, PartialEq, Eq)]
enum GhNeed {
    Needed(String),
    NotNeeded,
    Unknown(String),
}

fn gh_need(registry: &Registry) -> GhNeed {
    let repos = registry.repos.as_deref().unwrap_or_default();
    if let Some(repo) = repos.iter().find(|r| r.ship == ShipMode::Pr) {
        return GhNeed::Needed(format!("repository {} ships in pr mode", repo.name));
    }
    match (&registry.watcher_on, &registry.repos) {
        (Ok(true), _) => GhNeed::Needed("the GitHub watcher is on".to_owned()),
        (Err(why), _) | (_, Err(why)) => GhNeed::Unknown(why.clone()),
        (Ok(false), Ok(_)) => GhNeed::NotNeeded,
    }
}

pub(super) async fn check_gh(binary: &Path, registry: &Registry) -> Check {
    let login = gh_logged_in(binary).await;
    gh_verdict(binary, login, gh_need(registry))
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

fn gh_verdict(binary: &Path, login: Result<(), String>, need: GhNeed) -> Check {
    const NAME: &str = "gh";
    let Err(problem) = login else {
        return check(
            NAME,
            Level::Ok,
            format!("{} is logged in", binary.display()),
        );
    };
    assert!(!problem.is_empty(), "a failed login check says why");
    match need {
        GhNeed::Needed(what) => check(NAME, Level::Fail, format!("{problem}; {what}")),
        GhNeed::NotNeeded => check(
            NAME,
            Level::Warn,
            format!("{problem}; no repository ships in pr mode and the GitHub watcher is off"),
        ),
        GhNeed::Unknown(why) => check(
            NAME,
            Level::Warn,
            format!("{problem}; whether anything needs it is unknown: {why}"),
        ),
    }
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
pub(super) async fn run_bounded(
    program: &Path,
    args: &[&str],
    dir: Option<&Path>,
    timeout: Duration,
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
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("{shown} could not run: {e}")),
        Err(_) => Err(format!("{shown} did not finish within {timeout:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use archon_core::id::RepoId;
    use archon_core::testing::write_script;

    use super::*;

    fn repo(name: &str, path: &Path, ship: ShipMode) -> Repo {
        Repo {
            id: RepoId::new(),
            name: name.to_owned(),
            path: path.to_path_buf(),
            ship,
            base_branch: None,
            default_pipeline: None,
            autonomy_max: Default::default(),
            grant_merge: false,
            attribution: Default::default(),
            tiers: Default::default(),
            added_at: archon_core::now(),
        }
    }

    fn registry(repos: Vec<Repo>, watcher_on: bool) -> Registry {
        Registry {
            repos: Ok(repos),
            watcher_on: Ok(watcher_on),
        }
    }

    /// A fake `gh` that prints a token, as `gh auth status --show-token` would.
    fn fake_gh(dir: &Path, exit: u8) -> PathBuf {
        let path = dir.join(format!("gh-{exit}"));
        let script = format!("#!/bin/sh\necho 'Token: gho_SECRET' >&2\nexit {exit}\n");
        write_script(&path, &script);
        path
    }

    #[test]
    fn pr_repositories_and_the_watcher_need_gh() {
        let tmp = Path::new("/tmp");
        let local = || repo("local", tmp, ShipMode::Local);
        let pr = repo("web", tmp, ShipMode::Pr);
        let need = gh_need(&registry(vec![local(), pr], false));
        assert_eq!(
            need,
            GhNeed::Needed("repository web ships in pr mode".into())
        );
        let need = gh_need(&registry(vec![local()], true));
        assert_eq!(need, GhNeed::Needed("the GitHub watcher is on".into()));
        assert_eq!(gh_need(&registry(vec![local()], false)), GhNeed::NotNeeded);
        let unknown = Registry::unknown("archond is not running".into());
        assert_eq!(
            gh_need(&unknown),
            GhNeed::Unknown("archond is not running".into())
        );
    }

    #[tokio::test]
    async fn gh_logged_out_fails_only_when_needed_and_never_shows_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let (good, bad) = (fake_gh(dir.path(), 0), fake_gh(dir.path(), 1));
        let needed = registry(vec![repo("web", dir.path(), ShipMode::Pr)], false);
        let unneeded = registry(vec![repo("lib", dir.path(), ShipMode::Local)], false);
        assert_eq!(check_gh(&good, &needed).await.level, Level::Ok);
        let fail = check_gh(&bad, &needed).await;
        assert_eq!(fail.level, Level::Fail);
        assert!(
            fail.detail.contains("web ships in pr mode"),
            "{}",
            fail.detail
        );
        let warn = check_gh(&bad, &unneeded).await;
        assert_eq!(warn.level, Level::Warn);
        for c in [&fail, &warn] {
            assert!(!c.detail.contains("gho_"), "{}", c.detail);
        }
        let missing = check_gh(&dir.path().join("no-gh"), &needed).await;
        assert_eq!(missing.level, Level::Fail);
        assert!(
            missing.detail.contains("could not run"),
            "{}",
            missing.detail
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
