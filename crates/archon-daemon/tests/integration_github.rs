//! Verifying the integration branch by its GitHub checks (issue #82, task
//! AR-I3): running checks show as verifying, and a head they judged is read
//! again, so a rerun that turns it is followed. A fake `gh` serves the
//! checks; the operator's git config, here `XDG_CONFIG_HOME`, reaches the
//! GitHub `origin` at a local bare repository.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::integration::{IntegrationHealth, IntegrationStatus};
use archon_core::protocol::{Clearable, RepoPatch, RepoRef, Request, TiersPatch};
use archon_core::queue::QueueItem;
use archon_daemon::integration::{self, FIX_TAG};
use common::delivery::{add_origin, origin_of};
use common::{Harness, REPO_NAME, git};

const GITHUB_URL: &str = "https://github.com/owner/project.git";
const INNER: &str = "github_checks_under_the_operator_s_git_config";
/// The directory of the fake `gh`, its checks and the operator's git config.
const TOOLS_ENV: &str = "ARCHON_TEST_GITHUB_TOOLS";

/// A fake `gh` answering every commit's check runs with `checks`' text.
fn fake_gh(dir: &Path, checks: &Path) -> PathBuf {
    let script = format!(
        "#!/bin/sh\ncase \"$4\" in\n*/check-runs*) cat '{}';;\n*) exit 1;;\nesac\n",
        checks.display()
    );
    let gh = dir.join("gh");
    archon_core::testing::write_script(&gh, &script);
    gh
}

/// One `test` check run in `status`, with `conclusion` once completed.
fn set_check(checks: &Path, status: &str, conclusion: Option<&str>) {
    let conclusion = conclusion.map_or("null".to_owned(), |c| format!("\"{c}\""));
    let run = format!(r#"{{"id":1,"name":"test","status":"{status}","conclusion":{conclusion}}}"#);
    let page = format!(r#"{{"total_count":1,"check_runs":[{run}]}}"#);
    std::fs::write(checks, page).unwrap();
}

/// Git, as the operator configured it, reaches [`GITHUB_URL`] at `origin`.
fn reach_github_at(config_home: &Path, origin: &Path) {
    let config = config_home.join("git");
    std::fs::create_dir_all(&config).unwrap();
    let text = format!(
        "[url \"{}\"]\n\tinsteadOf = {GITHUB_URL}\n",
        origin.display()
    );
    std::fs::write(config.join("config"), text).unwrap();
}

/// A harness whose `next` integrates into `main`, `origin` being on GitHub
/// and the fake `gh` reading `checks`.
async fn on_github(tools: &Path, checks: &Path) -> Harness {
    let mut config = Config::default();
    config.integration.idle_after = Age::from_secs(0);
    config.integration.slot_lock = Some(tools.join("slot.lock"));
    config.github.binary = fake_gh(tools, checks);
    let h = Harness::with_config(config).await;
    add_origin(&h);
    git(&h.repo, &["branch", "next", "main"]);
    git(&h.repo, &["push", "--quiet", "origin", "next"]);
    reach_github_at(tools, &origin_of(&h));
    git(&h.repo, &["remote", "set-url", "origin", GITHUB_URL]);
    let patch = RepoPatch {
        tiers: TiersPatch {
            integration_branch: Clearable::Set("next".into()),
            ..TiersPatch::default()
        },
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
    h
}

async fn branch_status(h: &Harness) -> IntegrationStatus {
    let mut branches = integration::status(&h.ctx).await.expect("status");
    assert_eq!(branches.len(), 1, "one integration branch: {branches:?}");
    branches.remove(0)
}

async fn fix_items(h: &Harness) -> Vec<QueueItem> {
    let mut items = h.ctx.store.lock().await.items(&Default::default()).unwrap();
    items.retain(|item| item.tags.iter().any(|tag| tag == FIX_TAG));
    items
}

/// Merge a branch adding `file` into `origin`'s `next`, as a pull request
/// lands, and return the merge commit.
fn land(h: &Harness, file: &str) -> String {
    let clone = h.dir.path().join(format!("upstream-{file}"));
    let origin = origin_of(h);
    let (from, to) = (origin.to_str().unwrap(), clone.to_str().unwrap());
    git(
        h.dir.path(),
        &["clone", "--quiet", "--branch", "next", from, to],
    );
    git(&clone, &["checkout", "--quiet", "-b", file]);
    std::fs::write(clone.join(file), "landed\n").unwrap();
    git(&clone, &["add", file]);
    git(&clone, &["commit", "--quiet", "-m", &format!("Add {file}")]);
    git(&clone, &["checkout", "--quiet", "next"]);
    let message = format!("Merge pull request: add {file}");
    git(
        &clone,
        &["merge", "--quiet", "--no-ff", "-m", &message, file],
    );
    git(&clone, &["push", "--quiet", from, "next"]);
    git(&clone, &["rev-parse", "HEAD"])
}

async fn look(
    h: &Harness,
    checks: &Path,
    status: &str,
    conclusion: Option<&str>,
) -> IntegrationStatus {
    set_check(checks, status, conclusion);
    integration::pass(&h.ctx).await.expect("a pass");
    branch_status(h).await
}

/// GitHub's checks decide, show as verifying while they run, and are read
/// again on a head they judged: a rerun turns it green, a later one red,
/// which revokes it as the baseline a later merge is blamed against.
#[test]
fn github_checks_are_followed_through_reruns_on_the_same_head() {
    let tools = tempfile::tempdir().expect("tools dir");
    let tools = tools.path().canonicalize().unwrap();
    let status = Command::new(std::env::current_exe().expect("the test binary"))
        .args([INNER, "--exact", "--ignored", "--quiet"])
        .env("XDG_CONFIG_HOME", &tools)
        .env(TOOLS_ENV, &tools)
        .status()
        .expect("the inner test runs");
    assert!(status.success(), "the inner test failed");
}

/// Run only by the test above, under its git config.
#[tokio::test]
#[ignore = "run by github_checks_are_followed_through_reruns_on_the_same_head"]
async fn github_checks_under_the_operator_s_git_config() {
    let tools = PathBuf::from(std::env::var_os(TOOLS_ENV).expect("run with the tools"));
    assert_eq!(
        std::env::var_os("XDG_CONFIG_HOME"),
        Some(tools.clone().into())
    );
    let checks = tools.join("checks.json");
    let h = on_github(&tools, &checks).await;
    let head = git(&h.repo, &["rev-parse", "next"]);

    let running = look(&h, &checks, "in_progress", None).await;
    assert_eq!(running.health, IntegrationHealth::Verifying, "{running:?}");
    assert_eq!(running.head.as_deref(), Some(head.as_str()));

    let red = look(&h, &checks, "completed", Some("failure")).await;
    assert_eq!(red.health, IntegrationHealth::Red, "{red:?}");
    assert_eq!(fix_items(&h).await.len(), 1);

    let rerun = look(&h, &checks, "in_progress", None).await;
    assert_eq!(rerun.health, IntegrationHealth::Verifying, "{rerun:?}");

    let green = look(&h, &checks, "completed", Some("success")).await;
    assert_eq!(green.health, IntegrationHealth::Green, "{green:?}");
    assert_eq!(green.last_green.as_deref(), Some(head.as_str()));

    let red_again = look(&h, &checks, "completed", Some("failure")).await;
    assert_eq!(red_again.health, IntegrationHealth::Red, "{red_again:?}");
    assert_eq!(red_again.last_green, None, "the green baseline is revoked");
    let fixes = fix_items(&h).await;
    assert_eq!(fixes.len(), 1, "none while the fix is open: {fixes:?}");

    integration::pass(&h.ctx).await.expect("the owed bisection");
    let harmless = land(&h, "harmless");
    integration::pass(&h.ctx)
        .await
        .expect("a red merge on a red base");
    let after = branch_status(&h).await;
    assert_eq!(after.head.as_deref(), Some(harmless.as_str()));
    assert_eq!(after.health, IntegrationHealth::Red, "{after:?}");
    assert_eq!(
        after.culprit, None,
        "the merge is not blamed for its red base"
    );
    assert_eq!(fix_items(&h).await.len(), 1);
}
