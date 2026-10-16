//! Archon's GitHub watcher against the fake aetherd: operator edits as a
//! compare-and-swap, the target following the orchestrator, status, the
//! trust boundary, and `github_reply` through a fake `gh`.

mod link_common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use archon_aether::fake::Scripted;
use archon_core::config::{AetherMode, Config};
use archon_core::error::ArchonError;
use archon_core::github::{
    AllowedUsers, GithubRepo, GithubThread, GithubWatch, MENTION_REGEX, OperatorUsers,
    REPLY_MARKER, WatchTarget, WatcherChange,
};
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::{github, handlers, supervisor};
use archon_queue::store::keys;
use link_common::{Harness, WAIT};
use serde_json::{Value, json};

const ORCH: &str = "agt_0001";
const ORCH_2: &str = "agt_0002";

/// Aether's watcher settings: a revision and the config, edited only by a
/// configure naming the current revision, as Aether does.
type Watcher = Arc<Mutex<(u64, Value)>>;

fn fixed(name: &str, agent: &str) -> Value {
    json!({"repository": name, "enabled": true, "agent": agent, "dispatch": {"mode": "fixed"}})
}

fn thread_mode(name: &str) -> Value {
    json!({"repository": name, "enabled": true, "agent": null,
           "dispatch": {"mode": "thread", "role": "r", "objective": "o"}})
}

fn serve_watcher(harness: &Harness, repositories: Value) -> Watcher {
    let config = json!({
        "enabled": true,
        "mention_regex": "(?i)(?:^|[^a-z0-9_@])@aether(?:$|[^a-z0-9_-])",
        "label_regex": "(?i)aether",
        "poll_interval_secs": 120,
        "repositories": repositories,
    });
    let watcher: Watcher = Arc::new(Mutex::new((1, config)));
    let status = |(revision, config): &(u64, Value)| {
        Scripted::reply(
            "github_watcher",
            json!({"revision": revision, "config": config, "repositories": []}),
        )
    };
    let read = watcher.clone();
    harness.fake.set_responder(
        "get_github_watcher",
        Arc::new(move |_| status(&read.lock().unwrap())),
    );
    let write = watcher.clone();
    harness.fake.set_responder(
        "configure_github_watcher",
        Arc::new(move |cmd| {
            let mut state = write.lock().unwrap();
            if cmd["expected_revision"] != state.0 {
                return Scripted::refuse(
                    "invalid_argument",
                    "GitHub watcher configuration changed; refresh before editing",
                );
            }
            *state = (state.0 + 1, cmd["config"].clone());
            status(&state)
        }),
    );
    // Every orchestrator started with the policy unless a test says otherwise.
    serve_orchestrators(harness, &[]);
    watcher
}

/// The orchestrators Aether runs that were started with an older Archon's
/// instructions, which lack the GitHub policy.
type Unprepared = Arc<Mutex<Vec<&'static str>>>;

fn orchestrator(id: &str, instructions: &str) -> Value {
    let mut agent = archon_aether::fake::agent(id, None);
    agent["role"] = json!(supervisor::ORCHESTRATOR_ROLE);
    agent["config"] = json!({"instructions": instructions, "tools": {"mode": "all"}});
    agent
}

fn serve_orchestrators(harness: &Harness, unprepared: &[&'static str]) -> Unprepared {
    let state: Unprepared = Arc::new(Mutex::new(unprepared.to_vec()));
    let read = state.clone();
    harness.fake.set_responder(
        "get_agent",
        Arc::new(move |cmd| {
            let id = cmd["id"].as_str().unwrap();
            let instructions = if read.lock().unwrap().contains(&id) {
                "# An older Archon prompt"
            } else {
                supervisor::github_policy()
            };
            Scripted::reply("agent", orchestrator(id, instructions))
        }),
    );
    state
}

fn targets(watcher: &Watcher) -> Vec<Value> {
    let state = watcher.lock().unwrap();
    let repos = state.1["repositories"].as_array().unwrap();
    repos.iter().map(|r| r["agent"].clone()).collect()
}

async fn configure(
    harness: &Harness,
    client: ClientKind,
    change: WatcherChange,
) -> Result<Reply, ArchonError> {
    let request = Request::ConfigureGithubWatcher { change };
    handlers::handle(&harness.ctx, client, request).await
}

fn add(name: &str) -> WatcherChange {
    WatcherChange::Add {
        repository: GithubRepo::parse(name).unwrap(),
        allowed_users: None,
    }
}

#[tokio::test]
async fn adding_a_repository_points_it_at_the_live_orchestrator() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let watcher = serve_watcher(&harness, json!([]));
    let reply = configure(&harness, ClientKind::ArchonCli, add("Owner/Repo")).await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("configure replied {reply:?}");
    };
    assert_eq!(view.repositories[0].repository, "owner/repo");
    assert_eq!(view.repositories[0].target, WatchTarget::Orchestrator);
    assert_eq!(view.mention_regex, MENTION_REGEX);
    let sent = harness.fake.received_named("configure_github_watcher");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["expected_revision"], 1, "an edit of what was read");
    assert_eq!(targets(&watcher), [json!(ORCH)]);
}

fn add_allowing(name: &str, logins: &[&str]) -> WatcherChange {
    WatcherChange::Add {
        repository: GithubRepo::parse(name).unwrap(),
        allowed_users: Some(AllowedUsers::parse(logins).unwrap()),
    }
}

#[tokio::test]
async fn add_with_an_allowlist_saves_both_in_one_write() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let watcher = serve_watcher(&harness, json!([]));
    let change = add_allowing("Example-Org/example-repo", &["monaLisa", "Hubot"]);
    let reply = configure(&harness, ClientKind::ArchonCli, change).await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("configure replied {reply:?}");
    };
    let team = vec!["monalisa".to_owned(), "hubot".to_owned()];
    assert_eq!(view.repositories[0].allowed_users, Some(team));
    let sent = harness.fake.received_named("configure_github_watcher");
    assert_eq!(
        sent.len(),
        1,
        "the repository is never watched without its list"
    );
    assert_eq!(
        sent[0]["config"]["repositories"][0]["allowed_users"],
        json!(["monalisa", "hubot"])
    );

    let clear = WatcherChange::Allow {
        repository: GithubRepo::parse("example-org/example-repo").unwrap(),
        allowed_users: None,
    };
    let reply = configure(&harness, ClientKind::ArchonCli, clear).await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("clearing replied {reply:?}");
    };
    assert_eq!(view.repositories[0].allowed_users, None);
    let stored = &watcher.lock().unwrap().1["repositories"][0];
    assert!(stored.get("allowed_users").is_none(), "{stored}");
}

/// An aetherd still on protocol 14 would drop the list and let every writer
/// dispatch: the edit is refused whole, before it starts an orchestrator or
/// reads the watcher, and other edits still apply.
#[tokio::test]
async fn an_allowlist_is_refused_while_aetherd_speaks_protocol_14() {
    let harness = Harness::new().await;
    harness.fake.set_protocol_window(13, 14);
    let client = harness.connect().await;
    assert_eq!(client.welcome().protocol_version, 14);
    let watcher = serve_watcher(&harness, json!([]));
    let change = add_allowing("example-org/example-repo", &["monalisa"]);
    let error = configure(&harness, ClientKind::ArchonCli, change)
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("protocol 14") && text.contains("allowlist"),
        "{text}"
    );
    for command in [
        "create_agent",
        "get_github_watcher",
        "configure_github_watcher",
    ] {
        let sent = harness.fake.received_named(command);
        assert!(sent.is_empty(), "{command} was sent: {sent:?}");
    }
    assert_eq!(watcher.lock().unwrap().0, 1, "nothing was written");
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;

    let reply = configure(
        &harness,
        ClientKind::ArchonCli,
        add("example-org/example-repo"),
    )
    .await;
    assert!(matches!(reply, Ok(Reply::GithubWatcher(_))), "{reply:?}");
    assert_eq!(targets(&watcher), [json!(ORCH)]);
}

fn operators(logins: &[&str]) -> WatcherChange {
    WatcherChange::Operators {
        operator_users: Some(OperatorUsers::parse(logins).unwrap()),
    }
}

const NO_OPERATORS: WatcherChange = WatcherChange::Operators {
    operator_users: None,
};

#[tokio::test]
async fn operators_replaces_or_clears_the_list_and_nothing_else() {
    let harness = Harness::new().await;
    let client = harness.connect().await;
    assert_eq!(client.welcome().protocol_version, 18);
    let mut narrowed = fixed("o/narrow", ORCH);
    narrowed["allowed_users"] = json!(["monalisa"]);
    let watcher = serve_watcher(&harness, json!([narrowed, fixed("o/open", ORCH)]));
    let repositories = watcher.lock().unwrap().1["repositories"].clone();

    let set = operators(&["Octocat", "monaLisa", "OCTOCAT"]);
    let reply = configure(&harness, ClientKind::ArchonCli, set).await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("setting replied {reply:?}");
    };
    let both = vec!["octocat".to_owned(), "monalisa".to_owned()];
    assert_eq!(view.operator_users, Some(both));
    assert_eq!(view.repositories[0].excluded_operators, ["octocat"]);
    assert!(view.repositories[1].excluded_operators.is_empty());
    let stored = watcher.lock().unwrap().1.clone();
    assert_eq!(stored["operator_users"], json!(["octocat", "monalisa"]));
    assert_eq!(stored["repositories"], repositories, "no route changed");

    let reply = configure(&harness, ClientKind::ArchonCli, operators(&["monalisa"])).await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("replacing replied {reply:?}");
    };
    assert_eq!(view.operator_users, Some(vec!["monalisa".to_owned()]));
    assert!(view.repositories[0].excluded_operators.is_empty());

    let reply = configure(&harness, ClientKind::ArchonCli, NO_OPERATORS).await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("clearing replied {reply:?}");
    };
    assert_eq!(view.operator_users, None);
    let stored = watcher.lock().unwrap().1.clone();
    assert!(stored.get("operator_users").is_none(), "{stored}");
    assert_eq!(stored["repositories"], repositories);
    let started = harness.fake.received_named("create_agent");
    assert!(started.is_empty(), "the list needs no orchestrator");
}

/// Every read-modify-write of the watcher sends back the list it read.
#[tokio::test]
async fn every_watcher_change_keeps_the_operator_logins() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let watcher = serve_watcher(&harness, json!([fixed("o/a", ORCH), fixed("o/b", ORCH)]));
    let list = json!(["octocat", "monalisa"]);
    watcher.lock().unwrap().1["operator_users"] = list.clone();
    let allow = |allowed_users| WatcherChange::Allow {
        repository: GithubRepo::parse("o/a").unwrap(),
        allowed_users,
    };
    let changes = [
        add("o/new"),
        add_allowing("o/a", &["octocat"]),
        allow(Some(AllowedUsers::parse(&["monalisa"]).unwrap())),
        allow(None),
        WatcherChange::Disable {
            repository: repository("o/b"),
        },
        WatcherChange::Enable {
            repository: repository("o/b"),
        },
        WatcherChange::Disable { repository: None },
        WatcherChange::Enable { repository: None },
        WatcherChange::Configure {
            mention_regex: Some("(?i)@bot".into()),
            label_regex: Some("(?i)bot".into()),
            poll_interval_secs: Some(300),
        },
        WatcherChange::Remove {
            repository: GithubRepo::parse("o/b").unwrap(),
        },
    ];
    let count = changes.len();
    for change in changes {
        let reply = configure(&harness, ClientKind::ArchonCli, change.clone()).await;
        let Ok(Reply::GithubWatcher(view)) = reply else {
            panic!("{change:?} replied {reply:?}");
        };
        let shown = serde_json::to_value(&view.operator_users).unwrap();
        assert_eq!(shown, list, "{change:?} shows another list");
        let stored = watcher.lock().unwrap().1["operator_users"].clone();
        assert_eq!(stored, list, "{change:?} dropped the operator logins");
    }
    // A restarted orchestrator's repoint is archond's own write.
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    assert!(github::repoint_now(&harness.ctx).await.unwrap());
    assert_eq!(targets(&watcher), [json!(ORCH_2), json!(ORCH_2)]);
    let sent = harness.fake.received_named("configure_github_watcher");
    assert_eq!(sent.len(), count + 1, "one write per change");
    for write in sent {
        assert_eq!(write["config"]["operator_users"], list, "{write}");
    }
}

/// An aetherd below protocol 17 cannot store the list: setting it is refused
/// whole, before anything is sent, and every other edit still applies.
#[tokio::test]
async fn operator_logins_are_refused_while_aetherd_speaks_protocol_15_or_16() {
    for speaking in [15, 16] {
        let harness = Harness::new().await;
        harness.fake.set_protocol_window(speaking - 1, speaking);
        let client = harness.connect().await;
        assert_eq!(client.welcome().protocol_version, speaking);
        let watcher = serve_watcher(&harness, json!([]));
        let error = configure(&harness, ClientKind::ArchonCli, operators(&["octocat"]))
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains(&format!("speaks Aether protocol {speaking}"))
                && text.contains("operator logins (they need 17)")
                && text.contains("archon daemon stop --aether")
                && text.contains("Nothing was changed"),
            "{text}"
        );
        for command in [
            "create_agent",
            "get_github_watcher",
            "configure_github_watcher",
        ] {
            let sent = harness.fake.received_named(command);
            assert!(sent.is_empty(), "{command} was sent: {sent:?}");
        }
        assert_eq!(watcher.lock().unwrap().0, 1, "nothing was written");

        harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
        let change = add_allowing("example-org/example-repo", &["monalisa"]);
        let reply = configure(&harness, ClientKind::ArchonCli, change).await;
        assert!(matches!(reply, Ok(Reply::GithubWatcher(_))), "{reply:?}");
        let reply = configure(&harness, ClientKind::ArchonCli, NO_OPERATORS).await;
        let Ok(Reply::GithubWatcher(view)) = reply else {
            panic!("clearing nothing replied {reply:?}");
        };
        assert_eq!(view.operator_users, None);
        let stored = watcher.lock().unwrap().1.clone();
        assert!(stored.get("operator_users").is_none(), "{stored}");
    }
}

/// A watcher write Aether answers without the list archond sent is an error.
#[tokio::test]
async fn a_write_that_loses_the_operator_logins_is_reported() {
    let harness = Harness::new().await;
    harness.connect().await;
    let watcher = serve_watcher(&harness, json!([fixed("o/a", ORCH)]));
    watcher.lock().unwrap().1["operator_users"] = json!(["octocat"]);
    let mut lost = watcher.lock().unwrap().1.clone();
    lost.as_object_mut().unwrap().remove("operator_users");
    harness.fake.script(
        "configure_github_watcher",
        Scripted::reply(
            "github_watcher",
            json!({"revision": 2, "config": lost, "repositories": []}),
        ),
    );
    let disable = WatcherChange::Disable { repository: None };
    let error = configure(&harness, ClientKind::ArchonCli, disable)
        .await
        .unwrap_err();
    assert!(matches!(error, ArchonError::Protocol(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("operator logins") && text.contains("octocat"),
        "{text}"
    );
}

#[tokio::test]
async fn status_is_quiet_until_a_repository_is_watched() {
    let harness = Harness::new().await;
    harness.connect().await;
    serve_watcher(&harness, json!([]));
    let Ok(Reply::Status(status)) = harness_status(&harness).await else {
        panic!("status failed");
    };
    assert_eq!(status.github, None, "nothing to report");
    assert_eq!(harness.fake.received_named("get_github_watcher").len(), 1);
}

fn assert_restart_advised(result: Result<impl std::fmt::Debug, ArchonError>) {
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("archon orchestrator restart"),
        "{err}"
    );
}

fn repository(name: &str) -> Option<GithubRepo> {
    Some(GithubRepo::parse(name).unwrap())
}

/// A pre-GitHub orchestrator may be mid-turn, and Aether steers new inbox
/// items into a running turn with the instructions it started with: patching
/// them would not reach it, so Archon refuses it rather than patching.
#[tokio::test]
async fn an_orchestrator_started_before_github_gets_no_requests_until_restarted() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let watcher = serve_watcher(
        &harness,
        json!([{
            "repository": "o/r", "enabled": false, "agent": ORCH, "dispatch": {"mode": "fixed"}
        }]),
    );
    serve_orchestrators(&harness, &[ORCH]);
    let enable_repo = WatcherChange::Enable {
        repository: repository("o/r"),
    };
    assert_restart_advised(configure(&harness, ClientKind::ArchonCli, add("o/new")).await);
    assert_eq!(
        harness.kv("orchestrator_github").await.as_deref(),
        Some("1"),
        "the restart advised brings the policy"
    );
    assert_restart_advised(configure(&harness, ClientKind::ArchonCli, enable_repo).await);
    watcher.lock().unwrap().1["repositories"][0]["enabled"] = json!(true);
    watcher.lock().unwrap().1["enabled"] = json!(false);
    let enable_all = WatcherChange::Enable { repository: None };
    assert_restart_advised(configure(&harness, ClientKind::ArchonCli, enable_all.clone()).await);
    assert!(
        harness
            .fake
            .received_named("configure_github_watcher")
            .is_empty(),
        "nothing enabled"
    );
    assert!(
        harness.fake.received_named("update_agent").is_empty(),
        "a live orchestrator's instructions are never patched"
    );
    // `archon orchestrator restart` starts a generation with the policy.
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    let reply = configure(&harness, ClientKind::ArchonCli, enable_all).await;
    assert!(matches!(reply, Ok(Reply::GithubWatcher(_))), "{reply:?}");
    assert_eq!(targets(&watcher), [json!(ORCH_2)]);
    assert_eq!(watcher.lock().unwrap().1["enabled"], true);
}

#[tokio::test]
async fn no_edit_moves_delivery_to_an_orchestrator_without_the_policy() {
    let harness = Harness::new().await;
    harness.connect().await;
    // Aether restarted the orchestrator from one started by an older Archon.
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    let watcher = serve_watcher(
        &harness,
        json!([fixed("o/a", ORCH), fixed("o/b", ORCH), fixed("o/c", ORCH)]),
    );
    let unprepared = serve_orchestrators(&harness, &[ORCH_2]);
    assert_restart_advised(github::repoint_now(&harness.ctx).await);
    let edits = [
        WatcherChange::Configure {
            mention_regex: None,
            label_regex: None,
            poll_interval_secs: Some(300),
        },
        WatcherChange::Remove {
            repository: GithubRepo::parse("o/c").unwrap(),
        },
        WatcherChange::Disable {
            repository: repository("o/b"),
        },
    ];
    for edit in edits {
        let reply = configure(&harness, ClientKind::ArchonCli, edit.clone()).await;
        assert!(
            matches!(reply, Ok(Reply::GithubWatcher(_))),
            "{edit:?}: {reply:?}"
        );
        assert!(
            targets(&watcher).iter().all(|t| t == ORCH),
            "{edit:?} moved a route: {:?}",
            targets(&watcher)
        );
    }
    let state = watcher.lock().unwrap().1.clone();
    assert_eq!(state["poll_interval_secs"], 300);
    assert_eq!(state["repositories"].as_array().unwrap().len(), 2);
    assert_eq!(state["repositories"][1]["enabled"], false);
    // Once the orchestrator runs with the policy, the check repoints.
    unprepared.lock().unwrap().clear();
    assert!(github::repoint_now(&harness.ctx).await.unwrap());
    assert_eq!(targets(&watcher), [json!(ORCH_2), json!(ORCH_2)]);
}

#[tokio::test]
async fn a_route_that_delivers_nothing_follows_any_orchestrator() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    let watcher = serve_watcher(&harness, json!([fixed("o/a", ORCH)]));
    watcher.lock().unwrap().1["enabled"] = json!(false);
    serve_orchestrators(&harness, &[ORCH_2]);
    assert!(github::repoint_now(&harness.ctx).await.unwrap());
    assert_eq!(targets(&watcher), [json!(ORCH_2)]);
    let enable_all = WatcherChange::Enable { repository: None };
    assert_restart_advised(configure(&harness, ClientKind::ArchonCli, enable_all).await);
    assert_eq!(watcher.lock().unwrap().1["enabled"], false);
}

/// A refused `operators` edit says why and that no login was set.
fn assert_operators_not_set(result: Result<Reply, ArchonError>) {
    let error = result.unwrap_err();
    assert!(matches!(error, ArchonError::Unavailable(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("archon orchestrator restart")
            && text.contains("the operator logins were not set"),
        "{text}"
    );
}

/// Operator standing reaches an orchestrator only through instructions that
/// say what it allows, so the list waits for `archon orchestrator restart`.
#[tokio::test]
async fn operator_logins_are_not_set_for_an_orchestrator_without_the_policy() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let watcher = serve_watcher(&harness, json!([fixed("o/a", ORCH), thread_mode("o/t")]));
    let unprepared = serve_orchestrators(&harness, &[ORCH]);
    let before = watcher.lock().unwrap().clone();
    let set = || configure(&harness, ClientKind::ArchonCli, operators(&["octocat"]));
    assert_operators_not_set(set().await);
    // Aether restarted it from the old one: the route stays, and the refusal.
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    unprepared.lock().unwrap().push(ORCH_2);
    assert_operators_not_set(set().await);
    assert_eq!(*watcher.lock().unwrap(), before, "nothing was written");
    for command in ["configure_github_watcher", "update_agent", "create_agent"] {
        let sent = harness.fake.received_named(command);
        assert!(sent.is_empty(), "{command} was sent: {sent:?}");
    }
    // `archon orchestrator restart` starts a generation with the policy.
    unprepared.lock().unwrap().retain(|id| *id != ORCH_2);
    let reply = set().await;
    let Ok(Reply::GithubWatcher(view)) = reply else {
        panic!("setting replied {reply:?}");
    };
    assert_eq!(view.operator_users, Some(vec!["octocat".to_owned()]));
    let (revision, config) = watcher.lock().unwrap().clone();
    assert_eq!(revision, before.0 + 1, "one write");
    assert_eq!(config["operator_users"], json!(["octocat"]));
    assert_eq!(targets(&watcher), [json!(ORCH_2), Value::Null]);
}

/// Taking authority away never waits for the orchestrator: not for one
/// without the policy, not for one Aether no longer knows, not for none.
#[tokio::test]
async fn clearing_operator_logins_works_whatever_the_orchestrator_runs() {
    for (orchestrator, known) in [(Some(ORCH_2), true), (Some(ORCH_2), false), (None, true)] {
        let harness = Harness::new().await;
        harness.connect().await;
        if let Some(id) = orchestrator {
            harness.set_kv(keys::ORCHESTRATOR_AGENT, id).await;
        }
        let watcher = serve_watcher(&harness, json!([fixed("o/a", ORCH)]));
        watcher.lock().unwrap().1["operator_users"] = json!(["octocat"]);
        serve_orchestrators(&harness, &[ORCH, ORCH_2]);
        if !known {
            let gone = Arc::new(|_: &Value| Scripted::refuse("not_found", "no such agent"));
            harness.fake.set_responder("get_agent", gone);
        }
        let reply = configure(&harness, ClientKind::ArchonCli, NO_OPERATORS).await;
        let Ok(Reply::GithubWatcher(view)) = reply else {
            panic!("clearing with {orchestrator:?} (known: {known}) replied {reply:?}");
        };
        assert_eq!(view.operator_users, None);
        let (revision, config) = watcher.lock().unwrap().clone();
        assert_eq!(revision, 2, "one write");
        assert!(config.get("operator_users").is_none(), "{config}");
        assert_eq!(targets(&watcher), [json!(ORCH)], "no route moved");
    }
}

/// With no fixed target taking requests there is no orchestrator to wait
/// for, and switching delivery on afterwards meets the check `add` meets.
#[tokio::test]
async fn operator_logins_are_set_while_no_fixed_target_takes_requests() {
    let off = json!({"repository": "o/off", "enabled": false, "agent": ORCH,
                     "dispatch": {"mode": "fixed"}});
    let enable = |repository| WatcherChange::Enable { repository };
    let cases = [
        (true, json!([]), add("o/new")),
        (
            true,
            json!([off, thread_mode("o/t")]),
            enable(repository("o/off")),
        ),
        (false, json!([fixed("o/a", ORCH)]), enable(None)),
    ];
    for (enabled, repositories, delivery) in cases {
        let harness = Harness::new().await;
        harness.connect().await;
        harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
        let watcher = serve_watcher(&harness, repositories.clone());
        watcher.lock().unwrap().1["enabled"] = json!(enabled);
        serve_orchestrators(&harness, &[ORCH]);
        let reply = configure(&harness, ClientKind::ArchonCli, operators(&["octocat"])).await;
        assert!(
            matches!(reply, Ok(Reply::GithubWatcher(_))),
            "{repositories}: {reply:?}"
        );
        assert_restart_advised(configure(&harness, ClientKind::ArchonCli, delivery).await);
        let (revision, config) = watcher.lock().unwrap().clone();
        assert_eq!(revision, 2, "only the list was written: {repositories}");
        assert_eq!(config["operator_users"], json!(["octocat"]));
        assert_eq!(config["enabled"], enabled);
        assert_eq!(config["repositories"], repositories, "no route changed");
    }
}

#[tokio::test]
async fn nothing_but_the_operators_cli_configures_the_watcher() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    serve_watcher(&harness, json!([]));
    let allow = WatcherChange::Allow {
        repository: GithubRepo::parse("o/r").unwrap(),
        allowed_users: None,
    };
    let changes = [add("o/r"), allow, operators(&["octocat"]), NO_OPERATORS];
    for change in changes {
        let refused = configure(&harness, ClientKind::ArchonMcp, change.clone()).await;
        assert!(
            matches!(refused, Err(ArchonError::Forbidden(_))),
            "{change:?}: {refused:?}"
        );
    }
    for command in ["get_github_watcher", "configure_github_watcher"] {
        let sent = harness.fake.received_named(command);
        assert!(sent.is_empty(), "refused before asking Aether: {sent:?}");
    }
}

#[tokio::test]
async fn an_edit_made_meanwhile_fails_the_compare_and_swap() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    let watcher = serve_watcher(&harness, json!([fixed("o/r", ORCH)]));
    // Someone else's edit lands between archond's read and its write.
    let stale = json!({"revision": 1, "config": watcher.lock().unwrap().1.clone(),
                       "repositories": []});
    harness.fake.script(
        "get_github_watcher",
        Scripted::reply("github_watcher", stale),
    );
    watcher.lock().unwrap().0 = 2;
    let disable = WatcherChange::Disable { repository: None };
    let err = configure(&harness, ClientKind::ArchonCli, disable)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("refresh before editing"), "{err}");
    assert_eq!(
        watcher.lock().unwrap().1["enabled"],
        true,
        "nothing overwritten"
    );
}

#[tokio::test]
async fn archons_repositories_follow_the_orchestrator_and_others_stay() {
    let harness = Harness::new().await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH_2).await;
    let watcher = serve_watcher(&harness, json!([fixed("o/a", ORCH), thread_mode("o/t")]));
    let status = handlers::handle(
        &harness.ctx,
        ClientKind::ArchonMcp,
        Request::Status(Default::default()),
    )
    .await;
    let Ok(Reply::Status(status)) = status else {
        panic!("status replied {status:?}");
    };
    let Some(GithubWatch::Watching(view)) = &status.github else {
        panic!("no watcher in {:?}", status.github);
    };
    assert_eq!(view.stale().count(), 1, "status shows the old target");
    // The loop reads the stored orchestrator, so a restarted archond heals too.
    let ctx = harness.ctx.clone();
    let loop_task = tokio::spawn(github::watch_github(ctx));
    github::check_github_soon(&harness.ctx);
    harness
        .fake
        .wait_for("configure_github_watcher", 1, WAIT)
        .await;
    assert_eq!(targets(&watcher), [json!(ORCH_2), Value::Null]);
    assert!(
        !github::repoint_now(&harness.ctx).await.unwrap(),
        "idempotent"
    );
    loop_task.abort();
    assert_eq!(
        harness
            .fake
            .received_named("configure_github_watcher")
            .len(),
        1
    );
}

#[tokio::test]
async fn shared_mode_leaves_the_operators_aether_alone() {
    let mut config = Config::default();
    config.aether.mode = AetherMode::Shared;
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    harness.set_kv(keys::ORCHESTRATOR_AGENT, ORCH).await;
    serve_watcher(&harness, json!([]));
    let refused = configure(&harness, ClientKind::ArchonCli, add("o/r")).await;
    assert!(
        matches!(refused, Err(ArchonError::Invalid(_))),
        "{refused:?}"
    );
    let Ok(Reply::Status(status)) = harness_status(&harness).await else {
        panic!("status failed");
    };
    assert_eq!(status.github, None);
    let skipped = github::repoint_now(&harness.ctx).await;
    assert!(
        matches!(skipped, Err(ArchonError::Invalid(_))),
        "{skipped:?}"
    );
    assert!(harness.fake.received_named("get_github_watcher").is_empty());
    assert!(
        harness
            .fake
            .received_named("configure_github_watcher")
            .is_empty()
    );
}

async fn harness_status(harness: &Harness) -> Result<Reply, ArchonError> {
    handlers::handle(
        &harness.ctx,
        ClientKind::ArchonCli,
        Request::Status(Default::default()),
    )
    .await
}

/// A `gh` that records its arguments and stdin beside itself and answers
/// like `gh api` creating a comment.
fn fake_gh(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("gh");
    let body = format!(
        "#!/bin/sh\nprintf '%s ' \"$@\" > {dir}/args\ncat > {dir}/stdin\n\
         echo '{{\"html_url\": \"https://github.com/o/r/issues/5#issuecomment-9\"}}'\n",
        dir = dir.display()
    );
    archon_core::testing::write_script(&script, &body);
    script
}

/// The harness, and the directory the fake `gh` records into.
async fn reply_harness() -> (Harness, tempfile::TempDir) {
    let gh_dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.github.binary = fake_gh(gh_dir.path());
    let harness = Harness::with(config, None).await;
    harness.connect().await;
    serve_watcher(&harness, json!([fixed("o/r", ORCH), thread_mode("o/t")]));
    (harness, gh_dir)
}

fn thread(repository: &str, number: u64) -> GithubThread {
    serde_json::from_value(json!({"repository": repository, "number": number})).unwrap()
}

async fn reply(harness: &Harness, thread: GithubThread) -> Result<Reply, ArchonError> {
    let request = Request::GithubReply {
        thread,
        body: "Queued as qi_1; I will post the outcome here.".into(),
    };
    harness.set_kv(keys::ORCHESTRATOR_AGENT, "agt_0001").await;
    harness.mcp_as("agt_0001", request).await
}

#[tokio::test]
async fn a_reply_on_a_watched_thread_is_posted_marked_and_logged() {
    let (harness, gh_dir) = reply_harness().await;
    let posted = reply(&harness, thread("O/R", 5)).await;
    let Ok(Reply::GithubComment { url }) = posted else {
        panic!("github_reply replied {posted:?}");
    };
    assert_eq!(url, "https://github.com/o/r/issues/5#issuecomment-9");
    let args = std::fs::read_to_string(gh_dir.path().join("args")).unwrap();
    assert_eq!(
        args.trim(),
        "api --hostname github.com --method POST repos/o/r/issues/5/comments --input -"
    );
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(gh_dir.path().join("stdin")).unwrap())
            .unwrap();
    let body = stdin["body"].as_str().unwrap();
    assert!(body.starts_with("Queued as qi_1;") && body.ends_with(REPLY_MARKER));
    let logged = harness.notifications(NotificationKind::Progress).await;
    assert!(
        logged
            .iter()
            .any(|n| n.contains("o/r#5") && n.contains(&url))
    );
}

#[tokio::test]
async fn replies_outside_archons_watched_repositories_are_refused() {
    let (harness, gh_dir) = reply_harness().await;
    for elsewhere in [thread("someone/else", 1), thread("o/t", 2)] {
        let refused = reply(&harness, elsewhere.clone()).await;
        assert!(
            matches!(refused, Err(ArchonError::Forbidden(_))),
            "{elsewhere}: {refused:?}"
        );
    }
    assert!(!gh_dir.path().join("args").exists(), "gh never ran");
    assert!(
        harness
            .notifications(NotificationKind::Progress)
            .await
            .is_empty()
    );
}
