//! Repository watches (A18) over the stub `gh` of `github_sync`: one
//! conditional `GET` of a registered repository's open pull requests per
//! interval, and a `github_watch` trigger, with no operator standing, only
//! when the list changed.

use crate::common;
use crate::github_sync::{Github, harness, reference};

use archon_core::config::Config;
use archon_core::protocol::github_links::LinkSource;
use archon_core::protocol::{GithubKind, GithubLink, LinkRelation, Reply, RepoRef, Request};
use archon_daemon::github_sync::watch::{KEY_PREFIX, WATCH_TRUST};
use archon_daemon::github_sync::{GithubSync, MIN_INTERVAL, Pass};
use archon_daemon::triggers::{self, TriggerEvent};
use common::{Harness, REPO_NAME};

fn pull(number: u64, head: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number, "title": format!("PR {number}"), "user": {"login": "devArchOverclocked"},
        "html_url": format!("https://github.com/o/r/pull/{number}"),
        "head": {"sha": head}, "updated_at": "2026-11-01T00:00:00Z", "requested_reviewers": [],
    })
}

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
    assert!(
        last.ends_with("repos/o/r/pulls?state=open&per_page=100"),
        "{last}"
    );
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
