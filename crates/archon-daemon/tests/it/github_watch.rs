//! Repository watches (A18) over the stub `gh` of `github_sync`: one
//! conditional `GET` of a registered repository's open pull requests per
//! interval, and a `github_watch` trigger, with no operator standing, only
//! when the list changed.

use crate::common;
use crate::github_sync::{Github, busy, git, harness, reference};

use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::config::Config;
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::{
    ClientKind, GithubKind, GithubLink, LinkRelation, Reply, RepoRef, Request,
};
use archon_daemon::github_sync::watch::{KEY_PREFIX, WATCH_TRUST};
use archon_daemon::github_sync::{GithubSync, MIN_INTERVAL, Pass};
use archon_daemon::handlers;
use archon_daemon::triggers::{self, TriggerEvent};
use common::{Harness, REPO_NAME};

const T0: &str = "2026-11-01T00:00:00Z";
const T1: &str = "2026-11-02T00:00:00Z";
const T2: &str = "2026-11-03T00:00:00Z";
const T3: &str = "2026-11-04T00:00:00Z";

fn pull(number: u64, head: &str) -> serde_json::Value {
    pull_at(number, head, T0)
}

fn pull_at(number: u64, head: &str, updated: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number, "title": format!("PR {number}"), "user": {"login": "devArchOverclocked"},
        "html_url": format!("https://github.com/o/r/pull/{number}"),
        "head": {"sha": head}, "updated_at": updated, "requested_reviewers": [],
    })
}

/// The open pull requests `numbers`, newest first, each at `updated`.
fn pulls(numbers: impl Iterator<Item = u64>, updated: &str) -> serde_json::Value {
    numbers.map(|n| pull_at(n, "a", updated)).collect()
}

const LIST: &str = "repos/o/r/pulls?state=open&sort=updated&direction=desc&per_page=100&page=";

fn watch_repo(repo: &str) -> Request {
    Request::WatchRepo {
        repo: RepoRef(repo.into()),
        interval_minutes: None,
        remove: false,
    }
}

async fn pass(h: &Harness) -> Pass {
    GithubSync::new(MIN_INTERVAL).pass(&h.ctx).await.unwrap()
}

async fn watch_events(h: &Harness) -> Vec<TriggerEvent> {
    let held = triggers::held(&h.ctx).await.unwrap();
    held.into_iter()
        .filter(|e| matches!(e, TriggerEvent::GithubWatch { .. }))
        .collect()
}

async fn watches(h: &Harness) -> usize {
    let store = h.ctx.store.lock().await;
    store.kv_prefix(KEY_PREFIX).unwrap().len()
}

/// The orchestrator watches the harness repository, and the first read, the
/// baseline, raises nothing.
async fn watched(github: &Github) -> Harness {
    github.serve_list(serde_json::json!([pull(1, "a")]), "W/\"l1\"");
    let h = harness(github).await;
    let Reply::RepoWatches(listed) = h.orchestrator(watch_repo(REPO_NAME)).await.unwrap() else {
        panic!("watch_repo replies with the watches");
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].repository.as_deref(), Some("o/r"));
    let baseline = pass(&h).await;
    assert_eq!(
        (baseline.watch.requested, baseline.watch.changed),
        (1, 1),
        "{baseline:?}"
    );
    assert_eq!(baseline.watch.triggers, 0, "the first list is the baseline");
    assert!(watch_events(&h).await.is_empty());
    h
}

#[tokio::test]
async fn an_unchanged_list_costs_one_conditional_request_and_raises_nothing() {
    let github = Github::new();
    let h = watched(&github).await;
    let written = h.ctx.store.lock().await.rows_written();
    let unchanged = pass(&h).await;
    assert_eq!(unchanged.requested, 1, "one request: {unchanged:?}");
    assert_eq!(
        (unchanged.watch.unchanged, unchanged.watch.triggers),
        (1, 0)
    );
    let last = github.calls().pop().unwrap();
    assert!(last.contains("If-None-Match: W/\"l1\""), "{last}");
    assert!(last.ends_with(&format!("{LIST}1")), "{last}");
    assert_eq!(
        h.ctx.store.lock().await.rows_written(),
        written,
        "a 304 writes nothing"
    );
    assert!(
        watch_events(&h).await.is_empty(),
        "no trigger, so no model turn"
    );
    assert!(!triggers::due(&h.ctx).await.unwrap());
    github.assert_only_reads();
}

#[tokio::test]
async fn a_new_pull_request_raises_one_trigger_without_operator_standing() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"l2\"");
    let changed = pass(&h).await;
    assert_eq!(
        (changed.watch.changed, changed.watch.triggers),
        (1, 1),
        "{changed:?}"
    );
    let again = pass(&h).await;
    assert_eq!(again.watch.triggers, 0, "the same list again is no news");

    let events = watch_events(&h).await;
    assert_eq!(events.len(), 1, "exactly one trigger: {events:?}");
    let TriggerEvent::GithubWatch {
        repo,
        repository,
        changes,
        operator,
        trust,
        ..
    } = &events[0]
    else {
        unreachable!()
    };
    assert_eq!((repo.as_str(), repository.as_str()), (REPO_NAME, "o/r"));
    let told: Vec<(u64, &str)> = changes
        .iter()
        .map(|c| (c.number, c.change.as_str()))
        .collect();
    assert_eq!(told, [(2, "new_pull_request")]);
    assert_eq!(changes[0].author.as_deref(), Some("devArchOverclocked"));
    assert!(!operator, "a watch carries no operator standing");
    assert_eq!(trust, WATCH_TRUST);
    let wire = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(wire["operator"], false);
    assert_eq!(wire["type"], "github_watch");
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "news wakes the orchestrator"
    );
    github.assert_only_reads();
}

#[tokio::test]
async fn only_a_registered_repository_with_a_github_origin_is_watched() {
    let github = Github::new();
    let h = harness(&github).await;
    let refused = h
        .orchestrator(watch_repo("ccso-unregistered"))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("not a registered repository"),
        "{refused}"
    );
    assert!(refused.to_string().contains("never registers"), "{refused}");
    let interval = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: Some(1),
        remove: false,
    };
    let refused = h.orchestrator(interval).await.unwrap_err();
    assert!(
        refused.to_string().contains("interval_minutes must be 5"),
        "{refused}"
    );
    assert_eq!(watches(&h).await, 0);

    let mut config = Config::default();
    config.github.binary = github.binary();
    let no_origin = Harness::with_config(config).await;
    let refused = no_origin
        .orchestrator(watch_repo(REPO_NAME))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("no github.com origin"),
        "{refused}"
    );
    assert_eq!(watches(&no_origin).await, 0);
    assert!(github.calls().is_empty(), "a refusal reads nothing");
}

/// 60 linked objects and a watch: one pass still sends at most 50 `GET`s,
/// and the watch is among them.
#[tokio::test]
async fn the_per_pass_request_budget_holds_with_a_watch() {
    let github = Github::new();
    let h = watched(&github).await;
    let item = h.enqueue("linked work").await;
    for number in 1..=60 {
        let link = GithubLink {
            item: item.id.clone(),
            reference: reference(number, GithubKind::Issue),
            relation: LinkRelation::Mentioned,
            source: LinkSource::Operator,
            created_at: archon_core::now(),
        };
        h.ctx.store.lock().await.link_github(&link).unwrap();
    }
    let before = github.calls().len();
    let full = pass(&h).await;
    assert_eq!(full.requested, 50, "{full:?}");
    assert_eq!(full.watch.requested, 1, "the watch is read: {full:?}");
    assert_eq!(full.missing, 49, "the linked objects take the rest");
    assert_eq!(github.calls().len() - before, 50);
}

#[tokio::test]
async fn a_repository_removed_while_watched_is_no_longer_read() {
    let github = Github::new();
    let h = watched(&github).await;
    assert!(matches!(
        h.cli(Request::RepoRemove {
            repo: RepoRef(REPO_NAME.into())
        })
        .await,
        Ok(Reply::Ok)
    ));
    assert_eq!(
        watches(&h).await,
        0,
        "removing the repository drops its watch"
    );
    let calls = github.calls().len();
    assert_eq!(pass(&h).await.requested, 0);
    assert_eq!(github.calls().len(), calls, "nothing more is read");
}

/// A watch whose registration vanished without the removal request (a
/// store written by another path) is dropped by the next pass, unread.
#[tokio::test]
async fn a_pass_drops_the_watch_of_an_unregistered_repository() {
    let github = Github::new();
    let h = watched(&github).await;
    {
        let mut store = h.ctx.store.lock().await;
        let repo = store.repo(&RepoRef(REPO_NAME.into())).unwrap();
        store.remove_repo(&repo.id).unwrap();
    }
    assert_eq!(watches(&h).await, 1);
    let calls = github.calls().len();
    let after = pass(&h).await;
    assert_eq!((after.requested, after.watch.dropped), (0, 1), "{after:?}");
    assert_eq!(watches(&h).await, 0);
    assert_eq!(github.calls().len(), calls);
}

#[tokio::test]
async fn a_removed_watch_stops_and_the_list_says_so() {
    let github = Github::new();
    let h = watched(&github).await;
    let remove = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: None,
        remove: true,
    };
    let Reply::RepoWatches(left) = h.orchestrator(remove).await.unwrap() else {
        panic!("watch_repo replies with the watches");
    };
    assert!(left.is_empty());
    let calls = github.calls().len();
    assert_eq!(pass(&h).await.watch.requested, 0);
    assert_eq!(github.calls().len(), calls);
}

/// The one trigger held since the baseline: its changes as (number, change).
async fn told(h: &Harness) -> Vec<(u64, String, Vec<String>)> {
    let events = watch_events(h).await;
    let Some(TriggerEvent::GithubWatch { changes, .. }) = events.last() else {
        return Vec::new();
    };
    changes
        .iter()
        .map(|c| (c.number, c.change.clone(), c.reviewers.clone()))
        .collect()
}

async fn listed(h: &Harness) -> Vec<archon_core::protocol::github_links::RepoWatchView> {
    let Reply::RepoWatches(listed) = h.cli(Request::RepoWatches).await.unwrap() else {
        panic!("repo_watches replies with the watches");
    };
    listed
}

#[tokio::test]
async fn only_the_orchestrator_and_the_operator_set_a_watch() {
    let github = Github::new();
    github.serve_list(serde_json::json!([pull(1, "a")]), "W/\"l1\"");
    let h = harness(&github).await;
    h.orchestrator(Request::Status(Default::default()))
        .await
        .unwrap();
    let stage = Request::ForAgent {
        caller: AetherAgentId::parse("agt_stage_worker").unwrap(),
        request: Box::new(watch_repo(REPO_NAME)),
    };
    let refused = handlers::handle(&h.ctx, ClientKind::ArchonMcp, stage)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("orchestrator"),
        "a stage agent is refused: {refused}"
    );
    assert_eq!(watches(&h).await, 0);
    let Reply::RepoWatches(set) = h.cli(watch_repo(REPO_NAME)).await.unwrap() else {
        panic!("watch_repo replies with the watches");
    };
    assert_eq!(set.len(), 1, "the operator sets one from the CLI");
    assert!(github.calls().is_empty(), "setting a watch reads nothing");
}

/// One sync, as archond runs it: a watch is read again only after its interval.
#[tokio::test]
async fn a_watch_is_read_again_only_after_its_interval() {
    let github = Github::new();
    let h = watched(&github).await;
    tokio::time::pause();
    let mut sync = GithubSync::new(MIN_INTERVAL);
    let first = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!((first.watch.requested, first.watch.unchanged), (1, 1));
    tokio::time::advance(Duration::from_secs(4 * 60)).await;
    let early = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        early.watch.requested, 0,
        "asked four minutes ago: {early:?}"
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    let due = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(due.watch.requested, 1, "five minutes on: {due:?}");
}

/// GitHub's backoff on a watch read stops the pass: no linked object is read,
/// and the sync waits before asking again.
#[tokio::test]
async fn a_backoff_on_a_watch_read_pauses_the_sync() {
    let github = Github::new();
    let h = watched(&github).await;
    let item = h.enqueue("linked work").await;
    let link = GithubLink {
        item: item.id.clone(),
        reference: reference(9, GithubKind::Issue),
        relation: LinkRelation::Mentioned,
        source: LinkSource::Operator,
        created_at: archon_core::now(),
    };
    h.ctx.store.lock().await.link_github(&link).unwrap();
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"l2\"");
    std::fs::write(github.path("retry-after"), "120").unwrap();
    let mut sync = GithubSync::new(MIN_INTERVAL);
    let limited = sync.pass(&h.ctx).await.unwrap();
    assert_eq!(
        limited.backoff,
        Some(Duration::from_secs(120)),
        "{limited:?}"
    );
    assert_eq!((limited.requested, limited.watch.triggers), (1, 1));
    assert_eq!(limited.missing, 0, "the linked object waits");
    std::fs::remove_file(github.path("retry-after")).unwrap();
    let paused = sync.pass(&h.ctx).await.unwrap();
    assert_eq!(paused.requested, 0, "still within the backoff");
}

/// A watch removed and set up again while its read is in flight starts over:
/// the old read writes no baseline into the new watch.
#[tokio::test]
async fn a_read_begun_under_a_removed_watch_is_not_written_back() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"l2\"");
    let hold = github.path("hold-pulls-1");
    std::fs::write(&hold, "").unwrap();
    let remove = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: None,
        remove: true,
    };
    let mut sync = GithubSync::new(MIN_INTERVAL);
    let driver = async {
        while !github.path("started-pulls-1").exists() {
            tokio::task::yield_now().await;
        }
        h.orchestrator(remove).await.unwrap();
        h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
        std::fs::remove_file(&hold).unwrap();
    };
    let (read, ()) = busy(async { tokio::join!(sync.pass(&h.ctx), driver) }).await;
    let read = read.unwrap();
    assert_eq!(
        (read.watch.changed, read.watch.triggers),
        (1, 0),
        "{read:?}"
    );
    assert!(watch_events(&h).await.is_empty());
    let after = listed(&h).await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].tracked_pull_requests, None,
        "the new watch has no baseline"
    );
}

/// A label or edit moves `updated_at` and is no news; a comment or a review
/// after the mark is.
#[tokio::test]
async fn only_a_new_comment_or_review_makes_activity_news() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"l2\"");
    let edited = pass(&h).await;
    assert_eq!(
        (edited.watch.requested, edited.watch.triggers),
        (3, 0),
        "{edited:?}"
    );
    let calls = github.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with("issues/1/comments?since=2026-10-31T23:59:59Z&per_page=100")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with("pulls/1/reviews?per_page=100")),
        "{calls:?}"
    );
    assert_eq!(
        pass(&h).await.watch.requested,
        1,
        "the edit is marked: one 304"
    );

    github.serve_named(
        "comments-1",
        serde_json::json!([{"id": 7, "created_at": T2}]),
        "",
    );
    github.serve_list(serde_json::json!([pull_at(1, "a", T2)]), "W/\"l3\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(1, "new_comment".to_owned(), vec![])]);

    github.serve_named(
        "reviews-1",
        serde_json::json!([{"id": 8, "submitted_at": T3}]),
        "",
    );
    github.serve_list(serde_json::json!([pull_at(1, "a", T3)]), "W/\"l4\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(1, "new_review".to_owned(), vec![])]);
    github.assert_only_reads();
}

#[tokio::test]
async fn a_team_review_request_is_news() {
    let github = Github::new();
    let h = watched(&github).await;
    let mut requested = pull_at(1, "a", T1);
    requested["requested_teams"] = serde_json::json!([{"slug": "core"}]);
    github.serve_list(serde_json::json!([requested]), "W/\"l2\"");
    let read = pass(&h).await;
    assert_eq!(
        (read.watch.requested, read.watch.triggers),
        (1, 1),
        "{read:?}"
    );
    assert_eq!(
        told(&h).await,
        [(
            1,
            "review_requested".to_owned(),
            vec!["team:core".to_owned()]
        )]
    );
}

/// Past 100 open pull requests: every read asks every page, down to a
/// short one.
#[tokio::test]
async fn more_than_a_page_of_pull_requests_is_watched() {
    let github = Github::new();
    github.serve_list(pulls((6..=105).rev(), T0), "W/\"p1\"");
    github.serve_named("pulls-2", pulls((1..=5).rev(), T0), "");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    let baseline = pass(&h).await;
    assert_eq!(baseline.watch.requested, 2, "{baseline:?}");
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(105));

    github.serve_list(pulls((106..=205).rev(), T1), "W/\"p2\"");
    github.serve_named("pulls-2", pulls((6..=105).rev(), T0), "");
    github.serve_named("pulls-3", pulls((1..=5).rev(), T0), "");
    let before = github.calls().len();
    let grown = pass(&h).await;
    assert_eq!(
        (grown.watch.requested, grown.watch.triggers),
        (3, 1),
        "{grown:?}"
    );
    assert!(github.calls()[before + 1].ends_with(&format!("{LIST}2")));
    let events = watch_events(&h).await;
    let TriggerEvent::GithubWatch { changes, more, .. } = &events[0] else {
        unreachable!()
    };
    assert_eq!(
        (changes.len(), *more),
        (20, 80),
        "100 new pull requests, 20 listed"
    );
    assert!(changes.iter().all(|c| c.change == "new_pull_request"));

    let mut newest = pulls((107..=205).rev(), T1);
    newest
        .as_array_mut()
        .unwrap()
        .insert(0, pull_at(206, "a", T2));
    github.serve_list(newest, "W/\"p3\"");
    let one = pass(&h).await;
    assert_eq!(
        (one.watch.requested, one.watch.triggers),
        (3, 1),
        "every page is asked, though only page 1 holds news"
    );
    github.assert_only_reads();
}

/// Follow-ups past the budget are left for the next pass, whose first page
/// is read again unconditionally, until every one was asked.
#[tokio::test]
async fn follow_ups_past_the_budget_wait_for_the_next_pass() {
    let github = Github::new();
    github.serve_list(pulls((1..=30).rev(), T0), "W/\"f1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    pass(&h).await;
    github.serve_list(pulls((1..=30).rev(), T1), "W/\"f2\"");
    let first = pass(&h).await;
    assert_eq!(
        (first.watch.requested, first.watch.incomplete),
        (25, 1),
        "{first:?}"
    );
    let second = pass(&h).await;
    let unconditional = github.calls()[github.calls().len() - 25].clone();
    assert!(!unconditional.contains("If-None-Match"), "{unconditional}");
    assert_eq!((second.watch.requested, second.watch.incomplete), (25, 1));
    let third = pass(&h).await;
    assert_eq!(
        (third.watch.requested, third.watch.incomplete),
        (13, 0),
        "{third:?}"
    );
    let settled = pass(&h).await;
    assert_eq!((settled.requested, settled.watch.unchanged), (1, 1));
    assert!(watch_events(&h).await.is_empty(), "edits only, no news");
}

/// Old comments and reviews filling a page are no news: the last page,
/// where the newest ids are, is read, and only a later one is told.
#[tokio::test]
async fn full_pages_of_old_comments_and_reviews_are_no_news() {
    let github = Github::new();
    let h = watched(&github).await;
    let old = |ids: std::ops::RangeInclusive<u64>, field: &str| -> serde_json::Value {
        let made = "2026-10-01T00:00:00Z";
        ids.map(|id| serde_json::json!({"id": id, field: made, "updated_at": T1}))
            .collect()
    };
    github.serve_named("comments-1", old(1..=100, "created_at"), "");
    github.serve_named("comments-1-p2", old(101..=101, "created_at"), "");
    std::fs::write(github.path("link-comments-1"), "2").unwrap();
    github.serve_named("reviews-1", old(201..=300, "submitted_at"), "");
    github.serve_named("reviews-1-p2", old(301..=301, "submitted_at"), "");
    std::fs::write(github.path("link-reviews-1"), "2").unwrap();
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"l2\"");
    let edited = pass(&h).await;
    assert_eq!(
        (edited.watch.requested, edited.watch.triggers),
        (5, 0),
        "{edited:?}"
    );
    let calls = github.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with("pulls/1/reviews?per_page=100&page=2")),
        "{calls:?}"
    );

    let newest = serde_json::json!([{"id": 302, "submitted_at": T2}]);
    github.serve_named("reviews-1-p2", newest, "");
    github.serve_list(serde_json::json!([pull_at(1, "a", T2)]), "W/\"l3\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(1, "new_review".to_owned(), vec![])]);
}

/// GitHub dates to the second: a comment in the same second as the mark
/// leaves `updated_at` as it was, so a mark that recent is checked again.
#[tokio::test]
async fn a_comment_in_the_marked_second_is_still_told() {
    let github = Github::new();
    let fresh = archon_core::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    github.serve_list(serde_json::json!([pull_at(1, "a", &fresh)]), "W/\"s1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    pass(&h).await;
    let comment = serde_json::json!([{"id": 5, "created_at": fresh}]);
    github.serve_named("comments-1", comment, "");
    let before = github.calls().len();
    let recheck = pass(&h).await;
    let list = &github.calls()[before];
    assert!(
        !list.contains("If-None-Match"),
        "an unsettled mark keeps no ETag: {list}"
    );
    assert_eq!(recheck.watch.triggers, 1, "{recheck:?}");
    assert_eq!(told(&h).await, [(1, "new_comment".to_owned(), vec![])]);
    let again = pass(&h).await;
    assert_eq!(again.watch.triggers, 0, "a comment is told once: {again:?}");
}

/// GitHub's order by update is not exact, so news can change a later page
/// while the first is unchanged: every page is asked, each conditionally.
#[tokio::test]
async fn news_on_a_later_page_is_read_though_the_first_is_unchanged() {
    let github = Github::new();
    github.serve_list(pulls((6..=105).rev(), T0), "W/\"t1\"");
    github.serve_named("pulls-2", pulls((1..=5).rev(), T0), "W/\"u1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    assert_eq!(pass(&h).await.watch.requested, 2);
    let mut later: Vec<serde_json::Value> = vec![pull_at(106, "n", T0)];
    later.extend((2..=5).rev().map(|n| pull_at(n, "a", T0)));
    later.push(pull_at(1, "a", T1));
    github.serve_named("pulls-2", later.into(), "W/\"u2\"");
    github.serve_named(
        "comments-1",
        serde_json::json!([{"id": 7, "created_at": T1}]),
        "",
    );
    let read = pass(&h).await;
    assert_eq!(
        (read.watch.requested, read.watch.triggers),
        (4, 1),
        "two pages, then #1's comments and reviews: {read:?}"
    );
    assert_eq!(
        told(&h).await,
        [
            (106, "new_pull_request".to_owned(), vec![]),
            (1, "new_comment".to_owned(), vec![])
        ]
    );
    let first = github.calls()[github.calls().len() - 4].clone();
    assert!(first.contains("If-None-Match: W/\"t1\""), "{first}");
    let unchanged = pass(&h).await;
    assert_eq!(
        (unchanged.watch.requested, unchanged.watch.unchanged),
        (2, 1),
        "one conditional GET a page: {unchanged:?}"
    );
}

/// A checkout re-pointed at another repository during a read: the read of
/// the old one is discarded, and the next pass baselines the new one.
#[tokio::test]
async fn a_read_is_discarded_when_the_origin_moves_meanwhile() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"l2\"");
    let hold = github.path("hold-pulls-1");
    std::fs::write(&hold, "").unwrap();
    let mut sync = GithubSync::new(MIN_INTERVAL);
    let driver = async {
        while !github.path("started-pulls-1").exists() {
            tokio::task::yield_now().await;
        }
        let moved = ["remote", "set-url", "origin", "https://github.com/o/s.git"];
        git(&h.repo, &moved);
        std::fs::remove_file(&hold).unwrap();
    };
    let (read, ()) = busy(async { tokio::join!(sync.pass(&h.ctx), driver) }).await;
    assert_eq!(read.unwrap().watch.triggers, 0);
    assert!(
        watch_events(&h).await.is_empty(),
        "no news of the old repository"
    );
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(1));
    let rebased = pass(&h).await;
    assert_eq!(
        (rebased.watch.changed, rebased.watch.triggers),
        (1, 0),
        "{rebased:?}"
    );
    let after = listed(&h).await;
    assert_eq!(after[0].repository.as_deref(), Some("o/s"));
    assert_eq!(after[0].tracked_pull_requests, Some(2), "a new baseline");
    let last = github.calls().pop().unwrap();
    assert!(last.contains("repos/o/s/pulls"), "{last}");
}

/// A review gets its id when started: one seen pending and submitted later,
/// after a newer review was told, is still told when it is submitted.
#[tokio::test]
async fn a_pending_review_is_told_when_it_is_submitted() {
    let github = Github::new();
    let h = watched(&github).await;
    let started = serde_json::json!([
        {"id": 10, "submitted_at": null},
        {"id": 11, "submitted_at": T1},
    ]);
    github.serve_named("reviews-1", started, "");
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"r1\"");
    assert_eq!(pass(&h).await.watch.triggers, 1, "#11 was submitted");
    let submitted = serde_json::json!([
        {"id": 10, "submitted_at": T2},
        {"id": 11, "submitted_at": T1},
    ]);
    github.serve_named("reviews-1", submitted, "");
    github.serve_list(serde_json::json!([pull_at(1, "a", T2)]), "W/\"r2\"");
    assert_eq!(pass(&h).await.watch.triggers, 1, "#10 is submitted now");
    assert_eq!(told(&h).await, [(1, "new_review".to_owned(), vec![])]);
    assert_eq!(watch_events(&h).await.len(), 2);
}

/// A pull request closed and reopened unchanged is no news.
#[tokio::test]
async fn a_reopened_pull_request_alone_is_no_news() {
    let github = Github::new();
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"o1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    pass(&h).await;
    github.serve_list(serde_json::json!([pull_at(2, "b", T0)]), "W/\"o2\"");
    assert_eq!(pass(&h).await.watch.triggers, 0, "a closing is no news");
    let tracked = listed(&h).await[0].tracked_pull_requests;
    assert_eq!(tracked, Some(2), "a closed pull request keeps its mark");
    let reopened = serde_json::json!([pull_at(1, "a", T1), pull(2, "b")]);
    github.serve_list(reopened, "W/\"o3\"");
    let read = pass(&h).await;
    assert_eq!(
        (read.watch.requested, read.watch.triggers),
        (3, 0),
        "the list, then #1's comments and reviews: {read:?}"
    );
    assert!(watch_events(&h).await.is_empty());
}

/// A watch removed while its list is read sends nothing more: the follow-ups
/// the changed list asks for are not sent.
#[tokio::test]
async fn a_watch_removed_during_a_read_sends_no_more_requests() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"v2\"");
    let hold = github.path("hold-pulls-1");
    std::fs::write(&hold, "").unwrap();
    let remove = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: None,
        remove: true,
    };
    let mut sync = GithubSync::new(MIN_INTERVAL);
    let driver = async {
        while !github.path("started-pulls-1").exists() {
            tokio::task::yield_now().await;
        }
        h.orchestrator(remove).await.unwrap();
        std::fs::remove_file(&hold).unwrap();
    };
    let (read, ()) = busy(async { tokio::join!(sync.pass(&h.ctx), driver) }).await;
    let read = read.unwrap();
    assert_eq!(
        (read.watch.requested, read.watch.revoked),
        (1, 1),
        "{read:?}"
    );
    let calls = github.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("/comments") || c.contains("/reviews")),
        "{calls:?}"
    );
    assert_eq!(watches(&h).await, 0);
}

/// A comment and a review already there at the baseline, in the second of
/// its `updated_at`, are no news when a later label moves the pull request.
#[tokio::test]
async fn activity_already_there_at_the_baseline_is_no_news() {
    let github = Github::new();
    let h = watched(&github).await;
    let comment = serde_json::json!([{"id": 5, "created_at": T0}]);
    github.serve_named("comments-1", comment, "");
    let review = serde_json::json!([{"id": 6, "submitted_at": T0}]);
    github.serve_named("reviews-1", review, "");
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"b2\"");
    let labelled = pass(&h).await;
    assert_eq!(
        (labelled.watch.requested, labelled.watch.triggers),
        (3, 0),
        "{labelled:?}"
    );
    assert!(watch_events(&h).await.is_empty());
}

/// A pull request closed at the baseline and reopened later is no new one,
/// though its number is above every one the baseline saw; one opened since is.
#[tokio::test]
async fn a_pull_request_closed_at_the_baseline_and_reopened_is_no_new_one() {
    let github = Github::new();
    let h = watched(&github).await;
    let mut reopened = pull_at(7, "c", T1);
    reopened["created_at"] = T0.into();
    let mut opened = pull_at(8, "d", T1);
    opened["created_at"] = archon_core::now().to_rfc3339().into();
    github.serve_list(
        serde_json::json!([opened, reopened, pull(1, "a")]),
        "W/\"c2\"",
    );
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(8, "new_pull_request".to_owned(), vec![])]);
}

/// A watch saved before the record kept its pages and baseline clock reads
/// on: a new pull request is told by its number, and nothing panics.
#[tokio::test]
async fn a_watch_saved_by_an_earlier_build_reads_on() {
    let github = Github::new();
    let h = watched(&github).await;
    {
        let mut store = h.ctx.store.lock().await;
        let (key, raw) = store.kv_prefix(KEY_PREFIX).unwrap().remove(0);
        let mut record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let record_map = record.as_object_mut().unwrap();
        for gone in ["pages", "baseline_at"] {
            record_map.remove(gone);
        }
        record_map.insert("etag".into(), "W/\"l1\"".into());
        record_map.insert("watermark".into(), T0.into());
        for mark in record_map["pulls"].as_object_mut().unwrap().values_mut() {
            mark.as_object_mut().unwrap().remove("listed");
        }
        store.kv_set(&key, &record.to_string()).unwrap();
    }
    let mut opened = pull_at(2, "b", T1);
    opened["created_at"] = T1.into();
    github.serve_list(serde_json::json!([opened, pull(1, "a")]), "W/\"e2\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(2, "new_pull_request".to_owned(), vec![])]);
}
