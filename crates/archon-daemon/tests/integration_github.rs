//! Verifying the integration branch by its GitHub checks (issue #82, task
//! AR-I3): running checks show as verifying, and a head they judged is read
//! again, so a rerun that turns it is followed. A fake `gh` serves the
//! checks; the operator's git config, here `XDG_CONFIG_HOME`, reaches the
//! GitHub `origin` at a local bare repository. Each test runs in a child
//! process holding that environment.

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
/// The directory of the fake `gh`, its checks and the operator's git config.
const TOOLS_ENV: &str = "ARCHON_TEST_GITHUB_TOOLS";

/// A fake `gh` answering a commit's check runs with the text of
/// `<commit>.json` beside `checks`, else with `checks`' text.
fn fake_gh(dir: &Path, checks: &Path) -> PathBuf {
    let own = format!("{}/$sha.json", checks.parent().unwrap().display());
    let script = format!(
        "#!/bin/sh\ncase \"$4\" in\n*/check-runs*)\n\
         sha=$(echo \"$4\" | sed 's|.*/commits/\\([0-9a-f]*\\)/.*|\\1|')\n\
         if [ -f \"{own}\" ]; then cat \"{own}\"; else cat '{}'; fi;;\n\
         *) exit 1;;\nesac\n",
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

/// Run the ignored test `inner` in a child process under the operator's
/// git config and the fake `gh`.
fn run_inner(inner: &str) {
    let tools = tempfile::tempdir().expect("tools dir");
    let tools = tools.path().canonicalize().unwrap();
    let status = Command::new(std::env::current_exe().expect("the test binary"))
        .args([inner, "--exact", "--ignored", "--quiet"])
        .env("XDG_CONFIG_HOME", &tools)
        .env(TOOLS_ENV, &tools)
        .status()
        .expect("the inner test runs");
    assert!(status.success(), "the inner test {inner} failed");
}

/// The tools directory a test runs with, its harness and its checks file.
async fn inner_harness() -> (Harness, PathBuf, PathBuf) {
    let tools = PathBuf::from(std::env::var_os(TOOLS_ENV).expect("run with the tools"));
    assert_eq!(
        std::env::var_os("XDG_CONFIG_HOME"),
        Some(tools.clone().into())
    );
    let checks = tools.join("checks.json");
    let h = on_github(&tools, &checks).await;
    (h, tools, checks)
}

/// GitHub's checks decide, show as verifying while they run, and are read
/// again on a head they judged: a rerun turns it green, a later one red,
/// which revokes it as the baseline a later merge is blamed against.
#[test]
fn github_checks_are_followed_through_reruns_on_the_same_head() {
    run_inner("github_checks_under_the_operator_s_git_config");
}

/// A head that turns green again is not blamed by the bisection its red
/// verdict owed, whether that bisection had not begun or waits on a probe.
#[test]
fn a_head_rerun_green_ends_the_bisection_it_owed() {
    run_inner("a_rerun_green_head_under_the_operator_s_git_config");
}

/// Run only by [`github_checks_are_followed_through_reruns_on_the_same_head`].
#[tokio::test]
#[ignore = "run by github_checks_are_followed_through_reruns_on_the_same_head"]
async fn github_checks_under_the_operator_s_git_config() {
    let (h, _tools, checks) = inner_harness().await;
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

/// Run only by [`a_head_rerun_green_ends_the_bisection_it_owed`].
#[tokio::test]
#[ignore = "run by a_head_rerun_green_ends_the_bisection_it_owed"]
async fn a_rerun_green_head_under_the_operator_s_git_config() {
    let (h, tools, checks) = inner_harness().await;
    let first = land(&h, "first");
    let green = look(&h, &checks, "completed", Some("success")).await;
    assert_eq!(green.last_green.as_deref(), Some(first.as_str()));

    let red = look(&h, &checks, "completed", Some("failure")).await;
    assert_eq!(red.health, IntegrationHealth::Red, "{red:?}");
    assert_eq!(red.last_green, None, "the green baseline is revoked");
    let again = look(&h, &checks, "completed", Some("success")).await;
    assert_eq!(again.health, IntegrationHealth::Green, "{again:?}");
    assert_eq!(again.last_green.as_deref(), Some(first.as_str()));
    assert_eq!(again.culprit, None, "a green head blames no merge");
    assert!(
        fix_items(&h).await.is_empty(),
        "a green head is not repaired"
    );

    let second = land(&h, "second");
    let third = land(&h, "third");
    set_check(&tools.join(format!("{second}.json")), "in_progress", None);
    let waiting = look(&h, &checks, "completed", Some("failure")).await;
    assert_eq!(waiting.head.as_deref(), Some(third.as_str()));
    assert_eq!(waiting.health, IntegrationHealth::Red, "{waiting:?}");
    assert!(
        fix_items(&h).await.is_empty(),
        "the bisection waits on a probe"
    );
    let recovered = look(&h, &checks, "completed", Some("success")).await;
    assert_eq!(recovered.health, IntegrationHealth::Green, "{recovered:?}");
    assert_eq!(recovered.last_green.as_deref(), Some(third.as_str()));
    assert_eq!(recovered.culprit, None);
    assert!(
        fix_items(&h).await.is_empty(),
        "a green head is not repaired"
    );
}
