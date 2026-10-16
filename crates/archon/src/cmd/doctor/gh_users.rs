//! The GitHub login each repository runs as, when the operator named one:
//! `gh` holds its token, `origin` is a github.com repository, and the login
//! can push there. The token is read only to ask GitHub and never shown.

use std::path::Path;

use archon_core::repo::{GH_HOST, GhUser, Repo, github_slug};

use super::registry::{COMMAND_TIMEOUT, Registry, run_bounded, run_bounded_with};
use super::{Check, Level, check};

const NAME: &str = "gh users";
/// Every variable through which `gh` would pick another token or host.
const AMBIENT: [&str; 5] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
];

pub(super) async fn check_gh_users(binary: &Path, registry: &Registry) -> Check {
    let repos = match registry.repos() {
        Ok(repos) => repos,
        Err(why) => return check(NAME, Level::Warn, format!("not checked: {why}")),
    };
    let mut fine = Vec::new();
    let mut broken = Vec::new();
    for repo in repos {
        let Some(user) = &repo.gh_user else {
            continue;
        };
        match user_problem(binary, repo, user).await {
            None => fine.push(format!("{} as {user}", repo.name)),
            Some(problem) => broken.push(format!("{} runs as {user}, who {problem}", repo.name)),
        }
    }
    if !broken.is_empty() {
        return check(NAME, Level::Fail, broken.join("; "));
    }
    if fine.is_empty() {
        return check(
            NAME,
            Level::Ok,
            "every repository runs as archond's own login",
        );
    }
    check(NAME, Level::Ok, format!("can push: {}", fine.join(", ")))
}

/// Why `user` cannot push `repo`'s `origin`, or `None` when it can.
async fn user_problem(binary: &Path, repo: &Repo, user: &GhUser) -> Option<String> {
    let origin = match origin_url(&repo.path).await {
        Ok(origin) => origin,
        Err(why) => return Some(why),
    };
    let Some(slug) = github_slug(&origin) else {
        return Some(format!(
            "cannot reach `origin` ({origin}): not a {GH_HOST} repository over https or ssh"
        ));
    };
    let token = match token(binary, &repo.path, user).await {
        Ok(token) => token,
        Err(why) => return Some(why),
    };
    can_push(binary, &repo.path, &token, &slug).await.err()
}

/// Where `repo`'s `origin` takes a push.
async fn origin_url(repo: &Path) -> Result<String, String> {
    for key in ["remote.origin.pushurl", "remote.origin.url"] {
        let args = ["config", "--get", key];
        let out = run_bounded(Path::new("git"), &args, Some(repo), COMMAND_TIMEOUT).await?;
        if out.status.success() {
            let url = String::from_utf8_lossy(out.stdout.trim_ascii()).into_owned();
            assert!(!url.contains('\n'), "one URL per key");
            return Ok(url);
        }
    }
    Err("cannot push: the repository has no `origin` remote".to_owned())
}

/// The token `gh` holds for `user` on github.com. What `gh` printed is
/// never part of the error.
async fn token(binary: &Path, repo: &Path, user: &GhUser) -> Result<String, String> {
    let args = user.token_args();
    let out = run_bounded_with(binary, &args, Some(repo), COMMAND_TIMEOUT, |command| {
        for name in AMBIENT {
            command.env_remove(name);
        }
    })
    .await?;
    let token = String::from_utf8_lossy(out.stdout.trim_ascii()).into_owned();
    if !out.status.success() || token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "has no token in `gh`: `gh auth login --hostname {GH_HOST}` as {user}"
        ));
    }
    Ok(token)
}

/// Whether GitHub says the holder of `token` may push to `slug`.
async fn can_push(binary: &Path, repo: &Path, token: &str, slug: &str) -> Result<(), String> {
    assert!(!token.is_empty(), "a token read from gh");
    assert_eq!(slug.matches('/').count(), 1, "owner/name");
    let endpoint = format!("repos/{slug}");
    let args = ["api", &endpoint, "--jq", ".permissions.push"];
    let out = run_bounded_with(binary, &args, Some(repo), COMMAND_TIMEOUT, |command| {
        for name in AMBIENT {
            command.env_remove(name);
        }
        command.env("GH_TOKEN", token).env("GH_HOST", GH_HOST);
    })
    .await?;
    if !out.status.success() {
        return Err(format!(
            "cannot read {slug} on {GH_HOST}: the token is refused or the repository is not \
             visible to that login"
        ));
    }
    match out.stdout.trim_ascii() {
        b"true" => Ok(()),
        _ => Err(format!("cannot push to {slug} on {GH_HOST}")),
    }
}

#[cfg(test)]
mod tests {
    use archon_core::repo::ShipMode;
    use archon_core::testing::write_script;

    use super::super::registry::tests::{registry, repo};
    use super::*;

    const SECRET: &str = "gho_FAKEdoctorFAKE012345";

    /// A `gh` that knows `devfake`, who can push to o/r, and `reader`, who
    /// cannot; it checks that the token reached it alone.
    fn fake_gh(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
case "$*" in
"auth token --hostname github.com --user devfake") echo {SECRET} ;;
"auth token --hostname github.com --user reader") echo {SECRET}r ;;
"auth token "*) echo "no token; secret {SECRET}" >&2; exit 1 ;;
"api repos/o/r --jq .permissions.push")
    [ "$GH_HOST" = github.com ] && [ -z "$GITHUB_TOKEN" ] || exit 3
    case "$GH_TOKEN" in {SECRET}) echo true ;; {SECRET}r) echo false ;; *) exit 4 ;; esac ;;
*) echo "HTTP 404: Not Found ($GH_TOKEN)" >&2; exit 1 ;;
esac
"#
        );
        write_script(&path, &script);
        path
    }

    fn work_tree(dir: &Path, name: &str, origin: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let git = |args: &[&str]| {
            let ran = std::process::Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(args)
                .status();
            assert!(ran.unwrap().success(), "{args:?}");
        };
        std::fs::create_dir(&path).unwrap();
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", origin]);
        path
    }

    fn as_user(name: &str, path: &Path, user: Option<&str>) -> Repo {
        let mut repo = repo(name, path, ShipMode::Pr);
        repo.gh_user = user.map(|user| user.parse().unwrap());
        repo
    }

    #[tokio::test]
    async fn a_login_that_can_push_passes() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let web = work_tree(dir.path(), "web", "git@github.com:o/r.git");
        let lib = work_tree(dir.path(), "lib", "/srv/lib.git");
        let fine = registry(
            vec![
                as_user("web", &web, Some("devfake")),
                as_user("lib", &lib, None),
            ],
            false,
        );
        let passed = check_gh_users(&gh, &fine).await;
        assert_eq!(passed.level, Level::Ok, "{}", passed.detail);
        assert_eq!(passed.detail, "can push: web as devfake");
        let none = registry(vec![as_user("lib", &lib, None)], false);
        assert_eq!(check_gh_users(&gh, &none).await.level, Level::Ok);
    }

    #[tokio::test]
    async fn a_missing_or_unauthorized_login_fails_and_no_token_is_shown() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let github = work_tree(dir.path(), "github", "https://github.com/o/r.git");
        let other = work_tree(dir.path(), "other", "https://github.com/o/gone.git");
        let ghe = work_tree(dir.path(), "ghe", "https://ghe.example.com/o/r.git");
        let failing = registry(
            vec![
                as_user("missing", &github, Some("nobody")),
                as_user("readonly", &github, Some("reader")),
                as_user("hidden", &other, Some("devfake")),
                as_user("enterprise", &ghe, Some("devfake")),
            ],
            false,
        );
        let failed = check_gh_users(&gh, &failing).await;
        assert_eq!(failed.level, Level::Fail);
        let said = &failed.detail;
        assert!(
            said.contains("missing runs as nobody, who has no token in `gh`"),
            "{said}"
        );
        assert!(
            said.contains("readonly runs as reader, who cannot push to o/r"),
            "{said}"
        );
        assert!(
            said.contains("hidden runs as devfake, who cannot read o/gone"),
            "{said}"
        );
        assert!(
            said.contains("enterprise runs as devfake, who cannot reach"),
            "{said}"
        );
        assert!(!said.contains("gho_"), "{said}");
    }
}
