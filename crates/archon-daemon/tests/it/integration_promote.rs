//! Promoting the integration branch (issue #82, task AR-I4): a green `next`
//! merges into `main` through its pull request and is fast-forwarded to it;
//! a red, verifying or unverified one is refused with why; without merge
//! permission promotion stops at the pull request and asks the operator. A
//! bare `origin` and a `gh` that merges on it as GitHub would.

use crate::common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use archon_core::age::Age;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::integration::{IntegrationHealth, Promotion, PromotionOutcome};
use archon_core::notify::{FeedEntry, NotificationKind};
use archon_core::protocol::{Clearable, Reply, RepoPatch, RepoRef, Request, TiersPatch};
use archon_daemon::integration;
use common::delivery::{add_origin, origin_of, pushed};
use common::{Harness, REPO_NAME, git};

/// A `gh` keeping the one pull request in `gh-state/pr` (`<base> <head>`):
/// `pr view` shows `origin`'s head of it, and `pr merge` merges it there
/// with a merge commit, as GitHub does, only at the commit it names. With
/// `gh-state/queue` it only queues the merge, which `queue land` lands.
/// Labels are created and set as asked; an edited body is `edited-body`.
const MERGING_GH: &str = r#"#!/bin/sh
state="$(dirname "$0")/gh-state"
origin="$(cat "$state/origin")"
printf '%s\n' "$*" >> "$state/calls"
url=https://github.com/o/r/pull/9
land() {
    work="$state/merge-work"
    rm -rf "$work"
    git clone --quiet "$origin" "$work" && cd "$work" || exit 1
    git checkout --quiet "$base" &&
    git -c user.name=GitHub -c user.email=gh@github.invalid merge --quiet --no-ff -m "Merge pull request #9 from $head" "origin/$head" &&
    git push --quiet origin "$base" || exit 1
    touch "$state/merged"
}
case "$1 $2" in
"pr list")
    [ -f "$state/pr" ] && [ ! -f "$state/merged" ] || { printf '[]'; exit 0; }
    read -r base head < "$state/pr"
    [ "$4" = "$head" ] || { printf '[]'; exit 0; }
    printf '[{"url":"%s","number":9,"headRefName":"%s","state":"OPEN","baseRefName":"%s"}]' "$url" "$head" "$base"
    ;;
"pr create")
    [ "$3 $5 $7 $9 ${10}" = "--base --head --title --body-file -" ] || exit 2
    printf '%s %s\n' "$4" "$6" > "$state/pr"
    cat > "$state/body"
    printf '%s\n' "$url"
    ;;
"pr view")
    read -r base head < "$state/pr"
    shown=OPEN
    [ -f "$state/merged" ] && shown=MERGED
    printf '{"state":"%s","isDraft":false,"headRefOid":"%s","baseRefName":"%s","mergeable":"MERGEABLE","statusCheckRollup":[],"mergedBy":null,"autoMergeRequest":null}' \
        "$shown" "$(git --git-dir="$origin" rev-parse "$head")" "$base"
    ;;
"pr merge")
    [ "$# $4 $6" = "6 --match-head-commit --merge" ] || exit 2
    read -r base head < "$state/pr"
    [ "$5" = "$(git --git-dir="$origin" rev-parse "$head")" ] || { echo "Head branch was modified" >&2; exit 1; }
    [ -f "$state/queue" ] && { touch "$state/queued"; exit 0; }
    land
    ;;
"queue land")
    read -r base head < "$state/pr"
    land
    ;;
"label list") printf '[]' ;;
"label create") ;;
"pr edit")
    if [ "$4" = "--body-file" ]; then cat > "$state/edited-body"; fi
    ;;
*)
    exit 2
    ;;
esac
"#;

struct Gh {
    state: PathBuf,
}

impl Gh {
    fn install(h: &Harness) -> Self {
        let binary = &h.ctx.config.github.binary;
        let state = binary.parent().expect("beside the binary").join("gh-state");
        std::fs::create_dir_all(&state).expect("gh state");
        std::fs::write(state.join("origin"), origin_of(h).to_str().unwrap()).unwrap();
        std::fs::write(binary, MERGING_GH).expect("gh script");
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).expect("mode");
        Self { state }
    }

    fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.state.join("calls")).unwrap_or_default();
        calls.lines().map(str::to_owned).collect()
    }

    fn queue_merges(&self) {
        std::fs::write(self.state.join("queue"), "").expect("queue on");
    }

    /// The merge queue lets the queued pull request in.
    fn land_queued(&self, binary: &std::path::Path) {
        assert!(self.state.join("queued").exists(), "a merge was queued");
        let landed = std::process::Command::new(binary)
            .args(["queue", "land"])
            .output()
            .expect("run gh");
        assert!(landed.status.success(), "{landed:?}");
    }

    fn merged(&self) -> bool {
        self.state.join("merged").exists()
    }
}

/// A harness whose `next` integrates into `main`, verified at once by a full
/// check that counts its runs and fails while a `broken` file exists; the
/// release hook records the commit it ran at.
async fn promoting() -> (Harness, Gh, tempfile::TempDir) {
    let slot = tempfile::tempdir().expect("slot dir");
    let mut config = Config::default();
    config.integration.idle_after = Age::from_secs(0);
    config.integration.slot_lock = Some(slot.path().join("slot.lock"));
    let h = Harness::with_config(config).await;
    add_origin(&h);
    git(&h.repo, &["branch", "next", "main"]);
    git(&h.repo, &["push", "--quiet", "origin", "next"]);
    let full = format!(
        "echo run >> '{}'; [ ! -f broken ]",
        file(&h, "full-runs.log").display()
    );
    let hook = format!("git rev-parse HEAD > '{}'", file(&h, "released").display());
    let tiers = TiersPatch {
        integration_branch: Clearable::Set("next".into()),
        full_check: Clearable::Set(full),
        release_hook: Clearable::Set(hook),
        ..TiersPatch::default()
    };
    update(
        &h,
        RepoPatch {
            tiers,
            ..RepoPatch::default()
        },
    )
    .await;
    let gh = Gh::install(&h);
    (h, gh, slot)
}

async fn update(h: &Harness, patch: RepoPatch) {
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

fn file(h: &Harness, name: &str) -> PathBuf {
    h.dir.path().canonicalize().expect("root").join(name)
}

fn full_runs(h: &Harness) -> usize {
    let runs = std::fs::read_to_string(file(h, "full-runs.log"));
    runs.map_or(0, |runs| runs.lines().count())
}

/// Land one merge of a branch adding `name` on `origin`'s `next`; returns it.
fn land(h: &Harness, name: &str) -> String {
    land_file(h, name, name, "landed\n")
}

/// Land one merge, named `name`, of a branch writing `text` to `path`.
fn land_file(h: &Harness, name: &str, path: &str, text: &str) -> String {
    let clone = h.dir.path().join(format!("upstream-{name}"));
    let origin = origin_of(h);
    let to = clone.to_str().unwrap();
    git(
        h.dir.path(),
        &["clone", "--quiet", origin.to_str().unwrap(), to],
    );
    git(
        &clone,
        &["checkout", "--quiet", "-b", "feature", "origin/next"],
    );
    let written = clone.join(path);
    std::fs::create_dir_all(written.parent().unwrap()).unwrap();
    std::fs::write(written, text).unwrap();
    git(&clone, &["add", path]);
    git(&clone, &["commit", "--quiet", "-m", &format!("Add {name}")]);
    git(
        &clone,
        &["checkout", "--quiet", "-B", "next", "origin/next"],
    );
    let message = format!("Merge pull request: add {name}");
    git(
        &clone,
        &["merge", "--quiet", "--no-ff", "-m", &message, "feature"],
    );
    git(&clone, &["push", "--quiet", "origin", "next"]);
    git(&clone, &["rev-parse", "HEAD"])
}

fn promote() -> Request {
    Request::Promote {
        repo: RepoRef(REPO_NAME.into()),
    }
}

fn promotion(reply: Result<Reply, ArchonError>) -> Promotion {
    match reply {
        Ok(Reply::Promotion(promotion)) => *promotion,
        other => panic!("promote replied {other:?}"),
    }
}

fn refusal(reply: Result<Reply, ArchonError>) -> String {
    match reply {
        Err(error @ ArchonError::InvalidTransition(_)) => error.to_string(),
        other => panic!("promote was not refused: {other:?}"),
    }
}

async fn health(h: &Harness) -> IntegrationHealth {
    let branches = integration::status(&h.ctx).await.expect("status");
    assert_eq!(branches.len(), 1, "{branches:?}");
    branches[0].health
}

async fn told(h: &Harness, kind: NotificationKind, words: &str) -> bool {
    let feed = h.ctx.store.lock().await.feed_after(0, 10_000).unwrap();
    feed.iter().any(|entry| match &entry.entry {
        FeedEntry::Notification(n) => n.kind == kind && n.text.contains(words),
        _ => false,
    })
}

/// A green `next` merges into `main` with a merge commit, `next` is
/// fast-forwarded to it, the hook runs at it, and it is not verified again.
#[tokio::test]
async fn a_green_integration_branch_merges_into_the_release_branch_and_is_realigned() {
    let (h, gh, _slot) = promoting().await;
    let landed = land(&h, "one");
    integration::pass(&h.ctx).await.expect("green pass");
    assert_eq!(health(&h).await, IntegrationHealth::Green);
    let runs = full_runs(&h);

    let done = promotion(h.cli(promote()).await);
    let PromotionOutcome::Merged {
        release_head,
        realign_problem,
        hook,
    } = &done.outcome
    else {
        panic!("not merged: {done:?}");
    };
    let origin = origin_of(&h);
    assert_eq!(
        pushed(&origin, "main").as_deref(),
        Some(release_head.as_str())
    );
    let parents = git(&origin, &["rev-list", "--parents", "-n", "1", release_head]);
    assert!(
        parents.ends_with(&landed),
        "a merge commit of the head: {parents}"
    );
    assert_eq!(realign_problem, &None);
    assert_eq!(
        pushed(&origin, "next").as_deref(),
        Some(release_head.as_str())
    );
    let hook = hook.as_ref().expect("the release hook ran");
    assert!(hook.passed, "{hook:?}");
    let released = std::fs::read_to_string(file(&h, "released")).expect("hook output");
    assert_eq!(released.trim(), release_head, "the hook ran at the merge");
    assert_eq!(
        done.pull_request.as_deref(),
        Some("https://github.com/o/r/pull/9")
    );
    let calls = gh.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("pr create --base main --head next")),
        "{calls:?}"
    );
    let labelled = "pr edit https://github.com/o/r/pull/9 --add-label risk: high";
    assert!(calls.iter().any(|c| c == labelled), "{calls:?}");
    let body = std::fs::read_to_string(gh.state.join("edited-body")).expect("risk section");
    assert!(
        body.starts_with("<!-- archon-risk -->\n## Risk: high\n"),
        "{body}"
    );
    assert!(
        body.contains("Archon labels every promotion `high`."),
        "{body}"
    );
    let risk = done.risk.as_deref().unwrap_or("");
    assert!(risk.starts_with("RISK: high, labelled"), "{done:?}");
    assert!(told(&h, NotificationKind::Outcome, "promoted demo/next").await);

    integration::pass(&h.ctx)
        .await
        .expect("pass after promotion");
    assert_eq!(health(&h).await, IntegrationHealth::Green);
    assert_eq!(
        full_runs(&h),
        runs,
        "the merge commit's tree was verified already"
    );
    let again = promotion(h.cli(promote()).await);
    assert!(
        matches!(
            again.outcome,
            PromotionOutcome::UpToDate {
                realign_problem: None
            }
        ),
        "{again:?}"
    );
}

/// A red head, a verifying one and one the full check has not seen yet are
/// refused with why, and no pull request is opened.
#[tokio::test]
async fn a_red_verifying_or_unverified_branch_is_refused_with_the_reason() {
    let (h, gh, _slot) = promoting().await;
    integration::pass(&h.ctx).await.expect("green pass");
    let unseen = land(&h, "one");
    let moved = refusal(h.cli(promote()).await);
    assert!(
        moved.contains(&format!("moved to {}", &unseen[..8])),
        "{moved}"
    );

    land(&h, "broken");
    integration::pass(&h.ctx).await.expect("red pass");
    let red = refusal(h.cli(promote()).await);
    assert!(red.contains("`next` is red: demo/next: red at"), "{red}");
    assert!(red.contains("broken by"), "names the culprit: {red}");

    let key = {
        let store = h.ctx.store.lock().await;
        format!("integration_{}", store.repos().unwrap()[0].id)
    };
    let mut store = h.ctx.store.lock().await;
    let mut state: serde_json::Value =
        serde_json::from_str(&store.kv_get(&key).unwrap().expect("a state")).unwrap();
    state["verifying"] = state["head"].clone();
    store.kv_set(&key, &state.to_string()).unwrap();
    drop(store);
    let verifying = refusal(h.cli(promote()).await);
    assert!(verifying.contains("`next` is verifying"), "{verifying}");

    assert!(
        gh.calls().iter().all(|c| !c.starts_with("pr ")),
        "{:?}",
        gh.calls()
    );
    assert!(!gh.merged());
}

/// The orchestrator without `grant_merge` gets the pull request opened and
/// the operator asked; under `grant_merge` it merges.
#[tokio::test]
async fn without_merge_permission_promotion_stops_at_the_pull_request() {
    let (h, gh, _slot) = promoting().await;
    land(&h, "one");
    integration::pass(&h.ctx).await.expect("green pass");
    let main = pushed(&origin_of(&h), "main");

    let asked = promotion(h.orchestrator(promote()).await);
    assert_eq!(asked.outcome, PromotionOutcome::AwaitingOperator);
    assert_eq!(
        asked.pull_request.as_deref(),
        Some("https://github.com/o/r/pull/9")
    );
    assert!(!gh.merged(), "nothing merged");
    assert_eq!(pushed(&origin_of(&h), "main"), main);
    assert!(told(&h, NotificationKind::Blocker, "run `archon promote demo`").await);
    let creates = |gh: &Gh| {
        gh.calls()
            .iter()
            .filter(|c| c.starts_with("pr create"))
            .count()
    };
    assert_eq!(creates(&gh), 1);

    update(
        &h,
        RepoPatch {
            grant_merge: Some(true),
            ..RepoPatch::default()
        },
    )
    .await;
    let merged = promotion(h.orchestrator(promote()).await);
    assert!(
        matches!(merged.outcome, PromotionOutcome::Merged { .. }),
        "{merged:?}"
    );
    assert!(gh.merged());
    assert_eq!(creates(&gh), 1, "the open pull request is reused");
}

/// A merge queue holds the merge: the promotion says to promote again, and
/// the next `promote` after it landed finishes it, the hook included, once.
#[tokio::test]
async fn a_queued_merge_is_finished_by_the_next_promotion() {
    let (h, gh, _slot) = promoting().await;
    land(&h, "one");
    integration::pass(&h.ctx).await.expect("green pass");
    gh.queue_merges();

    let held = h.cli(promote()).await;
    assert!(
        matches!(&held, Err(ArchonError::Unavailable(why)) if why.contains("merge queue")),
        "{held:?}"
    );
    assert!(!gh.merged());
    assert!(!file(&h, "released").exists(), "no hook before the merge");
    gh.land_queued(&h.ctx.config.github.binary);

    let done = promotion(h.cli(promote()).await);
    let PromotionOutcome::Merged {
        release_head, hook, ..
    } = &done.outcome
    else {
        panic!("not finished: {done:?}");
    };
    let origin = origin_of(&h);
    assert_eq!(
        pushed(&origin, "next").as_deref(),
        Some(release_head.as_str())
    );
    assert!(hook.as_ref().is_some_and(|hook| hook.passed), "{hook:?}");
    let released = std::fs::read_to_string(file(&h, "released")).expect("hook output");
    assert_eq!(released.trim(), release_head, "the hook ran at the merge");

    std::fs::remove_file(file(&h, "released")).unwrap();
    let again = promotion(h.cli(promote()).await);
    assert!(
        matches!(again.outcome, PromotionOutcome::UpToDate { .. }),
        "{again:?}"
    );
    assert!(!file(&h, "released").exists(), "the hook ran once");
}

/// A queued promotion is finished only between the branches it was sent for:
/// another release branch holding the head neither finishes nor drops it.
#[tokio::test]
async fn a_queued_merge_is_finished_only_for_its_own_release_branch() {
    let (h, gh, _slot) = promoting().await;
    let landed = land(&h, "one");
    integration::pass(&h.ctx).await.expect("green pass");
    gh.queue_merges();
    assert!(h.cli(promote()).await.is_err(), "the merge is queued");

    let origin = origin_of(&h);
    git(&origin, &["branch", "staging", &landed]);
    let release = |name: &str| RepoPatch {
        tiers: TiersPatch {
            release_branch: Clearable::Set(name.into()),
            ..TiersPatch::default()
        },
        ..RepoPatch::default()
    };
    update(&h, release("staging")).await;
    let elsewhere = promotion(h.cli(promote()).await);
    assert!(
        matches!(elsewhere.outcome, PromotionOutcome::UpToDate { .. }),
        "{elsewhere:?}"
    );
    assert!(!file(&h, "released").exists(), "not the queued promotion");

    update(&h, release("main")).await;
    gh.land_queued(&h.ctx.config.github.binary);
    let done = promotion(h.cli(promote()).await);
    assert!(
        matches!(done.outcome, PromotionOutcome::Merged { .. }),
        "{done:?}"
    );
    assert!(file(&h, "released").exists(), "the owed hook ran");
}

/// `.archon/repo.toml` naming `hook` as the release hook.
fn release_file(hook: &str) -> String {
    format!("[checks]\nrelease = \"{}\"\n", hook.replace('"', "\\\""))
}

/// Commit `.archon/repo.toml` naming `hook` in the registered checkout.
fn commit_release_file(h: &Harness, hook: &str) {
    std::fs::create_dir_all(h.repo.join(".archon")).unwrap();
    std::fs::write(h.repo.join(".archon/repo.toml"), release_file(hook)).unwrap();
    git(&h.repo, &["add", ".archon/repo.toml"]);
    git(&h.repo, &["commit", "--quiet", "-m", "Name a release hook"]);
}

/// Drop the registry's release hook and set the release confirmation.
async fn confirm_release(h: &Harness, confirmed: Option<&str>) {
    let tiers = TiersPatch {
        release_hook: Clearable::Clear,
        confirmed_release_check: confirmed.map_or(Clearable::Clear, |c| Clearable::Set(c.into())),
        ..TiersPatch::default()
    };
    update(
        h,
        RepoPatch {
            tiers,
            ..RepoPatch::default()
        },
    )
    .await;
}

/// A release hook in the registered checkout's committed `.archon/repo.toml`
/// never runs as the daemon while the operator has not confirmed its text.
#[tokio::test]
async fn an_unconfirmed_committed_release_hook_never_runs() {
    let (h, _gh, _slot) = promoting().await;
    let hook = format!("touch '{}'", file(&h, "unconfirmed").display());
    commit_release_file(&h, &hook);
    confirm_release(&h, None).await;
    land(&h, "one");
    integration::pass(&h.ctx).await.expect("green pass");
    assert_eq!(health(&h).await, IntegrationHealth::Green);

    let done = promotion(h.cli(promote()).await);
    let PromotionOutcome::Merged { hook, .. } = &done.outcome else {
        panic!("not merged: {done:?}");
    };
    assert!(hook.is_none(), "no trusted hook to run: {hook:?}");
    assert!(!file(&h, "unconfirmed").exists(), "the hook never ran");
    assert!(!file(&h, "released").exists(), "nor the cleared one");
}

/// A release commit landing after the queued merge and before the promotion
/// is finished is not released: the hook runs at the merge commit.
#[tokio::test]
async fn a_finished_queued_merge_runs_the_hook_at_its_merge_commit() {
    let (h, gh, _slot) = promoting().await;
    let landed = land(&h, "one");
    integration::pass(&h.ctx).await.expect("green pass");
    gh.queue_merges();
    let held = h.cli(promote()).await;
    assert!(
        matches!(&held, Err(ArchonError::Unavailable(_))),
        "{held:?}"
    );
    gh.land_queued(&h.ctx.config.github.binary);
    let origin = origin_of(&h);
    let merged = pushed(&origin, "main").expect("the queued merge landed");
    let parents = git(&origin, &["rev-list", "--parents", "-n", "1", &merged]);
    assert!(parents.ends_with(&landed), "a merge of the head: {parents}");

    let clone = h.dir.path().join("upstream-later");
    let to = clone.to_str().unwrap();
    git(
        h.dir.path(),
        &["clone", "--quiet", origin.to_str().unwrap(), to],
    );
    git(&clone, &["checkout", "--quiet", "main"]);
    std::fs::write(clone.join("later"), "not promoted\n").unwrap();
    git(&clone, &["add", "later"]);
    git(&clone, &["commit", "--quiet", "-m", "Add later"]);
    git(&clone, &["push", "--quiet", "origin", "main"]);
    let later = git(&clone, &["rev-parse", "HEAD"]);

    let done = promotion(h.cli(promote()).await);
    let PromotionOutcome::Merged {
        release_head, hook, ..
    } = &done.outcome
    else {
        panic!("not finished: {done:?}");
    };
    assert_eq!(release_head, &later, "the release branch's head now");
    assert!(hook.as_ref().is_some_and(|hook| hook.passed), "{hook:?}");
    let released = std::fs::read_to_string(file(&h, "released")).expect("hook output");
    assert_eq!(released.trim(), merged, "the hook ran at the merge commit");
}

/// The hook the promotion's merge commits runs when it is the confirmed one,
/// whatever the registered checkout's `HEAD` names instead.
#[tokio::test]
async fn the_hook_the_merge_commits_is_the_one_confirmed_and_run() {
    let (h, _gh, _slot) = promoting().await;
    let merged = format!("git rev-parse HEAD > '{}'", file(&h, "merged").display());
    let local = format!("touch '{}'", file(&h, "local").display());
    commit_release_file(&h, &local);
    confirm_release(&h, Some(&merged)).await;
    land_file(&h, "one", ".archon/repo.toml", &release_file(&merged));
    integration::pass(&h.ctx).await.expect("green pass");

    let done = promotion(h.cli(promote()).await);
    let PromotionOutcome::Merged {
        release_head, hook, ..
    } = &done.outcome
    else {
        panic!("not merged: {done:?}");
    };
    assert!(hook.as_ref().is_some_and(|hook| hook.passed), "{hook:?}");
    assert_eq!(hook.as_ref().unwrap().command, merged, "the merge's hook");
    let released = std::fs::read_to_string(file(&h, "merged")).expect("hook output");
    assert_eq!(released.trim(), release_head, "the hook ran at the merge");
    assert!(!file(&h, "local").exists(), "HEAD's hook never ran");
}

/// While a queued merge waits, the registered checkout commits and the
/// operator confirms another hook: the merge's own hook no longer matches,
/// and neither runs.
#[tokio::test]
async fn a_hook_changed_during_the_wait_does_not_run_at_the_merge() {
    let (h, gh, _slot) = promoting().await;
    let first = format!("touch '{}'", file(&h, "first").display());
    let second = format!("touch '{}'", file(&h, "second").display());
    commit_release_file(&h, &first);
    confirm_release(&h, Some(&first)).await;
    land_file(&h, "one", ".archon/repo.toml", &release_file(&first));
    integration::pass(&h.ctx).await.expect("green pass");
    gh.queue_merges();
    let held = h.cli(promote()).await;
    assert!(
        matches!(&held, Err(ArchonError::Unavailable(_))),
        "{held:?}"
    );

    commit_release_file(&h, &second);
    confirm_release(&h, Some(&second)).await;
    gh.land_queued(&h.ctx.config.github.binary);
    let done = promotion(h.cli(promote()).await);
    let PromotionOutcome::Merged { hook, .. } = &done.outcome else {
        panic!("not finished: {done:?}");
    };
    assert!(hook.is_none(), "no hook is trusted at the merge: {hook:?}");
    assert!(!file(&h, "first").exists(), "the unconfirmed one never ran");
    assert!(!file(&h, "second").exists(), "nor the one the merge lacks");
}
