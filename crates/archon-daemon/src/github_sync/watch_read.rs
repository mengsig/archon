//! The `GET`s a repository watch sends: pages of the open pull requests, and
//! one pull request's comment or review count. Each is a read-only `gh api` call.

use std::time::Duration;

use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::repo::{GH_HOST, Repo};
use archon_queue::store::MAX_ETAG_BYTES;

use super::fetch::{self, Response};
use crate::executor::gh;

/// Pull requests one page lists.
pub(crate) const PAGE: usize = 100;

/// Pages of the list one walk follows at most: past these, the oldest pull
/// requests are not watched, and the watch's view says the list is truncated.
pub(crate) const MAX_PAGES: u32 = 10;

/// Page `page` of `repository`'s open pull requests, newest first: an order
/// only an opening or a closing shifts.
pub(crate) fn list_endpoint(repository: &str, page: u32) -> String {
    assert!((1..=MAX_PAGES).contains(&page), "a bounded page: {page}");
    format!(
        "repos/{repository}/pulls?state=open&sort=created&direction=desc&per_page={PAGE}&page={page}"
    )
}

/// What a pull request's count is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    Comments,
    Reviews,
}

/// #`number`'s comments or reviews one to a page, so the last page GitHub
/// names is their count. A review the login began counts before it is submitted.
pub(crate) fn count_endpoint(repository: &str, number: u64, counted: Counted) -> String {
    assert!(number > 0, "a pull request");
    match counted {
        Counted::Comments => format!("repos/{repository}/issues/{number}/comments?per_page=1"),
        Counted::Reviews => format!("repos/{repository}/pulls/{number}/reviews?per_page=1"),
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
    NotModified {
        /// GitHub's clock when it answered, from the `Date` header.
        date: Option<Timestamp>,
    },
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
    let date = response
        .header("date")
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    let got = match response.status {
        200 => Ok(Got::Changed {
            etag: response
                .header("etag")
                .filter(|e| e.len() <= MAX_ETAG_BYTES)
                .map(str::to_owned),
            last_page: response.header("link").and_then(last_page),
            date,
            body: response.body,
        }),
        304 if etag.is_some() => Ok(Got::NotModified { date }),
        304 => Err(ArchonError::Protocol(format!(
            "GitHub answered 304 to the unconditional {endpoint}"
        ))),
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

/// The count a `per_page=1` answer tells: the last page it names, or the one
/// page's own length when it names none.
pub(crate) fn count(body: &str, last_page: Option<u32>) -> Result<u64, ArchonError> {
    let items: Vec<serde::de::IgnoredAny> = serde_json::from_str(body)
        .map_err(|e| ArchonError::Protocol(format!("GitHub's count did not parse: {e}")))?;
    match (items.len(), last_page) {
        (0 | 1, None) => Ok(items.len() as u64),
        (1, Some(last)) => Ok(u64::from(last)),
        (length, last) => Err(ArchonError::Protocol(format!(
            "GitHub's count had {length} items on a page of one, the last page {last:?}"
        ))),
    }
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
            args.ends_with("pulls?state=open&sort=created&direction=desc&per_page=100&page=1"),
            "{args}"
        );
        let comments = get_args(&count_endpoint("o/r", 7, Counted::Comments), None).join(" ");
        assert!(!comments.contains("If-None-Match"));
        assert!(
            comments.ends_with("repos/o/r/issues/7/comments?per_page=1"),
            "{comments}"
        );
        assert_eq!(
            count_endpoint("o/r", 7, Counted::Reviews),
            "repos/o/r/pulls/7/reviews?per_page=1"
        );
    }

    #[test]
    fn a_link_header_names_the_last_page() {
        let link = "<https://api.github.com/repositories/1/issues/7/comments?per_page=1&page=2>; \
                    rel=\"next\", <https://api.github.com/repositories/1/issues/7/comments?per_page=1&page=4>; rel=\"last\"";
        assert_eq!(last_page(link), Some(4));
        assert_eq!(last_page("<https://x/y?page=2>; rel=\"next\""), None);
    }

    #[test]
    fn a_count_is_the_last_page_or_the_one_pages_length() {
        assert_eq!(count("[]", None).unwrap(), 0);
        assert_eq!(count(r#"[{"id":3}]"#, None).unwrap(), 1);
        assert_eq!(count(r#"[{"id":3}]"#, Some(41)).unwrap(), 41);
        assert!(count("[]", Some(3)).is_err(), "a last page of nothing");
        assert!(count(r#"[{"id":3},{"id":4}]"#, None).is_err());
        assert!(count("{}", None).is_err());
    }
}
