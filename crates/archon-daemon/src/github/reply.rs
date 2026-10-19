//! `github_reply`: the orchestrator's comment on a thread of a repository
//! Archon watches, posted with `gh` under archond's login and recorded in
//! the feed (`archon log`).

use std::path::Path;
use std::process::Stdio;

use archon_core::error::ArchonError;
use archon_core::github::{GithubThread, REPLY_BUDGET, REPLY_MARKER};
use archon_core::notify::NotificationKind;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::Instant;

use super::{require_dedicated, view};
use crate::context::DaemonContext;
use crate::feed;

/// GitHub refuses comments over 65536 characters; the marker needs room.
pub const MAX_BODY_CHARS: usize = 65_000;
/// Most characters of `gh`'s complaint an error carries.
const MAX_ERROR_CHARS: usize = 2_000;

/// `body` ready to post: non-empty, bounded, ending in exactly one marker
/// so Aether's watcher never takes the comment as a new request.
pub fn reply_body(body: &str) -> Result<String, ArchonError> {
    let text = strip_markers(body);
    let text = text.trim();
    if text.is_empty() {
        return Err(ArchonError::Invalid("a GitHub reply needs a body".into()));
    }
    let chars = text.chars().count();
    if chars > MAX_BODY_CHARS {
        return Err(ArchonError::Invalid(format!(
            "a GitHub reply holds at most {MAX_BODY_CHARS} characters, not {chars}; \
             link a report instead"
        )));
    }
    let posted = format!("{text}\n\n{REPLY_MARKER}");
    assert!(
        posted.ends_with(REPLY_MARKER),
        "the marker closes every reply"
    );
    assert_eq!(posted.matches(REPLY_MARKER).count(), 1, "one marker");
    Ok(posted)
}

/// `body` without the marker, also where removing one marker joins the
/// halves of another; each pass shortens the text, so it ends.
fn strip_markers(body: &str) -> String {
    let mut text = body.to_owned();
    while text.contains(REPLY_MARKER) {
        text = text.replace(REPLY_MARKER, "");
    }
    assert!(!text.contains(REPLY_MARKER), "no marker survives stripping");
    text
}

/// Post `body` on `thread`, only in a repository Archon's watcher sends to
/// the orchestrator, all within [`REPLY_BUDGET`]; the comment's URL.
pub async fn post_reply(
    ctx: &DaemonContext,
    thread: &GithubThread,
    body: &str,
) -> Result<String, ArchonError> {
    require_dedicated(ctx)?;
    let body = reply_body(body)?;
    let deadline = Instant::now() + REPLY_BUDGET;
    let client = ctx.aether.client()?;
    let status = tokio::time::timeout_at(deadline, client.github_watcher())
        .await
        .map_err(|_| {
            ArchonError::Unavailable(format!(
                "Aether did not show its GitHub watcher within {REPLY_BUDGET:?}; nothing \
                 was posted on {thread}, so retry shortly"
            ))
        })??;
    let watched = view(&status, None).repositories.into_iter().find(|r| {
        r.repository
            .eq_ignore_ascii_case(thread.repository.as_str())
    });
    match watched {
        None => {
            return Err(ArchonError::Forbidden(format!(
                "{} is not a repository Archon watches; github_reply only answers \
                 threads of watched repositories",
                thread.repository
            )));
        }
        Some(repo) if !repo.is_archons() => {
            return Err(ArchonError::Forbidden(format!(
                "{} is watched for another agent, not Archon's orchestrator",
                thread.repository
            )));
        }
        Some(_) => {}
    }
    let url = gh_comment(&ctx.config.github.binary, thread, &body, deadline).await?;
    let note = format!("Replied on GitHub {thread}: {url}");
    tracing::info!(%thread, %url, "posted a GitHub reply");
    if let Err(error) = feed::notify(ctx, NotificationKind::Progress, note, None, None).await {
        // Posted already: a retry would post twice, so say where it went.
        return Err(ArchonError::Store(format!(
            "posted {url} but could not record it: {error}"
        )));
    }
    Ok(url)
}

/// `gh api` the comment in, pinned to github.com as Aether's watcher is,
/// whatever `GH_HOST` says.
fn gh_command(gh: &Path, endpoint: &str) -> Command {
    let mut command = Command::new(gh);
    command
        .args([
            "api",
            "--hostname",
            "github.com",
            "--method",
            "POST",
            endpoint,
        ])
        .args(["--input", "-"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Post `body` with `gh` by `deadline`, stdin included; the comment's
/// `html_url`. Past the deadline `gh` is killed and the outcome is unknown.
async fn gh_comment(
    gh: &Path,
    thread: &GithubThread,
    body: &str,
    deadline: Instant,
) -> Result<String, ArchonError> {
    let endpoint = format!(
        "repos/{}/issues/{}/comments",
        thread.repository, thread.number
    );
    let context = || format!("{} api {endpoint}", gh.display());
    let mut child = gh_command(gh, &endpoint)
        .spawn()
        .map_err(|e| ArchonError::io(context(), e))?;
    let input = serde_json::json!({ "body": body }).to_string();
    let mut stdin = child.stdin.take().expect("stdin was requested as a pipe");
    // Written while `gh` runs: one that stops reading cannot outlast the deadline.
    let write = async move {
        let written = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
        written
    };
    let run = async { tokio::join!(write, child.wait_with_output()) };
    let (written, output) = tokio::time::timeout_at(deadline, run).await.map_err(|_| {
        ArchonError::Unavailable(format!(
            "{} did not finish within {REPLY_BUDGET:?} and was stopped; {}",
            context(),
            thread.check_before_retry()
        ))
    })?;
    let output = output.map_err(|e| ArchonError::io(context(), e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let why: String = stderr.trim().chars().take(MAX_ERROR_CHARS).collect();
        return Err(ArchonError::Unavailable(format!(
            "{} failed ({}): {why}",
            context(),
            output.status
        )));
    }
    written.map_err(|e| ArchonError::io(context(), e))?;
    comment_url(&output.stdout).ok_or_else(|| {
        ArchonError::Protocol(format!(
            "{} answered without a comment URL; {}",
            context(),
            thread.check_before_retry()
        ))
    })
}

fn comment_url(stdout: &[u8]) -> Option<String> {
    let reply: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let url = reply.get("html_url")?.as_str()?;
    url.starts_with("https://").then(|| url.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_split_marker_is_stripped_to_one_closing_marker() {
        let (head, tail) = REPLY_MARKER.split_at(10);
        let split = format!("hi {head}{REPLY_MARKER}{tail} there");
        let posted = reply_body(&split).unwrap();
        assert_eq!(posted, format!("hi  there\n\n{REPLY_MARKER}"));
        assert_eq!(posted.matches(REPLY_MARKER).count(), 1);
    }

    fn thread() -> GithubThread {
        serde_json::from_value(serde_json::json!({"repository": "o/r", "number": 5})).unwrap()
    }

    /// An executable `gh` in `dir` running `script`.
    fn fake_gh(dir: &Path, script: &str) -> std::path::PathBuf {
        let path = dir.join("gh");
        archon_core::testing::write_script(&path, &format!("#!/bin/sh\n{script}\n"));
        path
    }

    /// `gh_comment` against `script` with a one-second deadline: its result
    /// and how long it took.
    async fn post_within_a_second(
        script: &str,
        body: &str,
    ) -> (Result<String, ArchonError>, Duration) {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), script);
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let result = gh_comment(&gh, &thread(), body, deadline).await;
        (result, started.elapsed())
    }

    #[test]
    fn replies_go_to_github_com_whatever_gh_host_says() {
        let command = gh_command(Path::new("gh"), "repos/o/r/issues/5/comments");
        let command = command.as_std();
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        let host = args.iter().position(|a| *a == "--hostname").unwrap();
        assert_eq!(args[host + 1], "github.com");
        assert_eq!(
            args,
            [
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                "repos/o/r/issues/5/comments",
                "--input",
                "-"
            ]
        );
    }

    #[tokio::test]
    async fn a_gh_that_stops_reading_stdin_is_stopped_at_the_deadline() {
        // Far more than a pipe buffer, so the write blocks until `gh` dies.
        let body = "é".repeat(MAX_BODY_CHARS);
        let (result, took) = post_within_a_second("exec sleep 30", &body).await;
        let Err(ArchonError::Unavailable(why)) = result else {
            panic!("a stalled gh gave {result:?}");
        };
        assert!(why.contains("https://github.com/o/r/issues/5"), "{why}");
        assert!(why.contains("never repeat"), "{why}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }

    #[tokio::test]
    async fn a_slow_answer_is_stopped_at_the_deadline() {
        let (result, took) = post_within_a_second("cat > /dev/null\nexec sleep 30", "hi").await;
        let Err(ArchonError::Unavailable(why)) = result else {
            panic!("a slow gh gave {result:?}");
        };
        assert!(why.contains("may have been posted"), "{why}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }

    #[tokio::test]
    async fn gh_failing_early_reports_its_complaint() {
        let body = "x".repeat(MAX_BODY_CHARS);
        let script = "echo 'HTTP 404: Not Found' >&2\nexit 1";
        let (result, _) = post_within_a_second(script, &body).await;
        let Err(ArchonError::Unavailable(why)) = result else {
            panic!("a failing gh gave {result:?}");
        };
        assert!(why.contains("HTTP 404"), "{why}");
    }

    #[test]
    fn every_reply_ends_in_exactly_one_marker() {
        let posted = reply_body("  Queued as qi_1.  ").unwrap();
        assert_eq!(posted, format!("Queued as qi_1.\n\n{REPLY_MARKER}"));
        let marked = reply_body(&format!("{REPLY_MARKER}\nDone.\n{REPLY_MARKER}")).unwrap();
        assert_eq!(marked, format!("Done.\n\n{REPLY_MARKER}"));
    }

    #[test]
    fn empty_and_oversized_replies_are_refused() {
        for empty in ["", "  \n", REPLY_MARKER] {
            assert!(matches!(reply_body(empty), Err(ArchonError::Invalid(_))));
        }
        let long = "x".repeat(MAX_BODY_CHARS + 1);
        assert!(matches!(reply_body(&long), Err(ArchonError::Invalid(_))));
        assert!(reply_body(&"é".repeat(MAX_BODY_CHARS)).is_ok());
    }

    #[test]
    fn the_comment_url_comes_from_ghs_json() {
        let json = br#"{"id": 1, "html_url": "https://github.com/o/r/issues/2#issuecomment-1"}"#;
        assert_eq!(
            comment_url(json).as_deref(),
            Some("https://github.com/o/r/issues/2#issuecomment-1")
        );
        assert_eq!(comment_url(b"not json"), None);
        assert_eq!(comment_url(br#"{"html_url": "javascript:x"}"#), None);
    }
}
