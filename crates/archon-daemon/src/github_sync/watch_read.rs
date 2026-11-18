//! The `GET`s a repository watch sends: pages of the open pull requests,
//! newest update first, and one pull request's comments or reviews. Each is
//! a read-only `gh api` call as the registered repository's login.

use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::repo::{GH_HOST, Repo};
use archon_queue::store::MAX_ETAG_BYTES;
use serde::Deserialize;

use super::fetch::{self, Response};
use crate::executor::gh;

/// Items one page lists.
pub(crate) const PAGE: usize = 100;

/// Pages of the list one read follows at most: past these, the least
/// recently updated pull requests are not watched.
pub(crate) const MAX_PAGES: u32 = 10;

/// Page `page` of `repository`'s open pull requests, newest update first.
pub(crate) fn list_endpoint(repository: &str, page: u32) -> String {
    assert!((1..=MAX_PAGES).contains(&page), "a bounded page: {page}");
    format!(
        "repos/{repository}/pulls?state=open&sort=updated&direction=desc&per_page={PAGE}&page={page}"
    )
}

/// The conversation comments on #`number` updated since `since`.
pub(crate) fn comments_endpoint(repository: &str, number: u64, since: Timestamp) -> String {
    assert!(number > 0, "a pull request number");
    let since = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!("repos/{repository}/issues/{number}/comments?since={since}&per_page={PAGE}")
}

/// The first page of #`number`'s reviews, oldest first.
pub(crate) fn reviews_endpoint(repository: &str, number: u64) -> String {
    assert!(number > 0, "a pull request number");
    format!("repos/{repository}/pulls/{number}/reviews?per_page={PAGE}")
}

/// The `gh api` arguments that `GET` `endpoint`, conditional on `etag`.
pub(crate) fn get_args(endpoint: &str, etag: Option<&str>) -> Vec<String> {
    assert!(
        endpoint.starts_with("repos/"),
        "a repository endpoint: {endpoint}"
    );
    let mut args: Vec<String> = ["api", "--method", "GET", "--include", "--hostname", GH_HOST]
        .map(String::from)
        .into();
    args.extend(["-H".into(), "Accept: application/vnd.github+json".into()]);
    if let Some(etag) = etag {
        args.extend(["-H".into(), format!("If-None-Match: {etag}")]);
    }
    args.push(endpoint.to_owned());
    fetch::assert_read_only(&args);
    args
}

/// What one `GET` found.
#[derive(Debug)]
pub(crate) enum Got {
    Changed { etag: Option<String>, body: String },
    NotModified,
}

/// A `GET` and the backoff its headers asked for.
#[derive(Debug)]
pub(crate) struct Answer {
    pub got: Result<Got, ArchonError>,
    pub backoff: Option<Duration>,
}

/// `GET` `endpoint` as `repo`'s login. Only a failure to run `gh` is an `Err`.
pub(crate) async fn get(
    binary: &std::path::Path,
    repo: &Repo,
    endpoint: &str,
    etag: Option<&str>,
) -> Result<Answer, ArchonError> {
    let args = get_args(endpoint, etag);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = gh::run(binary, repo, &args, None).await?;
    let Some(response) = Response::parse(&ran.stdout) else {
        return Err(ArchonError::Unavailable(format!(
            "gh api {endpoint} failed: {}",
            ran.detail()
        )));
    };
    let now = u64::try_from(archon_core::now().timestamp()).unwrap_or(0);
    let backoff = response.backoff(now);
    let got = match response.status {
        200 => Ok(Got::Changed {
            etag: response
                .header("etag")
                .filter(|e| e.len() <= MAX_ETAG_BYTES)
                .map(str::to_owned),
            body: response.body,
        }),
        304 => Ok(Got::NotModified),
        status => Err(ArchonError::Unavailable(format!(
            "GitHub answered {status} for {endpoint}: {}",
            ran.detail()
        ))),
    };
    Ok(Answer { got, backoff })
}

#[derive(Debug, Deserialize)]
struct Dated {
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(default)]
    submitted_at: Option<Timestamp>,
}

/// Whether a page of comments or reviews holds one made after `since`. A
/// full page counts as news: the newest may be on a later page.
pub(crate) fn has_newer(body: &str, since: Timestamp, what: &str) -> Result<bool, ArchonError> {
    let items: Vec<Dated> = serde_json::from_str(body)
        .map_err(|e| ArchonError::Protocol(format!("GitHub's {what} did not parse: {e}")))?;
    if items.len() >= PAGE {
        return Ok(true);
    }
    let newer = items
        .iter()
        .filter_map(|i| i.submitted_at.or(i.created_at))
        .any(|at| at > since);
    Ok(newer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_watch_read_is_a_get_and_only_the_given_etag_makes_it_conditional() {
        let args = get_args(&list_endpoint("o/r", 1), Some("W/\"l1\"")).join(" ");
        assert!(
            args.starts_with("api --method GET --include --hostname github.com"),
            "{args}"
        );
        assert!(args.contains("If-None-Match: W/\"l1\""), "{args}");
        assert!(
            args.ends_with("pulls?state=open&sort=updated&direction=desc&per_page=100&page=1"),
            "{args}"
        );
        let since: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
        let comments = get_args(&comments_endpoint("o/r", 7, since), None).join(" ");
        assert!(!comments.contains("If-None-Match"));
        assert!(
            comments.ends_with("issues/7/comments?since=2026-11-01T00:00:00Z&per_page=100"),
            "{comments}"
        );
    }

    #[test]
    fn only_a_comment_or_review_after_the_mark_is_news() {
        let since: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
        let old = r#"[{"created_at":"2026-10-01T00:00:00Z","updated_at":"2026-11-02T00:00:00Z"}]"#;
        assert!(
            !has_newer(old, since, "comments").unwrap(),
            "an edited old comment"
        );
        let new = r#"[{"created_at":"2026-11-02T00:00:00Z"}]"#;
        assert!(has_newer(new, since, "comments").unwrap());
        let pending = r#"[{"submitted_at":null}]"#;
        assert!(
            !has_newer(pending, since, "reviews").unwrap(),
            "a pending review"
        );
        let reviewed = r#"[{"submitted_at":"2026-11-03T00:00:00Z"}]"#;
        assert!(has_newer(reviewed, since, "reviews").unwrap());
        assert!(has_newer("{}", since, "reviews").is_err());
    }
}
