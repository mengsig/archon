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

/// Page `page` of the conversation comments on #`number` updated since
/// `since`, oldest first.
pub(crate) fn comments_endpoint(
    repository: &str,
    number: u64,
    since: Timestamp,
    page: u32,
) -> String {
    assert!(number > 0 && page > 0, "a pull request and a page");
    let since = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let endpoint =
        format!("repos/{repository}/issues/{number}/comments?since={since}&per_page={PAGE}");
    paged(endpoint, page)
}

/// Page `page` of #`number`'s reviews, oldest first.
pub(crate) fn reviews_endpoint(repository: &str, number: u64, page: u32) -> String {
    assert!(number > 0 && page > 0, "a pull request and a page");
    paged(
        format!("repos/{repository}/pulls/{number}/reviews?per_page={PAGE}"),
        page,
    )
}

fn paged(endpoint: String, page: u32) -> String {
    match page {
        1 => endpoint,
        page => format!("{endpoint}&page={page}"),
    }
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
    Changed {
        etag: Option<String>,
        body: String,
        /// The last page its `Link` header names, when there are more.
        last_page: Option<u32>,
        /// GitHub's clock when it answered, from the `Date` header.
        date: Option<Timestamp>,
    },
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
            last_page: response.header("link").and_then(last_page),
            date: response
                .header("date")
                .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
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

/// The page a `Link` header's `rel="last"` names.
fn last_page(link: &str) -> Option<u32> {
    let last = link.split(',').find(|part| part.contains("rel=\"last\""))?;
    let url = last
        .split(';')
        .next()?
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>');
    let query = url.split_once('?')?.1;
    let page = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("page="))?;
    page.parse().ok().filter(|page| *page > 1)
}

/// A comment or review: its id, and when it was made (`None` for a pending review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Activity {
    pub id: u64,
    pub at: Option<Timestamp>,
}

#[derive(Debug, Deserialize)]
struct Dated {
    id: u64,
    #[serde(default)]
    created_at: Option<Timestamp>,
    #[serde(default)]
    submitted_at: Option<Timestamp>,
}

/// One page of comments (`created_at`) or reviews (`submitted_at`).
pub(crate) fn activity(body: &str, what: &str) -> Result<Vec<Activity>, ArchonError> {
    let items: Vec<Dated> = serde_json::from_str(body)
        .map_err(|e| ArchonError::Protocol(format!("GitHub's {what} did not parse: {e}")))?;
    if items.len() > PAGE || items.iter().any(|i| i.id == 0) {
        return Err(ArchonError::Protocol(format!(
            "GitHub's {what} were over a page or had an id of 0"
        )));
    }
    let at = |i: &Dated| match what {
        "reviews" => i.submitted_at,
        _ => i.created_at,
    };
    Ok(items
        .iter()
        .map(|i| Activity {
            id: i.id,
            at: at(i),
        })
        .collect())
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
        let comments = get_args(&comments_endpoint("o/r", 7, since, 1), None).join(" ");
        assert!(!comments.contains("If-None-Match"));
        assert!(
            comments.ends_with("issues/7/comments?since=2026-11-01T00:00:00Z&per_page=100"),
            "{comments}"
        );
    }

    #[test]
    fn a_link_header_names_the_last_page() {
        let link = "<https://api.github.com/repositories/1/issues/7/comments?per_page=100&page=2>; \
                    rel=\"next\", <https://api.github.com/repositories/1/issues/7/comments?per_page=100&page=4>; rel=\"last\"";
        assert_eq!(last_page(link), Some(4));
        assert_eq!(last_page("<https://x/y?page=2>; rel=\"next\""), None);
        assert_eq!(
            reviews_endpoint("o/r", 7, 4),
            "repos/o/r/pulls/7/reviews?per_page=100&page=4"
        );
    }

    #[test]
    fn comments_and_reviews_read_with_their_own_dates() {
        let comments =
            r#"[{"id":3,"created_at":"2026-10-01T00:00:00Z","updated_at":"2026-11-02T00:00:00Z"}]"#;
        let read = activity(comments, "comments").unwrap();
        assert_eq!(read[0].at, Some("2026-10-01T00:00:00Z".parse().unwrap()));
        let pending = r#"[{"id":4,"submitted_at":null}]"#;
        assert_eq!(activity(pending, "reviews").unwrap()[0].at, None);
        assert!(activity("{}", "reviews").is_err());
        assert!(activity(r#"[{"id":0}]"#, "reviews").is_err());
    }
}
