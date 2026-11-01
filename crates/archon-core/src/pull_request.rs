//! A pull request's identity: its host, repository and number. Unlike its
//! branch, which a later pull request may carry, it never names another one.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::id::QueueItemId;

/// Longest pull request number accepted, in digits; `u64` holds 19.
const MAX_NUMBER_DIGITS: usize = 18;

/// One pull request, as `https://<host>/<owner>/<name>/pull/<number>` names
/// it. Host and repository are lowercase, as GitHub matches them. Stored as
/// that URL, read back only when it is one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PullRequestRef {
    /// `github.com`, or a GitHub Enterprise host.
    pub host: String,
    /// `owner/name`.
    pub repository: String,
    pub number: u64,
}

impl PullRequestRef {
    /// The pull request `url` names; `None` for any URL of another shape.
    pub fn from_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("https://")?;
        let (host, path) = rest.split_once('/')?;
        let (repository, number) = path.rsplit_once("/pull/")?;
        let (owner, name) = repository.split_once('/')?;
        let digits = !number.is_empty()
            && number.len() <= MAX_NUMBER_DIGITS
            && !number.starts_with('0')
            && number.bytes().all(|b| b.is_ascii_digit());
        if !is_host(host) || !is_segment(owner) || !is_segment(name) || !digits {
            return None;
        }
        let pr = Self {
            host: host.to_ascii_lowercase(),
            repository: repository.to_ascii_lowercase(),
            number: number.parse().ok()?,
        };
        assert!(pr.number > 0, "no leading zero, so not zero");
        assert_eq!(pr.url().len(), url.len(), "the URL is rebuilt as given");
        Some(pr)
    }

    /// The pull request's web URL.
    pub fn url(&self) -> String {
        format!(
            "https://{}/{}/pull/{}",
            self.host, self.repository, self.number
        )
    }
}

impl TryFrom<String> for PullRequestRef {
    type Error = String;

    fn try_from(url: String) -> Result<Self, Self::Error> {
        Self::from_url(&url).ok_or_else(|| format!("{url:?} is no pull request URL"))
    }
}

impl From<PullRequestRef> for String {
    fn from(pr: PullRequestRef) -> Self {
        pr.url()
    }
}

/// The pull request an item delivers into instead of opening its own, as
/// enqueue's `onto` names it: another item's, a number, or an open branch's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OntoTarget {
    Item(QueueItemId),
    Pr(u64),
    Branch(String),
}

impl fmt::Display for OntoTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Item(id) => write!(f, "item {id}"),
            Self::Pr(number) => write!(f, "pull request #{number}"),
            Self::Branch(branch) => write!(f, "branch `{branch}`"),
        }
    }
}

/// An `onto` target's pull request as GitHub showed it at enqueue, frozen:
/// later work starts from `head_sha` and expects the head still there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OntoPullRequest {
    /// What enqueue was asked for.
    pub target: OntoTarget,
    pub pr: PullRequestRef,
    /// GitHub's node id, when `gh` gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// `owner/name` of the repository holding `head_ref`.
    pub head_repository: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    /// The item's pipeline pushes nothing: it reviews or reads the pull
    /// request, whose head may be a fork. A review that passed `head_sha`
    /// lets `request_merge` merge the pull request at that commit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review_only: bool,
}

impl OntoPullRequest {
    /// Whether the head lives in another repository than the pull request:
    /// a fork, which Archon never pushes to.
    pub fn is_fork(&self) -> bool {
        !self
            .head_repository
            .eq_ignore_ascii_case(&self.pr.repository)
    }
}

impl fmt::Display for OntoPullRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let short = self.head_sha.get(..12).unwrap_or(&self.head_sha);
        write!(
            f,
            "{} (`{}` into `{}`, head {short})",
            self.pr.url(),
            self.head_ref,
            self.base_ref
        )
    }
}

/// `<host>/<owner>/<name>#<number>`, as the store keys a pull request.
impl fmt::Display for PullRequestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.host, self.repository, self.number)
    }
}

fn is_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:".contains(&b))
}

/// An owner or repository name, as GitHub allows them.
fn is_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.len() <= 100
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_request_url_names_host_repository_and_number() {
        let pr = PullRequestRef::from_url("https://GitHub.com/Mengsig/Archon/pull/69").unwrap();
        assert_eq!(pr.host, "github.com");
        assert_eq!(pr.repository, "mengsig/archon");
        assert_eq!(pr.number, 69);
        assert_eq!(pr.url(), "https://github.com/mengsig/archon/pull/69");
        assert_eq!(pr.to_string(), "github.com/mengsig/archon#69");
        let enterprise = PullRequestRef::from_url("https://ghe.example:8443/o/r/pull/1").unwrap();
        assert_eq!(enterprise.host, "ghe.example:8443");
    }

    #[test]
    fn a_pull_request_is_stored_as_its_url_and_read_back_only_from_one() {
        let pr = PullRequestRef::from_url("https://github.com/o/r/pull/7").unwrap();
        let json = serde_json::to_string(&pr).unwrap();
        assert_eq!(json, "\"https://github.com/o/r/pull/7\"");
        assert_eq!(serde_json::from_str::<PullRequestRef>(&json).unwrap(), pr);
        let bad = serde_json::from_str::<PullRequestRef>("\"https://github.com/o/r/pull/0\"");
        assert!(bad.is_err());
    }

    #[test]
    fn a_head_in_another_repository_is_a_fork() {
        let mut onto = OntoPullRequest {
            target: OntoTarget::Pr(7),
            pr: PullRequestRef::from_url("https://github.com/o/r/pull/7").unwrap(),
            node_id: None,
            head_repository: "o/r".into(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            head_sha: "a".repeat(40),
            review_only: true,
        };
        assert!(!onto.is_fork());
        let stored = serde_json::to_value(&onto).unwrap();
        assert_eq!(stored["review_only"], true);
        onto.review_only = false;
        let delivering = serde_json::to_value(&onto).unwrap();
        assert!(delivering.get("review_only").is_none(), "{delivering}");
        let read: OntoPullRequest = serde_json::from_value(delivering).unwrap();
        assert!(!read.review_only, "an older record delivers");
        onto.head_repository = "contributor/r".into();
        assert!(onto.is_fork());
    }

    #[test]
    fn an_onto_target_names_one_kind_on_the_wire() {
        let pr: OntoTarget = serde_json::from_str(r#"{"pr":100}"#).unwrap();
        assert_eq!(pr, OntoTarget::Pr(100));
        let branch: OntoTarget = serde_json::from_str(r#"{"branch":"feat/x"}"#).unwrap();
        assert_eq!(branch, OntoTarget::Branch("feat/x".into()));
        assert!(serde_json::from_str::<OntoTarget>(r#"{"pr":1,"branch":"b"}"#).is_err());
        assert!(serde_json::from_str::<OntoTarget>(r#"{"tag":"v1"}"#).is_err());
    }

    #[test]
    fn any_other_url_names_no_pull_request() {
        for url in [
            "http://github.com/o/r/pull/1",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/r/pull/01",
            "https://github.com/o/r/pull/1/",
            "https://github.com/o/r/pull/1?x=1",
            "https://github.com/o/r/pull/1#discussion",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/pull/1",
            "https://github.com/o/r/x/pull/1",
            "https://github.com/o/../pull/1",
            "https://github.com/o/r/pull/99999999999999999999",
            "https:///o/r/pull/1",
            "https://example.invalid/pr/1",
        ] {
            assert_eq!(PullRequestRef::from_url(url), None, "{url}");
        }
    }
}
