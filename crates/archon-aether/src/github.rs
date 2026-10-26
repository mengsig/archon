//! Aether's GitHub watcher config and status (`get_github_watcher`,
//! `configure_github_watcher`). Configuring replaces the whole config, so
//! everything Archon does not model (a `thread` dispatch template, a newer
//! Aether's fields) round-trips untouched rather than being dropped.

use archon_core::Timestamp;
use archon_core::aether_ids::AetherAgentId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Aether's `GithubWatcherConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatcherConfig {
    pub enabled: bool,
    pub mention_regex: String,
    pub label_regex: String,
    pub poll_interval_secs: u64,
    pub repositories: Vec<RepositoryConfig>,
    /// The logins whose own requests carry the operator's authority, stored
    /// lowercase. `None`: no GitHub login is an operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_users: Option<Vec<String>>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// Aether's `GithubRepositoryConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub repository: String,
    pub enabled: bool,
    #[serde(default)]
    pub agent: Option<AetherAgentId>,
    /// `{"mode": "fixed" | "automatic" | "thread", ...}`.
    #[serde(default = "fixed_dispatch")]
    pub dispatch: Value,
    /// The logins that alone may dispatch, stored lowercase.
    /// `None`: any collaborator with write access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_users: Option<Vec<String>>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl RepositoryConfig {
    /// Enabled, delivering to `agent` alone.
    pub fn fixed(repository: String, agent: AetherAgentId) -> Self {
        Self {
            repository,
            enabled: true,
            agent: Some(agent),
            dispatch: fixed_dispatch(),
            allowed_users: None,
            rest: Map::new(),
        }
    }

    /// The dispatch mode's name; `"unknown"` for a shape this client cannot read.
    pub fn mode(&self) -> &str {
        self.dispatch
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    }

    pub fn is_fixed(&self) -> bool {
        self.mode() == "fixed"
    }
}

fn fixed_dispatch() -> Value {
    serde_json::json!({"mode": "fixed"})
}

/// Aether's `GithubWatcherStatus`, the `github_watcher` reply.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WatcherStatus {
    /// Bumped by every configure; a configure names the one it edits.
    pub revision: u64,
    pub config: WatcherConfig,
    #[serde(default)]
    pub repositories: Vec<RepositoryHealth>,
}

/// Aether's `GithubRepositoryStatus`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RepositoryHealth {
    pub repository: String,
    #[serde(default)]
    pub last_poll_at: Option<Timestamp>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub dispatched: u64,
    #[serde(default)]
    pub ignored: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn aether_status() -> Value {
        json!({
            "revision": 4,
            "config": {
                "enabled": true,
                "mention_regex": "(?i)@archon",
                "label_regex": "(?i)archon",
                "poll_interval_secs": 120,
                "repositories": [
                    {"repository": "o/fixed", "enabled": true,
                     "agent": "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV", "dispatch": {"mode": "fixed"}},
                    {"repository": "o/thread", "enabled": false, "agent": null,
                     "dispatch": {"mode": "thread", "role": "r", "objective": "o"}},
                ],
            },
            "repositories": [{"repository": "o/fixed", "dispatched": 2, "ignored": 1,
                              "last_poll_at": "2026-09-28T10:00:00Z", "next_poll_at": null}],
        })
    }

    #[test]
    fn a_status_decodes_and_its_config_round_trips_untouched() {
        let status: WatcherStatus = serde_json::from_value(aether_status()).unwrap();
        assert_eq!(status.revision, 4);
        let [fixed, thread] = status.config.repositories.as_slice() else {
            panic!("two repositories");
        };
        assert!(fixed.is_fixed() && !thread.is_fixed());
        assert_eq!(thread.mode(), "thread");
        assert_eq!(status.repositories[0].dispatched, 2);
        let written = serde_json::to_value(&status.config).unwrap();
        assert_eq!(written, aether_status()["config"]);
    }

    #[test]
    fn fields_a_newer_aether_adds_are_kept() {
        let mut config = aether_status()["config"].clone();
        config["quiet_hours"] = json!([22, 7]);
        config["repositories"][0]["labels"] = json!(["x"]);
        let decoded: WatcherConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), config);
    }

    #[test]
    fn an_allowlist_round_trips_and_an_absent_one_stays_absent() {
        let mut config = aether_status()["config"].clone();
        let open: WatcherConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!(open.repositories[0].allowed_users, None);
        let written = serde_json::to_value(&open).unwrap();
        assert!(written["repositories"][0].get("allowed_users").is_none());

        config["repositories"][0]["allowed_users"] = json!(["monalisa", "hubot"]);
        let narrowed: WatcherConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!(
            narrowed.repositories[0].allowed_users.as_deref(),
            Some(&["monalisa".to_owned(), "hubot".to_owned()][..])
        );
        assert!(narrowed.repositories[0].rest.is_empty(), "a typed field");
        assert_eq!(serde_json::to_value(&narrowed).unwrap(), config);
    }

    #[test]
    fn operator_users_round_trip_and_an_absent_list_stays_absent() {
        let mut config = aether_status()["config"].clone();
        let unset: WatcherConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!(unset.operator_users, None);
        let written = serde_json::to_value(&unset).unwrap();
        assert!(written.get("operator_users").is_none(), "{written}");

        config["operator_users"] = json!(["octocat", "monalisa"]);
        let set: WatcherConfig = serde_json::from_value(config.clone()).unwrap();
        assert_eq!(
            set.operator_users.as_deref(),
            Some(&["octocat".to_owned(), "monalisa".to_owned()][..])
        );
        assert!(set.rest.is_empty(), "a typed field, not an unknown one");
        assert!(
            set.repositories.iter().all(|r| r.allowed_users.is_none()),
            "no repository was narrowed"
        );
        assert_eq!(serde_json::to_value(&set).unwrap(), config);
    }

    #[test]
    fn a_repository_without_dispatch_is_fixed_like_aether_reads_it() {
        let repo: RepositoryConfig =
            serde_json::from_value(json!({"repository": "o/r", "enabled": true})).unwrap();
        assert!(repo.is_fixed());
        let unreadable: RepositoryConfig = serde_json::from_value(
            json!({"repository": "o/r", "enabled": true, "dispatch": "fixed"}),
        )
        .unwrap();
        assert_eq!(unreadable.mode(), "unknown");
    }
}
