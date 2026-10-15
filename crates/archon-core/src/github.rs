//! The GitHub watcher's shared vocabulary (docs/github-watcher.md): the
//! thread a request came from, the operator's edits to Archon's watcher,
//! and archond's view of Aether's watcher with its targets.

use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroU64;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::aether_ids::AetherAgentId;
use crate::error::ArchonError;

/// Aether's watcher skips comments carrying it, so replies never re-trigger.
pub const REPLY_MARKER: &str = "<!-- aether:github-reply -->";

/// All the time archond gives one `github_reply`, lookup to `gh`'s answer;
/// under the MCP server's per-call budget, so its answer still arrives.
pub const REPLY_BUDGET: Duration = Duration::from_secs(10);

/// Matches `@archon` as a whole handle: not `@archon-bot`, not `me@archon`.
pub const MENTION_REGEX: &str = "(?i)(?:^|[^a-z0-9_@])@archon(?:$|[^a-z0-9_-])";

pub const LABEL_REGEX: &str = "(?i)archon";

/// A GitHub.com `owner/name`, lowercased as Aether stores it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GithubRepo(String);

impl GithubRepo {
    /// Accepts what Aether's watcher accepts: two segments of ASCII
    /// alphanumerics, `-`, `_` or `.`, neither `.` nor `..`, at most 100 bytes.
    pub fn parse(text: &str) -> Result<Self, ArchonError> {
        let valid_segment = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.len() <= 100
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        let text = text.trim();
        match text.split_once('/') {
            Some((owner, name)) if valid_segment(owner) && valid_segment(name) => {
                Ok(Self(text.to_ascii_lowercase()))
            }
            _ => Err(ArchonError::Invalid(format!(
                "{text:?} is not a GitHub repository; expected owner/name"
            ))),
        }
    }

    /// The github.com repository a git remote URL names: `https://`,
    /// `ssh://git@` or scp-like `git@github.com:owner/name`, with or without
    /// `.git`. `None` for any other host or shape.
    pub fn from_remote_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let path = [
            "https://github.com/",
            "ssh://git@github.com/",
            "git@github.com:",
        ]
        .into_iter()
        .find_map(|prefix| url.strip_prefix(prefix))?;
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if path.matches('/').count() != 1 {
            return None;
        }
        Self::parse(path).ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GithubRepo {
    type Error = ArchonError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<GithubRepo> for String {
    fn from(repo: GithubRepo) -> Self {
        repo.0
    }
}

impl fmt::Display for GithubRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Most distinct logins one list may hold, a repository's allowlist or the
/// watcher's operator logins (Aether's `MAX_ALLOWED_USERS`).
pub const MAX_ALLOWED_USERS: usize = 100;

/// GitHub's login rule, as Aether checks it: 1-39 ASCII alphanumerics or
/// single inner hyphens.
pub fn is_github_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The GitHub logins that alone may dispatch for one repository: non-empty,
/// at most [`MAX_ALLOWED_USERS`], lowercased and de-duplicated in first-seen
/// order, as Aether stores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct AllowedUsers(Vec<String>);

impl AllowedUsers {
    /// Accepts what Aether accepts, with its rules' wording on refusal.
    pub fn parse<S: AsRef<str>>(logins: &[S]) -> Result<Self, ArchonError> {
        let cleared = "clear it to allow every collaborator with write access";
        canonical_logins(logins, "the allowlist", cleared).map(Self)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// The stored form of the logins of `list`, by Aether's rules. `cleared`
/// says what removing the list does, for the refusal of an empty one.
fn canonical_logins<S: AsRef<str>>(
    logins: &[S],
    list: &str,
    cleared: &str,
) -> Result<Vec<String>, ArchonError> {
    if logins.is_empty() {
        return Err(ArchonError::Invalid(format!(
            "{list} is empty; {cleared}, or list at least one login"
        )));
    }
    let mut users: Vec<String> = Vec::with_capacity(logins.len());
    let mut seen = HashSet::with_capacity(logins.len());
    for login in logins.iter().map(|login| login.as_ref().trim()) {
        if !is_github_login(login) {
            return Err(ArchonError::Invalid(format!(
                "invalid GitHub login {login:?}; expected 1-39 letters, digits or single \
                 hyphens, not starting or ending with a hyphen"
            )));
        }
        let login = login.to_ascii_lowercase();
        if seen.insert(login.clone()) {
            users.push(login);
        }
    }
    if users.len() > MAX_ALLOWED_USERS {
        return Err(ArchonError::Invalid(format!(
            "{list} names {} distinct logins; at most {MAX_ALLOWED_USERS} are allowed",
            users.len()
        )));
    }
    debug_assert!(!users.is_empty(), "de-duplication keeps at least one login");
    Ok(users)
}

impl TryFrom<Vec<String>> for AllowedUsers {
    type Error = ArchonError;

    fn try_from(logins: Vec<String>) -> Result<Self, Self::Error> {
        Self::parse(&logins)
    }
}

impl From<AllowedUsers> for Vec<String> {
    fn from(users: AllowedUsers) -> Self {
        users.0
    }
}

/// The GitHub logins whose own requests carry the operator's authority.
/// A type of its own, so that no allowlist is ever saved as operator logins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct OperatorUsers(Vec<String>);

impl OperatorUsers {
    /// The allowlist's rules; Aether applies the same to `operator_users`.
    pub fn parse<S: AsRef<str>>(logins: &[S]) -> Result<Self, ArchonError> {
        let cleared = "clear it so that no GitHub login has the operator's authority";
        canonical_logins(logins, "the operator list", cleared).map(Self)
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl TryFrom<Vec<String>> for OperatorUsers {
    type Error = ArchonError;

    fn try_from(logins: Vec<String>) -> Result<Self, Self::Error> {
        Self::parse(&logins)
    }
}

impl From<OperatorUsers> for Vec<String> {
    fn from(users: OperatorUsers) -> Self {
        users.0
    }
}

/// One issue or pull request conversation: where a request came from and
/// where its replies go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubThread {
    pub repository: GithubRepo,
    pub number: NonZeroU64,
}

impl GithubThread {
    /// GitHub redirects `/issues/N` to `/pull/N` for a pull request.
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/issues/{}",
            self.repository, self.number
        )
    }

    /// What to do when a reply's outcome is unknown: a blind retry could
    /// post the same comment twice.
    pub fn check_before_retry(&self) -> String {
        format!(
            "the comment may have been posted: look for it on {} before replying again, \
             and never repeat the reply blindly",
            self.url()
        )
    }
}

impl fmt::Display for GithubThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repository, self.number)
    }
}

/// An operator's edit to Archon's watcher. Nothing here names a target:
/// archond always points Archon's repositories at the live orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WatcherChange {
    /// Watch a repository (and switch the watcher on). An allowlist replaces
    /// the repository's current one in the same write; `None` keeps it.
    Add {
        repository: GithubRepo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_users: Option<AllowedUsers>,
    },
    /// Replace a watched repository's allowlist, or clear it with `None` so
    /// every collaborator with write access may dispatch again.
    Allow {
        repository: GithubRepo,
        allowed_users: Option<AllowedUsers>,
    },
    /// Replace the watcher's operator logins, or clear them with `None` so
    /// that every GitHub request is an external request again.
    Operators {
        operator_users: Option<OperatorUsers>,
    },
    Remove {
        repository: GithubRepo,
    },
    /// One repository, or the whole watcher when `None`.
    Enable {
        repository: Option<GithubRepo>,
    },
    Disable {
        repository: Option<GithubRepo>,
    },
    /// `None` keeps the current setting.
    Configure {
        mention_regex: Option<String>,
        label_regex: Option<String>,
        poll_interval_secs: Option<u64>,
    },
}

/// Archon's view of its aetherd's GitHub watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubWatcherView {
    /// Aether's config revision; every edit is a compare-and-swap on it.
    pub revision: u64,
    pub enabled: bool,
    pub mention_regex: String,
    pub label_regex: String,
    pub poll_interval_secs: u64,
    /// The orchestrator Archon's repositories should target.
    pub orchestrator: Option<AetherAgentId>,
    /// The logins whose own requests Aether delivers with the operator's
    /// authority; `None`: no GitHub login has it.
    #[serde(default)]
    pub operator_users: Option<Vec<String>>,
    pub repositories: Vec<WatchedRepo>,
}

impl GithubWatcherView {
    /// Archon's repositories whose target is not the current orchestrator.
    pub fn stale(&self) -> impl Iterator<Item = &WatchedRepo> {
        self.repositories
            .iter()
            .filter(|r| matches!(r.target, WatchTarget::Stale { .. }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedRepo {
    pub repository: String,
    pub enabled: bool,
    pub target: WatchTarget,
    /// The logins that alone may dispatch; `None`: any collaborator with
    /// write access. Aether still checks a listed login's access.
    pub allowed_users: Option<Vec<String>>,
    /// Operator logins this repository's allowlist leaves out: Aether
    /// delivers none of their requests here, with or without authority.
    #[serde(default)]
    pub excluded_operators: Vec<String>,
    pub last_poll_at: Option<Timestamp>,
    pub last_error: Option<String>,
    /// Requests delivered and ignored since Aether started watching.
    pub dispatched: u64,
    pub ignored: u64,
}

impl WatchedRepo {
    /// Fixed dispatch: Archon's to repoint and to reply on.
    pub fn is_archons(&self) -> bool {
        !matches!(self.target, WatchTarget::Unmanaged { .. })
    }
}

/// Where a watched repository's requests go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchTarget {
    /// Archon's: fixed dispatch to the current orchestrator.
    Orchestrator,
    /// Archon's, but aimed elsewhere (an old orchestrator); archond repoints it.
    Stale { agent: Option<AetherAgentId> },
    /// Another dispatch mode, set through `archon aether github-watcher`;
    /// archond leaves it alone and `github_reply` refuses its threads.
    Unmanaged { mode: String },
}

/// The watcher in `archon status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GithubWatch {
    Watching(Box<GithubWatcherView>),
    /// archond could not ask Aether.
    Unreachable {
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_parse_like_aether_and_lowercase() {
        let repo = GithubRepo::parse(" Example-Org/Example.rs ").unwrap();
        assert_eq!(repo.as_str(), "example-org/example.rs");
        for bad in [
            "",
            "archon",
            "a/b/c",
            "/b",
            "a/",
            "./b",
            "a/..",
            "a b/c",
            "https://github.com/a/b",
        ] {
            assert!(GithubRepo::parse(bad).is_err(), "{bad:?} parsed");
        }
        assert!(GithubRepo::parse(&format!("{}/b", "a".repeat(101))).is_err());
        let decoded: Result<GithubRepo, _> = serde_json::from_str("\"not a repo\"");
        assert!(decoded.is_err());
    }

    #[test]
    fn only_a_github_com_remote_names_a_repository() {
        for url in [
            "https://github.com/Mengsig/Archon.git",
            "https://github.com/mengsig/archon/",
            "git@github.com:mengsig/archon.git",
            "ssh://git@github.com/mengsig/archon",
        ] {
            let repo = GithubRepo::from_remote_url(url);
            assert_eq!(
                repo.map(String::from).as_deref(),
                Some("mengsig/archon"),
                "{url}"
            );
        }
        for url in [
            "",
            "/srv/git/archon",
            "https://gitlab.com/mengsig/archon.git",
            "https://github.com.evil.example/mengsig/archon",
            "https://github.com/mengsig",
            "https://github.com/mengsig/archon/tree/main",
            "git@github.com:mengsig/../archon",
        ] {
            assert_eq!(GithubRepo::from_remote_url(url), None, "{url}");
        }
    }

    #[test]
    fn a_thread_names_its_url_and_refuses_number_zero() {
        let thread: GithubThread =
            serde_json::from_str(r#"{"repository":"Owner/Repo","number":42}"#).unwrap();
        assert_eq!(thread.to_string(), "owner/repo#42");
        assert_eq!(thread.url(), "https://github.com/owner/repo/issues/42");
        let zero: Result<GithubThread, _> =
            serde_json::from_str(r#"{"repository":"o/r","number":0}"#);
        assert!(zero.is_err());
    }

    #[test]
    fn logins_follow_githubs_rule() {
        let longest = "a".repeat(39);
        for valid in ["a", "monaLisa", "mona-lisa-octocat", "0x1", &longest] {
            assert!(is_github_login(valid), "{valid:?} refused");
        }
        let too_long = "a".repeat(40);
        for invalid in [
            "",
            "-lead",
            "trail-",
            "double--hyphen",
            "under_score",
            "bot[bot]",
            "d\u{e9}v",
            "a b",
            &too_long,
        ] {
            assert!(!is_github_login(invalid), "{invalid:?} accepted");
        }
    }

    #[test]
    fn an_allowlist_is_lowercased_deduplicated_and_bounded() {
        let users = AllowedUsers::parse(&["monaLisa", " HuBot", "MONALISA"]).unwrap();
        assert_eq!(users.as_slice(), ["monalisa", "hubot"]);
        let empty = AllowedUsers::parse::<&str>(&[]).unwrap_err();
        assert!(empty.to_string().contains("allowlist is empty"), "{empty}");
        let bad = AllowedUsers::parse(&["ok", "bad_login"]).unwrap_err();
        assert!(bad.to_string().contains("\"bad_login\""), "{bad}");

        let most: Vec<String> = (0..MAX_ALLOWED_USERS).map(|n| format!("u{n}")).collect();
        let mut repeated = most.clone();
        repeated.push("U0".into());
        assert_eq!(
            AllowedUsers::parse(&repeated).unwrap().as_slice().len(),
            MAX_ALLOWED_USERS
        );
        let mut over = most;
        over.push("one-more".into());
        let error = AllowedUsers::parse(&over).unwrap_err();
        assert!(error.to_string().contains("101 distinct logins"), "{error}");
    }

    #[test]
    fn operator_logins_follow_the_allowlists_rules_under_their_own_name() {
        let users = OperatorUsers::parse(&["Octocat", " monaLisa", "OCTOCAT"]).unwrap();
        assert_eq!(users.as_slice(), ["octocat", "monalisa"]);
        let empty = OperatorUsers::parse::<&str>(&[]).unwrap_err();
        assert!(
            empty.to_string().contains("the operator list is empty"),
            "{empty}"
        );
        let bad = OperatorUsers::parse(&["octocat", "bot[bot]"]).unwrap_err();
        assert!(bad.to_string().contains("\"bot[bot]\""), "{bad}");
        let over: Vec<String> = (0..=MAX_ALLOWED_USERS).map(|n| format!("u{n}")).collect();
        let error = OperatorUsers::parse(&over).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the operator list names 101 distinct logins"),
            "{error}"
        );
        assert!(OperatorUsers::parse(&over[1..]).is_ok(), "100 fit");
    }

    #[test]
    fn operator_logins_are_validated_when_decoded() {
        let decoded: OperatorUsers = serde_json::from_str(r#"["OctoCat"]"#).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), r#"["octocat"]"#);
        assert!(serde_json::from_str::<OperatorUsers>("[]").is_err());
        assert!(serde_json::from_str::<OperatorUsers>(r#"["a--b"]"#).is_err());
        let set = WatcherChange::Operators {
            operator_users: Some(decoded),
        };
        let text = serde_json::to_string(&set).unwrap();
        assert_eq!(
            text,
            r#"{"action":"operators","operator_users":["octocat"]}"#
        );
        assert_eq!(serde_json::from_str::<WatcherChange>(&text).unwrap(), set);
        let clear = r#"{"action":"operators","operator_users":null}"#;
        assert_eq!(
            serde_json::from_str::<WatcherChange>(clear).unwrap(),
            WatcherChange::Operators {
                operator_users: None
            }
        );
        let smuggled = r#"{"action":"operators","operator_users":["ok","not a login"]}"#;
        assert!(serde_json::from_str::<WatcherChange>(smuggled).is_err());
    }

    #[test]
    fn a_view_from_an_older_archond_names_no_operator_logins() {
        let older = serde_json::json!({
            "revision": 2, "enabled": true, "mention_regex": "m", "label_regex": "l",
            "poll_interval_secs": 120, "orchestrator": null,
            "repositories": [{
                "repository": "o/r", "enabled": true, "target": {"kind": "orchestrator"},
                "allowed_users": null, "last_poll_at": null, "last_error": null,
                "dispatched": 0, "ignored": 0,
            }],
        });
        let view: GithubWatcherView = serde_json::from_value(older).unwrap();
        assert_eq!(view.operator_users, None);
        assert!(view.repositories[0].excluded_operators.is_empty());
    }

    #[test]
    fn an_allowlist_is_validated_when_decoded() {
        let decoded: AllowedUsers = serde_json::from_str(r#"["MonaLisa"]"#).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), r#"["monalisa"]"#);
        assert!(serde_json::from_str::<AllowedUsers>("[]").is_err());
        assert!(serde_json::from_str::<AllowedUsers>(r#"["-x"]"#).is_err());
    }

    #[test]
    fn an_add_without_an_allowlist_keeps_its_old_wire_form() {
        let add = WatcherChange::Add {
            repository: GithubRepo::parse("o/r").unwrap(),
            allowed_users: None,
        };
        let text = serde_json::to_string(&add).unwrap();
        assert_eq!(text, r#"{"action":"add","repository":"o/r"}"#);
        assert_eq!(serde_json::from_str::<WatcherChange>(&text).unwrap(), add);
        let clear = WatcherChange::Allow {
            repository: GithubRepo::parse("o/r").unwrap(),
            allowed_users: None,
        };
        let text = serde_json::to_string(&clear).unwrap();
        assert_eq!(
            text,
            r#"{"action":"allow","repository":"o/r","allowed_users":null}"#
        );
        assert_eq!(serde_json::from_str::<WatcherChange>(&text).unwrap(), clear);
    }

    #[test]
    fn a_watcher_change_round_trips() {
        let change = WatcherChange::Enable { repository: None };
        let text = serde_json::to_string(&change).unwrap();
        assert_eq!(text, r#"{"action":"enable","repository":null}"#);
        assert_eq!(
            serde_json::from_str::<WatcherChange>(&text).unwrap(),
            change
        );
    }
}
