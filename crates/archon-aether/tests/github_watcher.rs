//! The GitHub watcher's allowlist and operator logins on the wire: sent in
//! the configure where the protocol stores them (15 and 17), refused before
//! sending where it would drop them.

mod common;

use archon_aether::fake::{OPERATOR_USERS_WATCHER_VERSION_ERROR, Scripted};
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

/// Protocol 14 has no `allowed_users`: an old aetherd would drop the list
/// and let every writer dispatch, so Archon refuses before sending.
#[tokio::test]
async fn an_allowlist_is_refused_against_a_protocol_14_aetherd() {
    let (_dir, fake) = fake_aether().await;
    fake.set_protocol_window(13, 14);
    let client = connect(&fake).await;
    assert_eq!(client.welcome().protocol_version, 14);
    let error = client
        .configure_github_watcher(config(Some(TEAM)), 1)
        .await
        .unwrap_err();
    assert!(matches!(error, ArchonError::Aether(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("protocol 14") && text.contains("allowlist") && text.contains("upgrade"),
        "{text}"
    );
    assert!(fake.received_named("configure_github_watcher").is_empty());

    fake.script(
        "configure_github_watcher",
        Scripted::reply("github_watcher", status(2, repository(None))),
    );
    let open = client.configure_github_watcher(config(None), 1).await;
    assert_eq!(
        open.unwrap().revision,
        2,
        "a config without one still saves"
    );
    let sent = fake.received_named("configure_github_watcher");
    assert!(
        sent[0]["config"]["repositories"][0]
            .get("allowed_users")
            .is_none()
    );
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

/// Protocols 15 and 16 have no `operator_users`: the aetherd could not keep
/// the list, so Archon refuses before sending and says how to upgrade it.
#[tokio::test]
async fn operator_logins_are_refused_against_an_aetherd_below_protocol_17() {
    for (window, spoken) in [((14, 15), 15), ((15, 16), 16)] {
        let (_dir, fake) = fake_aether().await;
        fake.set_protocol_window(window.0, window.1);
        let client = connect(&fake).await;
        assert_eq!(client.welcome().protocol_version, spoken);
        let error = client
            .configure_github_watcher(operator_config(), 1)
            .await
            .unwrap_err();
        assert!(matches!(error, ArchonError::Aether(_)), "{error:?}");
        let text = error.to_string();
        assert!(
            text.contains(&format!("speaks Aether protocol {spoken}"))
                && text.contains("operator logins (they need 17)")
                && text.contains("upgrade Archon's aetherd")
                && text.contains("archon daemon stop --aether")
                && text.ends_with("Nothing was changed"),
            "{text}"
        );
        assert!(fake.received_named("configure_github_watcher").is_empty());

        fake.script(
            "configure_github_watcher",
            Scripted::reply("github_watcher", status(2, repository(Some(TEAM)))),
        );
        let narrowed = client.configure_github_watcher(config(Some(TEAM)), 1).await;
        assert_eq!(narrowed.unwrap().revision, 2, "an allowlist still saves");
    }
}

/// An aetherd holding a list answers a protocol-16 peer `incompatible` for
/// every watcher command; Archon names the cause and writes nothing.
#[tokio::test]
async fn a_watcher_hiding_operator_logins_is_a_clear_error_not_a_decode_failure() {
    let (_dir, fake) = fake_aether().await;
    fake.set_protocol_window(15, 16);
    let held = with_operators(status(3, repository(None)), OPERATORS);
    fake.set_default(
        "get_github_watcher",
        Scripted::reply("github_watcher", held),
    );
    fake.set_default(
        "configure_github_watcher",
        Scripted::refuse("incompatible", OPERATOR_USERS_WATCHER_VERSION_ERROR),
    );
    let client = connect(&fake).await;
    assert_eq!(client.welcome().protocol_version, 16);
    let saving = client.configure_github_watcher(config(None), 3).await;
    for error in [
        client.github_watcher().await.unwrap_err(),
        saving.unwrap_err(),
    ] {
        assert!(matches!(error, ArchonError::Aether(_)), "{error:?}");
        let text = error.to_string();
        assert!(
            text.contains("has GitHub operator logins set")
                && text.contains("Aether protocol 16 cannot carry")
                && text.contains("operator-users --clear")
                && text.contains(OPERATOR_USERS_WATCHER_VERSION_ERROR),
            "{text}"
        );
        assert!(!text.contains("did not decode"), "{text}");
        assert!(!text.contains("upgrade Archon"), "{text}");
    }
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
