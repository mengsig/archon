//! The GitHub watcher's allowlist and operator logins on the wire: sent in
//! the configure where the protocol stores them (15 and 17), refused before
//! sending where it would drop them.

mod common;

use archon_aether::fake::Scripted;
use archon_aether::github::{WatcherConfig, WatcherStatus};
use archon_core::error::ArchonError;
use common::{connect, fake_aether};
use serde_json::{Value, json};

fn status(revision: u64, repository: Value) -> Value {
    json!({
        "revision": revision,
        "config": {
            "enabled": true,
            "mention_regex": "(?i)@archon",
            "label_regex": "(?i)archon",
            "poll_interval_secs": 120,
            "repositories": [repository],
        },
        "repositories": [],
    })
}

fn repository(allowed_users: Option<&[&str]>) -> Value {
    let mut repo = json!({"repository": "example-org/example-repo", "enabled": true,
                          "agent": common::AGENT, "dispatch": {"mode": "fixed"}});
    if let Some(users) = allowed_users {
        repo["allowed_users"] = json!(users);
    }
    repo
}

fn config(allowed_users: Option<&[&str]>) -> WatcherConfig {
    let status: WatcherStatus =
        serde_json::from_value(status(1, repository(allowed_users))).unwrap();
    status.config
}

const TEAM: &[&str] = &["monalisa", "hubot"];

#[tokio::test]
async fn an_allowlist_is_sent_in_the_configure_request() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "configure_github_watcher",
        Scripted::reply("github_watcher", status(2, repository(Some(TEAM)))),
    );
    let client = connect(&fake).await;
    let updated = client
        .configure_github_watcher(config(Some(TEAM)), 1)
        .await
        .unwrap();
    assert_eq!(
        updated.config.repositories[0].allowed_users.as_deref(),
        Some(&["monalisa".to_owned(), "hubot".to_owned()][..])
    );
    let sent = fake.received_named("configure_github_watcher");
    assert_eq!(sent.len(), 1);
    let repo = &sent[0]["config"]["repositories"][0];
    assert_eq!(repo["allowed_users"], json!(TEAM));
    assert_eq!(sent[0]["expected_revision"], 1);
}

const OPERATORS: &[&str] = &["octocat", "monalisa"];

fn with_operators(mut status: Value, operators: &[&str]) -> Value {
    status["config"]["operator_users"] = json!(operators);
    status
}

fn operator_config() -> WatcherConfig {
    let status = with_operators(status(1, repository(None)), OPERATORS);
    serde_json::from_value::<WatcherStatus>(status)
        .unwrap()
        .config
}

#[tokio::test]
async fn operator_logins_are_sent_and_read_at_protocol_17() {
    let (_dir, fake) = fake_aether().await;
    let saved = with_operators(status(2, repository(None)), OPERATORS);
    fake.script(
        "configure_github_watcher",
        Scripted::reply("github_watcher", saved.clone()),
    );
    fake.script(
        "get_github_watcher",
        Scripted::reply("github_watcher", saved),
    );
    fake.set_protocol_window(16, 17);
    let client = connect(&fake).await;
    assert_eq!(client.welcome().protocol_version, 17);
    let updated = client
        .configure_github_watcher(operator_config(), 1)
        .await
        .unwrap();
    let listed = Some(vec!["octocat".to_owned(), "monalisa".to_owned()]);
    assert_eq!(updated.config.operator_users, listed);
    let sent = fake.received_named("configure_github_watcher");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["config"]["operator_users"], json!(OPERATORS));
    assert_eq!(
        client.github_watcher().await.unwrap().config.operator_users,
        listed
    );
}

#[tokio::test]
async fn an_unset_list_is_absent_from_the_configure_request() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "configure_github_watcher",
        Scripted::reply("github_watcher", status(2, repository(None))),
    );
    let client = connect(&fake).await;
    let updated = client.configure_github_watcher(config(None), 1).await;
    assert_eq!(updated.unwrap().config.operator_users, None);
    let sent = fake.received_named("configure_github_watcher");
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0]["config"].get("operator_users").is_none(),
        "{sent:?}"
    );
}

/// Any other refusal of a watcher command keeps Aether's own code and text.
#[tokio::test]
async fn other_watcher_refusals_are_passed_on_unchanged() {
    let (_dir, fake) = fake_aether().await;
    fake.script(
        "get_github_watcher",
        Scripted::refuse("invalid_argument", "operator_users is empty"),
    );
    fake.script(
        "get_github_watcher",
        Scripted::refuse("incompatible", "GitHub watcher needs a newer client"),
    );
    let client = connect(&fake).await;
    let invalid = client.github_watcher().await.unwrap_err();
    assert!(matches!(invalid, ArchonError::Invalid(_)), "{invalid:?}");
    assert_eq!(
        invalid.to_string(),
        "invalid argument: aether: operator_users is empty"
    );
    let other = client.github_watcher().await.unwrap_err();
    assert_eq!(
        other.to_string(),
        "aether: incompatible: GitHub watcher needs a newer client"
    );
}
