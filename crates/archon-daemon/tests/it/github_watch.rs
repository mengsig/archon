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
/// Before and after every walk of these tests, whatever the clock says.
const LONG_AGO: &str = "2000-01-01T00:00:00Z";
const YET_TO_COME: &str = "2099-01-01T00:00:00Z";

fn pull(number: u64, head: &str) -> serde_json::Value {
    pull_at(number, head, T0)
}

/// #`number`, made long ago and last updated at `updated`.
fn pull_at(number: u64, head: &str, updated: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number, "title": format!("PR {number}"), "user": {"login": "devArchOverclocked"},
        "html_url": format!("https://github.com/o/r/pull/{number}"),
        "head": {"sha": head}, "updated_at": updated, "created_at": LONG_AGO,
        "requested_reviewers": [],
    })
}

/// #`number`, made since the last walk.
fn opened(number: u64, head: &str) -> serde_json::Value {
    let mut pull = pull_at(number, head, T1);
    pull["created_at"] = YET_TO_COME.into();
    pull
}

/// The open pull requests `numbers`, newest first, each at `updated`.
fn pulls(numbers: impl Iterator<Item = u64>, updated: &str) -> serde_json::Value {
    numbers.map(|n| pull_at(n, "a", updated)).collect()
}

const LIST: &str = "repos/o/r/pulls?state=open&sort=created&direction=desc&per_page=100&page=";

fn watch_repo(repo: &str) -> Request {
    Request::WatchRepo {
        repo: RepoRef(repo.into()),
        interval_minutes: None,
        remove: false,
    }
}

/// A sync begun five minutes ago: past the floor a restarted one waits.
async fn settled_sync() -> GithubSync {
    let sync = GithubSync::new(MIN_INTERVAL);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    tokio::time::resume();
    sync
}

async fn pass(h: &Harness) -> Pass {
    settled_sync().await.pass(&h.ctx).await.unwrap()
}

/// The watch's stored record, as JSON.
async fn stored(h: &Harness) -> serde_json::Value {
    let store = h.ctx.store.lock().await;
    let (_, raw) = store.kv_prefix(KEY_PREFIX).unwrap().remove(0);
    serde_json::from_str(&raw).unwrap()
}

async fn watch_events(h: &Harness) -> Vec<TriggerEvent> {
    let held = triggers::held(&h.ctx).await.unwrap();
    held.into_iter()
        .filter(|e| matches!(e, TriggerEvent::GithubWatch { .. }))
        .collect()
}

/// The orchestrator takes the news held so far.
async fn taken(h: &Harness) {
    let mut store = h.ctx.store.lock().await;
    store.kv_delete("trigger_outbox").unwrap();
    drop(store);
    assert!(watch_events(h).await.is_empty(), "no news is held");
}

async fn watches(h: &Harness) -> usize {
    let store = h.ctx.store.lock().await;
    store.kv_prefix(KEY_PREFIX).unwrap().len()
}

/// The orchestrator watches the harness repository, and the first walk (the
/// list, then #1's two counts) is stored and raises nothing.
async fn watched(github: &Github) -> Harness {
    github.serve_list(serde_json::json!([pull(1, "a")]), "W/\"l1\"");
    let h = harness(github).await;
    let Reply::RepoWatches(listed) = h.orchestrator(watch_repo(REPO_NAME)).await.unwrap() else {
        panic!("watch_repo replies with the watches");
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].repository.as_deref(), Some("o/r"));
    let first = pass(&h).await;
    assert_eq!(
        (first.watch.requested, first.watch.changed),
        (3, 1),
        "{first:?}"
    );
    assert_eq!(first.watch.triggers, 0, "the first walk tells nothing");
    assert!(watch_events(&h).await.is_empty());
    h
}

/// One sync's passes, five minutes apart, until a walk is not cut short.
async fn walk_whole(h: &Harness) -> Pass {
    let mut sync = settled_sync().await;
    tokio::time::pause();
    let mut walked = busy(sync.pass(&h.ctx)).await.unwrap();
    for _ in 0..20 {
        if walked.watch.incomplete == 0 {
            break;
        }
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        walked = busy(sync.pass(&h.ctx)).await.unwrap();
    }
    assert_eq!(walked.watch.incomplete, 0, "the walk completed: {walked:?}");
    tokio::time::resume();
    walked
}

/// A watch of 105 open pull requests, on two pages, walked once.
async fn two_pages(github: &Github) -> Harness {
    github.serve_list(pulls((6..=105).rev(), T0), "W/\"t1\"");
    github.serve_named("pulls-2", pulls((1..=5).rev(), T0), "W/\"u1\"");
    let h = harness(github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    assert_eq!(walk_whole(&h).await.watch.triggers, 0);
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(105));
    assert!(watch_events(&h).await.is_empty());
    h
}

#[tokio::test]
async fn an_unchanged_list_costs_one_conditional_request_and_raises_nothing() {
    let github = Github::new();
    let h = watched(&github).await;
    let mut before = stored(&h).await;
    let unchanged = pass(&h).await;
    assert_eq!(unchanged.requested, 1, "one request: {unchanged:?}");
    assert_eq!(
        (unchanged.watch.unchanged, unchanged.watch.triggers),
        (1, 0)
    );
    let last = github.calls().pop().unwrap();
    assert!(last.contains("If-None-Match: W/\"l1\""), "{last}");
    assert!(last.ends_with(&format!("{LIST}1")), "{last}");
    let mut after = stored(&h).await;
    assert_eq!((&before["walks"], &after["walks"]), (&1.into(), &2.into()));
    assert_ne!(before["walked_at"], after["walked_at"]);
    for record in [&mut before, &mut after] {
        let record = record.as_object_mut().unwrap();
        record.remove("walks");
        record.remove("walked_at");
    }
    assert_eq!(before, after, "a walk of 304s writes the pass time only");
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
    github.serve_list(
        serde_json::json!([opened(2, "b"), pull(1, "a")]),
        "W/\"l2\"",
    );
    let changed = pass(&h).await;
    assert_eq!(
        (changed.watch.changed, changed.watch.triggers),
        (1, 1),
        "{changed:?}"
    );
    assert_eq!(changed.watch.requested, 3, "the list and #2's two counts");
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

/// The last trigger's changes that name more than one difference: (number, also).
async fn also(h: &Harness) -> Vec<(u64, Vec<String>)> {
    let events = watch_events(h).await;
    let Some(TriggerEvent::GithubWatch { changes, .. }) = events.last() else {
        return Vec::new();
    };
    let more = changes.iter().filter(|c| !c.also.is_empty());
    more.map(|c| (c.number, c.also.clone())).collect()
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
    let mut sync = settled_sync().await;
    tokio::time::pause();
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

/// GitHub's backoff on a watch walk cuts it and stops the pass: no linked
/// object is read, nothing is stored, and the walk tells its news once resumed.
#[tokio::test]
async fn a_backoff_on_a_watch_walk_pauses_the_sync_and_loses_no_news() {
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
    github.serve_list(
        serde_json::json!([opened(2, "b"), pull(1, "a")]),
        "W/\"l2\"",
    );
    std::fs::write(github.path("retry-after"), "120").unwrap();
    let mut sync = settled_sync().await;
    tokio::time::pause();
    let limited = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        limited.backoff,
        Some(Duration::from_secs(120)),
        "{limited:?}"
    );
    assert_eq!(
        (limited.requested, limited.watch.incomplete),
        (1, 1),
        "#2 went uncounted: {limited:?}"
    );
    assert_eq!(limited.missing, 0, "the linked object waits");
    assert!(
        watch_events(&h).await.is_empty(),
        "a cut walk tells nothing"
    );
    std::fs::remove_file(github.path("retry-after")).unwrap();
    let paused = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(paused.requested, 0, "still within the backoff");
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let resumed = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(resumed.watch.triggers, 1, "{resumed:?}");
    assert_eq!(told(&h).await, [(2, "new_pull_request".to_owned(), vec![])]);
}

/// A watch removed and set up again while its walk is in flight starts over:
/// the old walk is stopped and writes nothing into the new watch.
#[tokio::test]
async fn a_walk_begun_under_a_removed_watch_is_not_written_back() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(
        serde_json::json!([opened(2, "b"), pull(1, "a")]),
        "W/\"l2\"",
    );
    let hold = github.path("hold-pulls-1");
    std::fs::write(&hold, "").unwrap();
    let remove = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: None,
        remove: true,
    };
    let mut sync = settled_sync().await;
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
        (
            read.watch.requested,
            read.watch.revoked,
            read.watch.triggers
        ),
        (1, 1, 0),
        "{read:?}"
    );
    assert!(watch_events(&h).await.is_empty());
    let after = listed(&h).await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].tracked_pull_requests, None,
        "the new watch is not walked yet"
    );
}

/// A label or edit moves `updated_at` and is no news; a comment or a review
/// more than the stored count is.
#[tokio::test]
async fn only_a_new_comment_or_review_makes_activity_news() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"l2\"");
    let edited = pass(&h).await;
    assert_eq!(
        (edited.watch.requested, edited.watch.triggers),
        (3, 0),
        "the list and #1's two counts: {edited:?}"
    );
    let calls = github.calls();
    for count in ["issues/1/comments?per_page=1", "pulls/1/reviews?per_page=1"] {
        assert!(calls.iter().any(|c| c.ends_with(count)), "{calls:?}");
    }
    assert_eq!(
        pass(&h).await.watch.requested,
        1,
        "the edit is stored: one 304"
    );

    github.serve_count("comments-1", 1);
    github.serve_list(serde_json::json!([pull_at(1, "a", T2)]), "W/\"l3\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(1, "new_comment".to_owned(), vec![])]);

    github.serve_count("reviews-1", 3);
    github.serve_list(serde_json::json!([pull_at(1, "a", T3)]), "W/\"l4\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(1, "new_comment".to_owned(), vec![])]);
    assert_eq!(also(&h).await, [(1, vec!["new_review".to_owned()])]);
    assert_eq!(
        watch_events(&h).await.len(),
        1,
        "news joins the repository's watch news still held"
    );
    github.assert_only_reads();
}

/// A push, a review request and a comment between two walks are one change
/// naming all three: the walk stores the counts, so no later walk would tell them.
#[tokio::test]
async fn a_push_and_a_comment_at_once_are_both_named() {
    let github = Github::new();
    let h = watched(&github).await;
    let mut pushed = pull_at(1, "b", T1);
    pushed["requested_reviewers"] = serde_json::json!([{"login": "octo"}]);
    github.serve_count("comments-1", 1);
    github.serve_list(serde_json::json!([pushed]), "W/\"l2\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(
        told(&h).await,
        [(1, "new_head".to_owned(), vec!["octo".to_owned()])]
    );
    let others = ["review_requested".to_owned(), "new_comment".to_owned()];
    assert_eq!(also(&h).await, [(1, others.to_vec())]);
    let wire = serde_json::to_value(watch_events(&h).await.last().unwrap()).unwrap();
    assert_eq!(
        wire["changes"][0]["also"],
        serde_json::json!(["review_requested", "new_comment"]),
        "{wire}"
    );
    assert_eq!(stored(&h).await["pulls"]["1"]["comments"], 1);

    let quiet = pass(&h).await;
    assert_eq!(
        (quiet.watch.requested, quiet.watch.unchanged),
        (1, 1),
        "{quiet:?}"
    );
    assert_eq!(watch_events(&h).await.len(), 1, "told once, in full");
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
        (3, 1),
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

/// Past 100 open pull requests every walk asks every page, each on its own
/// ETag: news on a later page is read though the first is unchanged.
#[tokio::test]
async fn every_page_is_asked_each_conditional_on_its_own_etag() {
    let github = Github::new();
    let h = two_pages(&github).await;
    let unchanged = pass(&h).await;
    assert_eq!(
        (unchanged.watch.requested, unchanged.watch.unchanged),
        (2, 1),
        "one conditional GET a page: {unchanged:?}"
    );

    let mut later: Vec<serde_json::Value> = (2..=5).rev().map(|n| pull(n, "a")).collect();
    later.push(pull_at(1, "a", T1));
    github.serve_named("pulls-2", later.into(), "W/\"u2\"");
    github.serve_count("comments-1", 1);
    let before = github.calls().len();
    let read = pass(&h).await;
    assert_eq!(
        (read.watch.requested, read.watch.triggers),
        (4, 1),
        "two pages, then #1's two counts: {read:?}"
    );
    assert_eq!(told(&h).await, [(1, "new_comment".to_owned(), vec![])]);
    let calls = github.calls();
    assert!(calls[before].contains("If-None-Match: W/\"t1\""));
    assert!(calls[before].ends_with(&format!("{LIST}1")));
    assert!(calls[before + 1].contains("If-None-Match: W/\"u1\""));
    assert!(calls[before + 1].ends_with(&format!("{LIST}2")));
    github.assert_only_reads();
}

/// A pull request closed on page 1 after it was read pulls #5 up from page
/// 2: that walk misses #5 and tells nothing, and the next tells its news.
#[tokio::test]
async fn a_page_shift_hides_a_pull_request_for_one_walk_and_loses_no_news() {
    let github = Github::new();
    let h = two_pages(&github).await;
    github.serve_named("pulls-2", pulls((1..=4).rev(), T0), "W/\"u2\"");
    let shifted = pass(&h).await;
    assert_eq!(
        (shifted.watch.requested, shifted.watch.triggers),
        (2, 0),
        "{shifted:?}"
    );
    assert!(watch_events(&h).await.is_empty(), "missing once is no news");
    let tracked = listed(&h).await[0].tracked_pull_requests;
    assert_eq!(tracked, Some(105), "#5 keeps its row");

    let mut first: Vec<serde_json::Value> = (6..=105)
        .rev()
        .filter(|n| *n != 50)
        .map(|n| pull(n, "a"))
        .collect();
    first.push(pull_at(5, "pushed", T1));
    github.serve_list(first.into(), "W/\"t2\"");
    let next = pass(&h).await;
    assert_eq!(
        (next.watch.requested, next.watch.triggers),
        (4, 1),
        "two pages, then #5's two counts: {next:?}"
    );
    assert_eq!(
        told(&h).await,
        [(5, "new_head".to_owned(), vec![])],
        "compared with its row, not first seen"
    );
    assert_eq!(pass(&h).await.watch.triggers, 0, "told once");
}

/// A walk the request budget cuts short stores nothing; it goes on five
/// minutes later, whatever its interval, with what it had counted.
#[tokio::test]
async fn a_walk_cut_by_the_budget_stores_nothing_and_resumes() {
    let github = Github::new();
    github.serve_list(pulls((1..=30).rev(), T0), "W/\"f1\"");
    let h = harness(&github).await;
    let hourly = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: Some(60),
        remove: false,
    };
    h.orchestrator(hourly).await.unwrap();
    let written = h.ctx.store.lock().await.rows_written();
    let mut sync = settled_sync().await;
    tokio::time::pause();
    let first = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        (first.watch.requested, first.watch.incomplete),
        (25, 1),
        "the list, then 12 pull requests' counts: {first:?}"
    );
    assert_eq!(
        h.ctx.store.lock().await.rows_written(),
        written,
        "a cut walk stores nothing"
    );
    assert_eq!(listed(&h).await[0].tracked_pull_requests, None);

    tokio::time::advance(Duration::from_secs(4 * 60)).await;
    let early = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(early.watch.requested, 0, "never before the floor");
    tokio::time::advance(Duration::from_secs(60)).await;
    let second = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        (second.watch.requested, second.watch.incomplete),
        (25, 1),
        "{second:?}"
    );
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let third = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        (third.watch.requested, third.watch.incomplete),
        (13, 0),
        "the list, then the last 6: the 24 counted before are kept: {third:?}"
    );
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(30));
    assert!(watch_events(&h).await.is_empty(), "a first walk");

    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let whole = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(whole.watch.requested, 0, "a whole walk waits its hour");
    github.assert_only_reads();
}

/// A checkout re-pointed at another repository during a walk: the walk of
/// the old one is discarded, and the next pass starts over on the new one.
#[tokio::test]
async fn a_walk_is_discarded_when_the_origin_moves_meanwhile() {
    let github = Github::new();
    let h = watched(&github).await;
    github.serve_list(
        serde_json::json!([opened(2, "b"), pull(1, "a")]),
        "W/\"l2\"",
    );
    let hold = github.path("hold-pulls-1");
    std::fs::write(&hold, "").unwrap();
    let mut sync = settled_sync().await;
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
    let kept = stored(&h).await;
    assert_eq!(kept["repository"], "o/r", "the walk was not stored");
    assert_eq!(kept["pulls"].as_object().unwrap().len(), 1);
    let before = listed(&h).await;
    assert_eq!(before[0].repository.as_deref(), Some("o/s"));
    assert_eq!(
        (before[0].tracked_pull_requests, before[0].fetched_at),
        (None, None),
        "o/r's snapshot is not shown as o/s's"
    );
    let rebased = pass(&h).await;
    assert_eq!(
        (rebased.watch.changed, rebased.watch.triggers),
        (1, 0),
        "{rebased:?}"
    );
    let after = listed(&h).await;
    assert_eq!(after[0].repository.as_deref(), Some("o/s"));
    assert_eq!(after[0].tracked_pull_requests, Some(2), "a first walk");
    let last = github.calls().pop().unwrap();
    assert!(last.contains("repos/o/s/pulls"), "{last}");
}

/// A pull request closed in one walk and open again in the next is compared
/// with its row: unchanged, it is no news.
#[tokio::test]
async fn a_pull_request_reopened_by_the_next_walk_is_no_news() {
    let github = Github::new();
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"o1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    pass(&h).await;
    github.serve_list(serde_json::json!([pull(2, "b")]), "W/\"o2\"");
    assert_eq!(pass(&h).await.watch.triggers, 0, "a closing is no news");
    let tracked = listed(&h).await[0].tracked_pull_requests;
    assert_eq!(tracked, Some(2), "missing once, #1 keeps its row");
    let reopened = serde_json::json!([pull(2, "b"), pull_at(1, "a", T1)]);
    github.serve_list(reopened, "W/\"o3\"");
    let read = pass(&h).await;
    assert_eq!(
        (read.watch.requested, read.watch.triggers),
        (3, 0),
        "the list, then #1's two counts: {read:?}"
    );
    assert!(watch_events(&h).await.is_empty());
}

/// A pull request the watch first sees is told once: as new when made since
/// the last walk, else (reopened, or long hidden) as seen.
#[tokio::test]
async fn a_first_seen_pull_request_is_told_once_as_new_or_as_seen() {
    let github = Github::new();
    let h = watched(&github).await;
    let list = serde_json::json!([opened(8, "d"), pull_at(7, "c", T1), pull(1, "a")]);
    github.serve_list(list.clone(), "W/\"c2\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(
        told(&h).await,
        [
            (7, "seen_pull_request".to_owned(), vec![]),
            (8, "new_pull_request".to_owned(), vec![])
        ]
    );
    github.serve_list(list, "W/\"c3\"");
    assert_eq!(pass(&h).await.watch.triggers, 0, "each is told once");
}

/// A watch removed while its list is read sends nothing more: the counts
/// the changed list asks for are not sent.
#[tokio::test]
async fn a_watch_removed_during_a_walk_sends_no_more_requests() {
    let github = Github::new();
    let h = watched(&github).await;
    let counted = github.calls().len();
    github.serve_list(serde_json::json!([pull_at(1, "a", T1)]), "W/\"v2\"");
    let hold = github.path("hold-pulls-1");
    std::fs::write(&hold, "").unwrap();
    let remove = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: None,
        remove: true,
    };
    let mut sync = settled_sync().await;
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
    let calls = &github.calls()[counted..];
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("/comments") || c.contains("/reviews")),
        "{calls:?}"
    );
    assert_eq!(watches(&h).await, 0);
}

/// A walk GitHub fails stores nothing, and the next one reads on.
#[tokio::test]
async fn a_failed_walk_stores_nothing() {
    let github = Github::new();
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    let written = h.ctx.store.lock().await.rows_written();
    let failed = pass(&h).await;
    assert_eq!(
        (failed.watch.requested, failed.watch.failed),
        (1, 1),
        "the stub has no list: {failed:?}"
    );
    assert_eq!(h.ctx.store.lock().await.rows_written(), written);
    github.serve_list(serde_json::json!([pull(1, "a")]), "W/\"l1\"");
    assert_eq!(pass(&h).await.watch.changed, 1);
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(1));
}

/// A record of another format keeps what was asked and is walked afresh: its
/// first walk tells nothing, and the next tells its news.
#[tokio::test]
async fn a_record_of_another_format_is_walked_afresh() {
    let github = Github::new();
    let h = watched(&github).await;
    {
        let mut store = h.ctx.store.lock().await;
        let (key, raw) = store.kv_prefix(KEY_PREFIX).unwrap().remove(0);
        let mut record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let record_map = record.as_object_mut().unwrap();
        for gone in ["version", "pages", "walked_at"] {
            record_map.remove(gone);
        }
        record_map.insert("etag".into(), "W/\"l1\"".into());
        record_map.insert("interval_minutes".into(), 30.into());
        let mark = serde_json::json!({"head": "a", "updated_at": T0, "settled": true});
        record_map.insert("pulls".into(), serde_json::json!({"1": mark}));
        store.kv_set(&key, &record.to_string()).unwrap();
    }
    assert_eq!(listed(&h).await[0].tracked_pull_requests, None);
    github.serve_list(
        serde_json::json!([opened(2, "b"), pull(1, "a")]),
        "W/\"e2\"",
    );
    let afresh = pass(&h).await;
    assert_eq!(
        (afresh.watch.requested, afresh.watch.triggers),
        (5, 0),
        "an unconditional list, then two pull requests' counts: {afresh:?}"
    );
    let after = listed(&h).await;
    assert_eq!(
        (after[0].tracked_pull_requests, after[0].interval_minutes),
        (Some(2), 30)
    );
    let list = serde_json::json!([opened(3, "c"), opened(2, "b"), pull(1, "a")]);
    github.serve_list(list, "W/\"e3\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(told(&h).await, [(3, "new_pull_request".to_owned(), vec![])]);
}

/// Past the 20 changes a trigger tells in full, every other one is named by
/// kind and number: the snapshot holds them all, so none is told later.
#[tokio::test]
async fn a_trigger_names_every_change_past_those_it_tells_in_full() {
    let github = Github::new();
    let h = watched(&github).await;
    let mut list: Vec<serde_json::Value> = (2..=23).rev().map(|n| opened(n, "b")).collect();
    list.push(pull(1, "a"));
    github.serve_list(list.into(), "W/\"m2\"");
    assert_eq!(walk_whole(&h).await.watch.triggers, 1);
    let events = watch_events(&h).await;
    assert_eq!(events.len(), 1, "one trigger: {events:?}");
    let TriggerEvent::GithubWatch { changes, rest, .. } = &events[0] else {
        unreachable!()
    };
    let full: Vec<u64> = changes.iter().map(|c| c.number).collect();
    assert_eq!(full, (2..=21).collect::<Vec<u64>>());
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert_eq!(rest["new_pull_request"], [22, 23]);
    assert_eq!(pass(&h).await.watch.triggers, 0, "all 22 are told");
    assert_eq!(watch_events(&h).await.len(), 1);
}

/// A pull request one walk misses and the next, of 304s, misses too is
/// closed: listed again it is first seen, and told.
#[tokio::test]
async fn a_pull_request_missing_from_a_walk_and_a_walk_of_304s_is_closed() {
    let github = Github::new();
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"n1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    pass(&h).await;
    github.serve_list(serde_json::json!([pull(2, "b")]), "W/\"n2\"");
    assert_eq!(pass(&h).await.watch.triggers, 0, "a closing is no news");
    let quiet = pass(&h).await;
    assert_eq!(
        (quiet.watch.requested, quiet.watch.unchanged),
        (1, 1),
        "{quiet:?}"
    );
    github.serve_list(serde_json::json!([pull(2, "b"), pull(1, "a")]), "W/\"n3\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(
        told(&h).await,
        [(1, "seen_pull_request".to_owned(), vec![])],
        "two completed walks missed #1"
    );
}

/// A pull request made before a walk of 304s and first listed after it is
/// seen, not new: that walk is the last completed one.
#[tokio::test]
async fn a_walk_of_304s_moves_the_clock_a_new_pull_request_is_told_against() {
    let github = Github::new();
    let h = watched(&github).await;
    let made = archon_core::now();
    assert_eq!(pass(&h).await.watch.unchanged, 1);
    let mut late = pull_at(2, "b", T1);
    late["created_at"] = serde_json::to_value(made).unwrap();
    github.serve_list(serde_json::json!([late, pull(1, "a")]), "W/\"q2\"");
    assert_eq!(pass(&h).await.watch.triggers, 1);
    assert_eq!(
        told(&h).await,
        [(2, "seen_pull_request".to_owned(), vec![])]
    );
}

/// Every pull request moves between a cut walk and its resumption. The counts read are kept, so
/// the walk completes and tells the head it lists with the comment it counted; the next counts again.
#[tokio::test]
async fn a_resumed_walk_gets_further_though_what_it_counted_moved_again() {
    let github = Github::new();
    github.serve_list(pulls((1..=14).rev(), T0), "W/\"g1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    walk_whole(&h).await;
    github.serve_count("comments-1", 1);
    github.serve_list(pulls((1..=14).rev(), T1), "W/\"g2\"");
    let mut sync = settled_sync().await;
    tokio::time::pause();
    let cut = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        (cut.watch.requested, cut.watch.incomplete),
        (25, 1),
        "the list, then #1 to #12: {cut:?}"
    );

    github.serve_count("comments-2", 1);
    let mut pushed = pulls((1..=14).rev(), T2);
    pushed[13]["head"]["sha"] = "b".into();
    github.serve_list(pushed, "W/\"g3\"");
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let resumed = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        (resumed.watch.requested, resumed.watch.incomplete),
        (5, 0),
        "the list, then #13 and #14 only: {resumed:?}"
    );
    assert_eq!(
        told(&h).await,
        [(1, "new_head".to_owned(), vec![])],
        "the head this walk listed is told by it; #2 was counted before its comment"
    );
    assert_eq!(
        also(&h).await,
        [(1, vec!["new_comment".to_owned()])],
        "the comment the cut walk counted is told with the head, not stored untold"
    );

    taken(&h).await;
    let before = github.calls().len();
    let mut back = pulls((1..=14).rev(), T2);
    back[13] = pull_at(1, "a", T3);
    github.serve_list(back, "W/\"g4\"");
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let next = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!(
        (next.watch.requested, next.watch.triggers),
        (25, 1),
        "the list, then the 12 rows kept: {next:?}"
    );
    let list = &github.calls()[before];
    assert!(list.ends_with(&format!("{LIST}1")), "{list}");
    assert!(!list.contains("If-None-Match"), "asked afresh: {list}");
    assert_eq!(
        told(&h).await,
        [
            (1, "new_head".to_owned(), vec![]),
            (2, "new_comment".to_owned(), vec![])
        ],
        "#1 was pushed back to its first head, and #2 is counted again"
    );
    assert!(also(&h).await.is_empty(), "#1's comment was told before");
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let quiet = busy(sync.pass(&h.ctx)).await.unwrap();
    assert_eq!((quiet.watch.requested, quiet.watch.unchanged), (1, 1));
}

/// A restarted archond does not know when a watch was last asked, so it
/// waits the five-minute floor before asking any.
#[tokio::test]
async fn a_restarted_sync_asks_no_watch_before_the_floor() {
    let github = Github::new();
    let h = watched(&github).await;
    let calls = github.calls().len();
    tokio::time::pause();
    let mut restarted = GithubSync::new(MIN_INTERVAL);
    let at_once = busy(restarted.pass(&h.ctx)).await.unwrap();
    assert_eq!(at_once.watch.requested, 0, "{at_once:?}");
    tokio::time::advance(Duration::from_secs(4 * 60)).await;
    let early = busy(restarted.pass(&h.ctx)).await.unwrap();
    assert_eq!(early.watch.requested, 0, "{early:?}");
    assert_eq!(github.calls().len(), calls, "nothing was asked");
    tokio::time::advance(Duration::from_secs(60)).await;
    let due = busy(restarted.pass(&h.ctx)).await.unwrap();
    assert_eq!(due.watch.requested, 1, "five minutes on: {due:?}");
}

/// A 304 to a page asked without an ETag is GitHub's mistake: the walk
/// fails, nothing is stored, and the next walk reads on.
#[tokio::test]
async fn a_304_to_an_unconditional_page_fails_the_walk() {
    let github = Github::new();
    github.serve_list(serde_json::json!([pull(1, "a")]), "W/\"l1\"");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    let written = h.ctx.store.lock().await.rows_written();
    std::fs::write(github.path("not-modified-pulls-1"), "").unwrap();
    let failed = pass(&h).await;
    assert_eq!(
        (failed.watch.requested, failed.watch.failed),
        (1, 1),
        "{failed:?}"
    );
    assert_eq!(h.ctx.store.lock().await.rows_written(), written);
    assert_eq!(listed(&h).await[0].tracked_pull_requests, None);
    std::fs::remove_file(github.path("not-modified-pulls-1")).unwrap();
    assert_eq!(pass(&h).await.watch.changed, 1);
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(1));
}

/// A stored record no write would make is refused where it is read, by the
/// pass and by the tool alike; removing the watch clears it.
#[tokio::test]
async fn a_malformed_stored_watch_is_refused_until_it_is_removed() {
    let github = Github::new();
    let h = watched(&github).await;
    {
        let mut store = h.ctx.store.lock().await;
        let (key, raw) = store.kv_prefix(KEY_PREFIX).unwrap().remove(0);
        let mut record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        record["interval_minutes"] = 0.into();
        store.kv_set(&key, &record.to_string()).unwrap();
    }
    let calls = github.calls().len();
    let refused = h.orchestrator(watch_repo(REPO_NAME)).await.unwrap_err();
    assert!(
        refused.to_string().contains("an interval of 0 minutes"),
        "{refused}"
    );
    let failed = settled_sync().await.pass(&h.ctx).await.unwrap_err();
    assert!(failed.to_string().contains("repository watch"), "{failed}");
    assert_eq!(github.calls().len(), calls, "nothing is read under it");
    let remove = Request::WatchRepo {
        repo: RepoRef(REPO_NAME.into()),
        interval_minutes: None,
        remove: true,
    };
    h.orchestrator(remove).await.unwrap();
    assert_eq!(watches(&h).await, 0);
    assert_eq!(pass(&h).await.requested, 0);
}

/// A list GitHub sends is a changed walk even when it is empty and unmarked as
/// before: only 304s make an unchanged one, and no ETag is sent that was not given.
#[tokio::test]
async fn an_empty_list_sent_again_is_not_a_walk_of_304s() {
    let github = Github::new();
    github.serve_list(serde_json::json!([]), "");
    let h = harness(&github).await;
    h.orchestrator(watch_repo(REPO_NAME)).await.unwrap();
    let first = pass(&h).await;
    assert_eq!((first.watch.requested, first.watch.changed), (1, 1));
    assert_eq!(listed(&h).await[0].tracked_pull_requests, Some(0));
    let again = pass(&h).await;
    assert_eq!(
        (
            again.watch.requested,
            again.watch.changed,
            again.watch.unchanged,
            again.watch.triggers
        ),
        (1, 1, 0, 0),
        "{again:?}"
    );
    let last = github.calls().pop().unwrap();
    assert!(!last.contains("If-None-Match"), "{last}");
    assert_eq!(stored(&h).await["walks"], 2);
}

/// A stored walk count with no room for one more fails each walk that would count
/// past it: nothing is written or overflows, and the watch still waits its interval.
#[tokio::test]
async fn a_stored_walk_count_with_no_room_left_fails_the_walk() {
    let github = Github::new();
    let h = watched(&github).await;
    {
        let mut store = h.ctx.store.lock().await;
        let (key, raw) = store.kv_prefix(KEY_PREFIX).unwrap().remove(0);
        let mut record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        record["walks"] = u64::MAX.into();
        store.kv_set(&key, &record.to_string()).unwrap();
    }
    let before = stored(&h).await;
    let calls = github.calls().len();
    let mut sync = settled_sync().await;
    let quiet = sync.pass(&h.ctx).await.unwrap_err();
    assert!(quiet.to_string().contains("walk count"), "{quiet}");
    let waiting = sync.pass(&h.ctx).await.unwrap();
    assert_eq!(
        waiting.watch.requested, 0,
        "asked a moment ago: {waiting:?}"
    );
    assert_eq!(github.calls().len(), calls + 1, "the one conditional GET");
    github.serve_list(
        serde_json::json!([opened(2, "b"), pull(1, "a")]),
        "W/\"x2\"",
    );
    let changed = settled_sync().await.pass(&h.ctx).await.unwrap_err();
    assert!(changed.to_string().contains("walk count"), "{changed}");
    assert_eq!(stored(&h).await, before, "neither walk was written");
    assert!(watch_events(&h).await.is_empty());
}
