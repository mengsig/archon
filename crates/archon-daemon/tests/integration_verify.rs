//! Verifying the integration branch (issue #82, task AR-I3): a red head is
//! bisected over the merges since the last green commit to the one that
//! broke it, one fix item is queued for it and none while it is open, and
//! `status` shows the last green commit and the merges since. A bare
//! `origin` and a fake full check that fails while a `broken` file exists.

mod common;

use std::path::{Path, PathBuf};

use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::integration::{IntegrationHealth, IntegrationStatus};
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{Clearable, Reply, RepoPatch, RepoRef, Request, TiersPatch};
use archon_core::queue::{QueueItem, QueueState};
use archon_daemon::integration::{self, FIX_TAG};
use common::delivery::{add_origin, origin_of};
use common::{Harness, REPO_NAME, git};

/// The full check: it counts its runs and fails while `broken` exists.
fn full_check(runs: &Path) -> String {
    format!(
        "echo run >> '{}'; if [ -f broken ]; then echo 'error: the broken file is here'; exit 1; fi",
        runs.display()
    )
}

/// A harness whose `next` integrates into `main` and is verified at once;
/// the build slot's lock lives under the returned directory.
async fn verifying() -> (Harness, tempfile::TempDir) {
    let slot = tempfile::tempdir().expect("slot dir");
    let mut config = Config::default();
    config.integration.idle_after = Age::from_secs(0);
    config.integration.slot_lock = Some(slot.path().join("slot.lock"));
    let h = Harness::with_config(config).await;
    add_origin(&h);
    git(&h.repo, &["branch", "next", "main"]);
    git(&h.repo, &["push", "--quiet", "origin", "next"]);
    let tiers = TiersPatch {
        integration_branch: Clearable::Set("next".into()),
        full_check: Clearable::Set(full_check(&runs_file(&h))),
        ..TiersPatch::default()
    };
    let patch = RepoPatch {
        tiers,
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
    (h, slot)
}

fn runs_file(h: &Harness) -> PathBuf {
    h.dir
        .path()
        .canonicalize()
        .expect("root")
        .join("full-runs.log")
}

fn full_runs(h: &Harness) -> usize {
    std::fs::read_to_string(runs_file(h)).map_or(0, |runs| runs.lines().count())
}

/// Merge one branch per file into `origin`'s `next` with `--no-ff`, as a
/// pull request lands, and return each merge commit, oldest first.
fn land(h: &Harness, files: &[&str]) -> Vec<String> {
    let clone = h.dir.path().join(format!("upstream-{}", files[0]));
    let (origin, to) = (origin_of(h), clone.to_str().unwrap().to_owned());
    git(
        h.dir.path(),
        &["clone", "--quiet", origin.to_str().unwrap(), &to],
    );
    git(&clone, &["checkout", "--quiet", "next"]);
    let mut merges = Vec::new();
    for file in files {
        git(
            &clone,
            &[
                "checkout",
                "--quiet",
                "-b",
                &format!("feature-{file}"),
                "next",
            ],
        );
        std::fs::write(clone.join(file), "landed\n").unwrap();
        git(&clone, &["add", file]);
        git(&clone, &["commit", "--quiet", "-m", &format!("Add {file}")]);
        git(&clone, &["checkout", "--quiet", "next"]);
        let message = format!("Merge pull request: add {file}");
        git(
            &clone,
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "-m",
                &message,
                &format!("feature-{file}"),
            ],
        );
        merges.push(git(&clone, &["rev-parse", "HEAD"]));
    }
    git(&clone, &["push", "--quiet", "origin", "next"]);
    merges
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

/// Five merges land after a green head, the third adding the breakage: the
/// red head is bisected to the third, and one fix item names it.
#[tokio::test]
async fn a_red_head_after_five_merges_is_bisected_to_the_merge_that_broke_it() {
    let (h, _slot) = verifying().await;
    let green = git(&h.repo, &["rev-parse", "main"]);
    integration::pass(&h.ctx).await.expect("first pass");
    let first = branch_status(&h).await;
    assert_eq!(first.health, IntegrationHealth::Green, "{first:?}");
    assert_eq!(first.last_green.as_deref(), Some(green.as_str()));
    assert_eq!(full_runs(&h), 1);

    let merges = land(&h, &["one", "two", "broken", "four", "five"]);
    integration::pass(&h.ctx).await.expect("red pass");

    let red = branch_status(&h).await;
    assert_eq!(red.health, IntegrationHealth::Red, "{red:?}");
    assert_eq!(red.head.as_deref(), Some(merges[4].as_str()));
    assert_eq!(red.last_green.as_deref(), Some(green.as_str()));
    assert_eq!(red.merges_since_green, 5);
    let culprit = red.culprit.clone().expect("a culprit");
    assert!(culprit.contains(&merges[2][..8]), "{culprit}");
    assert!(culprit.contains("add broken"), "{culprit}");
    assert_eq!(
        full_runs(&h),
        1 + 3,
        "the head, then two probes of five merges"
    );

    let fixes = fix_items(&h).await;
    assert_eq!(fixes.len(), 1, "one fix item: {fixes:?}");
    let fix = &fixes[0];
    assert_eq!(red.fix_item.as_ref(), Some(&fix.id));
    assert!(fix.title.contains("add broken"), "{}", fix.title);
    assert!(
        fix.brief.contains(&merges[2]),
        "the brief names the culprit"
    );
    assert!(
        fix.brief.contains("error: the broken file is here"),
        "{}",
        fix.brief
    );
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    let told = feed.iter().any(|entry| match &entry.entry {
        FeedEntry::Notification(n) => {
            n.kind == NotificationKind::Blocker && n.item.as_ref() == Some(&fix.id)
        }
        _ => false,
    });
    assert!(told, "the operator is told of the fix item");
}

/// A red head gets one fix item, however often it is looked at and however
/// many red merges follow while the item is open; a closed one is replaced.
#[tokio::test]
async fn exactly_one_fix_item_is_queued_and_none_while_one_is_open() {
    let (h, _slot) = verifying().await;
    integration::pass(&h.ctx).await.expect("green pass");
    land(&h, &["broken"]);
    integration::pass(&h.ctx).await.expect("red pass");
    integration::pass(&h.ctx).await.expect("the same red head");
    assert_eq!(fix_items(&h).await.len(), 1);
    let runs = full_runs(&h);

    let later = land(&h, &["later"]);
    integration::pass(&h.ctx).await.expect("a later red head");
    let open = fix_items(&h).await;
    assert_eq!(open.len(), 1, "none while the fix is open: {open:?}");
    let still = branch_status(&h).await;
    assert_eq!(still.health, IntegrationHealth::Red);
    assert_eq!(still.head.as_deref(), Some(later[0].as_str()));
    assert_eq!(full_runs(&h), runs + 1, "the known culprit stands unprobed");

    let cancel = Request::CancelItem {
        item: open[0].id.clone(),
        reason: Some("superseded".into()),
    };
    h.cli(cancel).await.expect("cancel");
    land(&h, &["latest"]);
    integration::pass(&h.ctx)
        .await
        .expect("red once the fix closed");
    let fixes = fix_items(&h).await;
    assert_eq!(fixes.len(), 2, "a closed fix is replaced: {fixes:?}");
    assert!(
        fixes
            .iter()
            .any(|item| matches!(item.state, QueueState::Cancelled { .. }))
    );
}

/// `status` shows the integration branch's last green commit and the
/// merges landed since.
#[tokio::test]
async fn status_shows_the_last_green_commit_and_the_merges_since() {
    let (h, _slot) = verifying().await;
    let green = git(&h.repo, &["rev-parse", "main"]);
    integration::pass(&h.ctx).await.expect("green pass");
    let merges = land(&h, &["one", "broken"]);
    integration::pass(&h.ctx).await.expect("red pass");

    let status = match h.cli(Request::Status(Default::default())).await {
        Ok(Reply::Status(status)) => *status,
        other => panic!("status replied {other:?}"),
    };
    let branch = status.integration.first().expect("the integration branch");
    assert_eq!(branch.branch, "next");
    assert_eq!(branch.last_green.as_deref(), Some(green.as_str()));
    assert_eq!(branch.merges_since_green, 2);
    let shown = branch.describe();
    let expected = format!(
        "red at {}; last green {}, 2 merge(s) since",
        &merges[1][..8],
        &green[..8]
    );
    assert!(shown.contains(&expected), "{shown}");
}
