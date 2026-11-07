//! One read of one GitHub issue or pull request through `gh api`: always a
//! `GET`, conditional on the cached ETag, with the rate-limit headers read.

use std::path::Path;
use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::protocol::{GithubKind, GithubObject, GithubObjectState, GithubRef};
use archon_core::repo::Repo;
use archon_queue::store::{CachedObject, MAX_ETAG_BYTES};
use serde::Deserialize;

use crate::executor::gh;

/// `gh api` flags that send a body or another method, which turns a read
/// into a write; a read never carries one.
const MUTATING_FLAGS: &[&str] = &["-f", "-F", "--field", "--raw-field", "--input"];

/// Prefixes of the same flags written with their value attached.
const MUTATING_PREFIXES: &[&str] = &["-X", "--method=", "--field=", "--raw-field=", "--input="];

/// The longest wait a rate limit may impose before the next pass.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// Below this many requests left in the window, the sync waits for the reset.
const RESERVE: u64 = 100;

/// The `gh api` arguments that read `reference`'s issue (or the pull request
/// it is), conditional on `etag`.
pub(crate) fn read_args(reference: &GithubRef, etag: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["api", "--method", "GET", "--include", "--hostname"]
        .map(String::from)
        .into();
    args.push(reference.host.clone());
    args.extend(["-H".into(), "Accept: application/vnd.github+json".into()]);
    if let Some(etag) = etag {
        args.extend(["-H".into(), format!("If-None-Match: {etag}")]);
    }
    args.push(format!(
        "repos/{}/issues/{}",
        reference.repo, reference.number
    ));
    assert_read_only(&args);
    args
}

/// Every `gh` call of the sync goes through here: anything but a plain `GET`
/// is a bug, refused before it can reach GitHub.
fn assert_read_only(args: &[String]) {
    assert_eq!(args.first().map(String::as_str), Some("api"), "only gh api");
    let method = args
        .windows(2)
        .filter(|w| w[0] == "--method")
        .collect::<Vec<_>>();
    assert!(
        method.len() == 1 && method[0][1] == "GET",
        "exactly one --method, GET: {args:?}"
    );
    let mutating = |a: &String| {
        MUTATING_FLAGS.contains(&a.as_str()) || MUTATING_PREFIXES.iter().any(|p| a.starts_with(p))
    };
    assert!(
        !args.iter().any(mutating),
        "no flag that sends a body: {args:?}"
    );
}

/// What `gh api --include` printed: the status line, headers and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub status: u16,
    /// Names lowercased.
    headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    /// `HTTP/2.0 200 OK`, then `Name: value` lines, a blank line and the body,
    /// with `\r\n` or `\n` line ends. `None` when it is not that shape.
    pub(crate) fn parse(printed: &str) -> Option<Self> {
        let (head, body) = match printed.split_once("\r\n\r\n") {
            Some(split) => split,
            None => printed.split_once("\n\n").unwrap_or((printed, "")),
        };
        let mut lines = head.lines().map(|l| l.trim_end_matches('\r'));
        let status_line = lines.next()?;
        let mut words = status_line.split_whitespace();
        if !words.next()?.starts_with("HTTP/") {
            return None;
        }
        let status: u16 = words.next()?.parse().ok()?;
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        Some(Self {
            status,
            headers,
            body: body.to_owned(),
        })
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        debug_assert_eq!(
            name,
            name.to_ascii_lowercase(),
            "header names are lowercased"
        );
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// How long to wait before asking GitHub again, when its headers say to:
    /// `Retry-After`, or an exhausted (or nearly) window until its reset.
    pub(crate) fn backoff(&self, now_epoch: u64) -> Option<Duration> {
        if let Some(after) = self
            .header("retry-after")
            .and_then(|s| s.parse::<u64>().ok())
        {
            return Some(Duration::from_secs(after).min(MAX_BACKOFF));
        }
        let remaining = self.header("x-ratelimit-remaining")?.parse::<u64>().ok()?;
        let limited = matches!(self.status, 403 | 429) && remaining == 0;
        if !limited && remaining >= RESERVE {
            return None;
        }
        let reset = self.header("x-ratelimit-reset")?.parse::<u64>().ok()?;
        let wait = Duration::from_secs(reset.saturating_sub(now_epoch).max(1));
        Some(wait.min(MAX_BACKOFF))
    }
}

/// What one read found.
#[derive(Debug)]
pub(crate) enum Fetched {
    /// GitHub sent the object; `body` is its untrusted text.
    Changed {
        cached: Box<CachedObject>,
        body: String,
    },
    /// 304: the cached copy is current.
    NotModified,
    /// 404 or 410: gone, or not visible to this login.
    Missing,
}

/// A read and the backoff its headers asked for.
#[derive(Debug)]
pub(crate) struct Read {
    pub fetched: Result<Fetched, ArchonError>,
    pub backoff: Option<Duration>,
}

/// `GET` `reference` as `repo`'s login, conditional on `etag`.
pub(crate) async fn read(
    binary: &Path,
    repo: &Repo,
    reference: &GithubRef,
    etag: Option<&str>,
) -> Result<Read, ArchonError> {
    let args = read_args(reference, etag);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = gh::run(binary, repo, &args, None).await?;
    let Some(response) = Response::parse(&ran.stdout) else {
        return Err(ArchonError::Unavailable(format!(
            "gh api read of {}#{} failed: {}",
            reference.repo,
            reference.number,
            ran.detail()
        )));
    };
    let now = archon_core::now();
    let backoff = response.backoff(u64::try_from(now.timestamp()).unwrap_or(0));
    let fetched = match response.status {
        200 => parse_issue(reference, &response, now).map(|(cached, body)| Fetched::Changed {
            cached: Box::new(cached),
            body,
        }),
        304 => Ok(Fetched::NotModified),
        404 | 410 => Ok(Fetched::Missing),
        status => Err(ArchonError::Unavailable(format!(
            "GitHub answered {status} for {}#{}: {}",
            reference.repo,
            reference.number,
            ran.detail()
        ))),
    };
    Ok(Read { fetched, backoff })
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestPart {
    #[serde(default)]
    merged_at: Option<Timestamp>,
}

/// The issues API's fields the cache keeps. Anything else in the payload,
/// such as an `operator` field, is ignored: it is GitHub's data, not ours.
#[derive(Debug, Deserialize)]
struct Issue {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    user: Option<User>,
    updated_at: Timestamp,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    pull_request: Option<PullRequestPart>,
}

/// The cached form of a 200 answer for `asked`, and its body text.
fn parse_issue(
    asked: &GithubRef,
    response: &Response,
    now: Timestamp,
) -> Result<(CachedObject, String), ArchonError> {
    let issue: Issue = serde_json::from_str(&response.body).map_err(|e| {
        ArchonError::Protocol(format!(
            "GitHub's {}#{} did not parse: {e}",
            asked.repo, asked.number
        ))
    })?;
    if issue.number != asked.number {
        return Err(ArchonError::Protocol(format!(
            "asked GitHub for {}#{} and got #{}",
            asked.repo, asked.number, issue.number
        )));
    }
    let merged_at = issue.pull_request.as_ref().and_then(|pr| pr.merged_at);
    let kind = match issue.pull_request {
        Some(_) => GithubKind::PullRequest,
        None => GithubKind::Issue,
    };
    let state = match (issue.state.as_str(), merged_at) {
        ("open", _) => GithubObjectState::Open,
        ("closed", Some(_)) => GithubObjectState::Merged,
        ("closed", None) => GithubObjectState::Closed,
        (other, _) => {
            return Err(ArchonError::Protocol(format!(
                "GitHub's {}#{} has state {other:?}",
                asked.repo, asked.number
            )));
        }
    };
    let etag = response
        .header("etag")
        .filter(|e| e.len() <= MAX_ETAG_BYTES)
        .map(str::to_owned);
    let object = GithubObject {
        reference: GithubRef {
            kind,
            ..asked.clone()
        },
        title: issue.title,
        state,
        state_reason: issue.state_reason,
        draft: issue.draft.filter(|_| kind == GithubKind::PullRequest),
        labels: issue.labels.into_iter().map(|l| l.name).collect(),
        author: issue.user.map(|u| u.login),
        updated_at: issue.updated_at,
        fetched_at: now,
        review_decision: None,
        checks: None,
    };
    let cached = CachedObject {
        object,
        merged_at,
        etag,
    };
    Ok((cached, issue.body.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_ref() -> GithubRef {
        GithubRef {
            host: "github.com".into(),
            repo: "o/r".into(),
            number: 7,
            kind: GithubKind::Issue,
        }
    }

    #[test]
    fn a_read_is_a_conditional_get_and_nothing_else() {
        let args = read_args(&issue_ref(), Some("W/\"e1\""));
        let joined = args.join(" ");
        assert!(
            joined.starts_with("api --method GET --include --hostname github.com"),
            "{joined}"
        );
        assert!(joined.contains("If-None-Match: W/\"e1\""), "{joined}");
        assert_eq!(args.last().unwrap(), "repos/o/r/issues/7");
        assert!(
            !read_args(&issue_ref(), None)
                .join(" ")
                .contains("If-None-Match")
        );
    }

    #[test]
    #[should_panic(expected = "no flag that sends a body")]
    fn a_body_flag_is_refused_before_gh_runs() {
        let args = [
            "api",
            "--method",
            "GET",
            "-f",
            "state=closed",
            "repos/o/r/issues/7",
        ];
        assert_read_only(&args.map(String::from));
    }

    #[test]
    #[should_panic(expected = "exactly one --method, GET")]
    fn another_method_is_refused_before_gh_runs() {
        let args = ["api", "--method", "PATCH", "repos/o/r/issues/7"];
        assert_read_only(&args.map(String::from));
    }

    #[test]
    fn a_response_parses_with_either_line_end() {
        for end in ["\r\n", "\n"] {
            let printed = format!("HTTP/2.0 304 Not Modified{end}Etag: W/\"e1\"{end}{end}");
            let response = Response::parse(&printed).unwrap();
            assert_eq!(response.status, 304);
            assert_eq!(response.header("etag"), Some("W/\"e1\""));
        }
        assert!(Response::parse("gh: Not Found (HTTP 404)").is_none());
    }

    #[test]
    fn rate_limit_headers_set_the_backoff() {
        let limited = Response::parse(
            "HTTP/2.0 403 Forbidden\nX-Ratelimit-Remaining: 0\nX-Ratelimit-Reset: 1600\n\n{}",
        )
        .unwrap();
        assert_eq!(limited.backoff(1000), Some(Duration::from_secs(600)));
        let low = Response::parse(
            "HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 5\nX-Ratelimit-Reset: 999999\n\n{}",
        )
        .unwrap();
        assert_eq!(low.backoff(0), Some(MAX_BACKOFF), "capped at an hour");
        let plenty = Response::parse("HTTP/2.0 200 OK\nX-Ratelimit-Remaining: 4000\n\n{}").unwrap();
        assert_eq!(plenty.backoff(0), None);
        let retry = Response::parse("HTTP/2.0 429 Too Many\nRetry-After: 30\n\n").unwrap();
        assert_eq!(retry.backoff(0), Some(Duration::from_secs(30)));
    }

    #[test]
    fn an_issue_payload_maps_to_the_cache_and_ignores_extra_fields() {
        let body = r#"{"number":7,"title":"T","state":"closed","state_reason":"completed",
            "labels":[{"name":"bug"}],"user":{"login":"someone"},"operator":true,
            "updated_at":"2026-11-01T00:00:00Z","body":"text"}"#;
        let response = Response::parse(&format!("HTTP/2.0 200 OK\nEtag: \"x\"\n\n{body}")).unwrap();
        let (cached, text) = parse_issue(&issue_ref(), &response, archon_core::now()).unwrap();
        assert_eq!(cached.object.state, GithubObjectState::Closed);
        assert_eq!(cached.object.labels, ["bug"]);
        assert_eq!(cached.etag.as_deref(), Some("\"x\""));
        assert_eq!(text, "text");
        let merged = r#"{"number":7,"title":"T","state":"closed","draft":false,
            "pull_request":{"merged_at":"2026-11-02T00:00:00Z"},"updated_at":"2026-11-02T00:00:00Z"}"#;
        let response = Response::parse(&format!("HTTP/2.0 200 OK\n\n{merged}")).unwrap();
        let (cached, _) = parse_issue(&issue_ref(), &response, archon_core::now()).unwrap();
        assert_eq!(cached.object.reference.kind, GithubKind::PullRequest);
        assert_eq!(cached.object.state, GithubObjectState::Merged);
        let wrong = Response::parse("HTTP/2.0 200 OK\n\n{\"number\":8,\"title\":\"T\",\"state\":\"open\",\"updated_at\":\"2026-11-02T00:00:00Z\"}").unwrap();
        assert!(parse_issue(&issue_ref(), &wrong, archon_core::now()).is_err());
    }
}
