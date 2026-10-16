//! Push notifications (`[notify]`): which events reach the operator's phone
//! or another service, and through what.

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

/// `[notify] min_gap_secs` above this (a day) would silence a kind for good.
pub const MAX_MIN_GAP_SECS: u64 = 24 * 60 * 60;

/// What a push notification is about; the `[notify] on` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    /// An operator gate waits for a decision.
    Gate,
    /// A question waits for an answer.
    Question,
    /// An item failed.
    Failure,
    /// An item is blocked or ended needing the operator.
    Blocked,
    /// A delivered pull request's checks failed or never started.
    PrChecks,
    /// A budget is reached and holds new work.
    Budget,
}

impl PushKind {
    pub const ALL: [Self; 6] = [
        Self::Gate,
        Self::Question,
        Self::Failure,
        Self::Blocked,
        Self::PrChecks,
        Self::Budget,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Question => "question",
            Self::Failure => "failure",
            Self::Blocked => "blocked",
            Self::PrChecks => "pr_checks",
            Self::Budget => "budget",
        }
    }
}

/// `[notify]`: nothing is sent until `command` or `webhook` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    /// The kinds that are sent; every kind by default.
    pub on: Vec<PushKind>,
    /// A program and its arguments, run without a shell, the event as JSON
    /// on stdin (e.g. `["ntfy", "publish", "--title", "archon", "mytopic"]`).
    pub command: Vec<String>,
    /// An `http(s)://` URL the event JSON is POSTed to (through `curl`).
    pub webhook: Option<String>,
    /// At most one notification of a kind per this many seconds.
    pub min_gap_secs: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            on: PushKind::ALL.to_vec(),
            command: Vec::new(),
            webhook: None,
            min_gap_secs: 60,
        }
    }
}

impl NotifyConfig {
    /// Whether an event of `kind` is sent anywhere.
    pub fn sends(&self, kind: PushKind) -> bool {
        let has_sink = !self.command.is_empty() || self.webhook.is_some();
        has_sink && self.on.contains(&kind)
    }

    pub fn validate(&self) -> Result<(), ArchonError> {
        let refuse = |why: &str| Err(ArchonError::Config(format!("notify.{why}")));
        if !self.command.is_empty() && self.webhook.is_some() {
            return refuse("command and notify.webhook are exclusive; set one");
        }
        if self.command.first().is_some_and(|p| p.trim().is_empty()) {
            return refuse("command must start with a program");
        }
        if self.command.iter().any(|arg| arg.contains('\0')) {
            return refuse("command arguments must not contain NUL");
        }
        if let Some(url) = &self.webhook {
            let rest = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"));
            if rest.is_none_or(str::is_empty) {
                return refuse("webhook must be an http:// or https:// URL");
            }
            if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return refuse("webhook must not contain whitespace or control characters");
            }
        }
        if self.min_gap_secs > MAX_MIN_GAP_SECS {
            return refuse(&format!("min_gap_secs must be at most {MAX_MIN_GAP_SECS}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<NotifyConfig, ArchonError> {
        let config: NotifyConfig =
            toml::from_str(text).map_err(|e| ArchonError::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn push_kinds_name_themselves_as_serde_does() {
        for kind in PushKind::ALL {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, kind.as_str(), "{kind:?}");
        }
    }

    #[test]
    fn nothing_is_sent_without_a_command_or_webhook() {
        let config = NotifyConfig::default();
        config.validate().unwrap();
        assert!(PushKind::ALL.iter().all(|kind| !config.sends(*kind)));
        let config = parse(r#"command = ["ntfy", "publish", "t"]"#).unwrap();
        assert!(PushKind::ALL.iter().all(|kind| config.sends(*kind)));
    }

    #[test]
    fn only_listed_kinds_are_sent() {
        let config = parse(
            r#"
            on = ["gate", "pr_checks"]
            webhook = "https://ntfy.example/topic"
            "#,
        )
        .unwrap();
        assert!(config.sends(PushKind::Gate));
        assert!(config.sends(PushKind::PrChecks));
        assert!(!config.sends(PushKind::Question));
        assert!(!config.sends(PushKind::Budget));
        assert!(
            parse(r#"on = ["gates"]"#).is_err(),
            "unknown kinds are refused"
        );
    }

    #[test]
    fn malformed_sinks_are_refused() {
        for bad in [
            "command = [\"ntfy\"]\nwebhook = \"https://x\"",
            r#"command = [" "]"#,
            r#"webhook = "ftp://x""#,
            r#"webhook = "https://""#,
            r#"webhook = "https://x/a b""#,
            "min_gap_secs = 86401",
            "commands = [\"x\"]",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
