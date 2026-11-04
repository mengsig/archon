use std::path::PathBuf;

use archon_core::protocol::GithubKind;

use super::*;

/// A `gh` that records each POST's stdin as `posts/<id>.json` beside itself
/// and answers a GET with every comment posted so far. `fail-post` makes a
/// POST fail unposted, `lose-answer` posts it and then fails, `fail-get`
/// makes a GET fail.
struct StubGh {
    dir: tempfile::TempDir,
}

impl StubGh {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("posts")).unwrap();
        let script = format!(
            r#"#!/bin/sh
dir='{dir}'
case "$*" in
  *"--method POST"*)
    input=$(cat)
    [ -e "$dir/fail-post" ] && {{ echo 'HTTP 502: Bad Gateway' >&2; exit 1; }}
    n=$(( $(ls "$dir/posts" | wc -l) + 1 ))
    printf '%s' "$input" > "$dir/posts/$n.json"
    [ -e "$dir/lose-answer" ] && {{ echo 'connection reset' >&2; exit 1; }}
    printf '{{"id": %s, "html_url": "https://github.com/o/r/issues/5#issuecomment-%s"}}\n' "$n" "$n" ;;
  *"--method GET"*)
    [ -e "$dir/fail-get" ] && {{ echo 'HTTP 502: Bad Gateway' >&2; exit 1; }}
    printf '['; sep=''
    for f in "$dir"/posts/*.json; do
      [ -e "$f" ] || continue
      printf '%s{{"id":%s,%s' "$sep" "$(basename "$f" .json)" "$(tail -c +2 "$f")"; sep=','
    done
    printf ']\n' ;;
  *) echo "unexpected gh $*" >&2; exit 2 ;;
esac
"#,
            dir = dir.path().display()
        );
        archon_core::testing::write_script(&dir.path().join("gh"), &script);
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("gh")
    }

    fn set(&self, mode: &str, on: bool) {
        let flag = self.dir.path().join(mode);
        if on {
            std::fs::write(flag, "").unwrap();
        } else {
            let _ = std::fs::remove_file(flag);
        }
    }

    /// The bodies posted, in order.
    fn posts(&self) -> Vec<String> {
        let mut posts: Vec<(u64, String)> = std::fs::read_dir(self.dir.path().join("posts"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let id = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
                let input: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                (id, input["body"].as_str().unwrap().to_owned())
            })
            .collect();
        posts.sort();
        posts.into_iter().map(|(_, body)| body).collect()
    }
}

fn reply_intent() -> OutboxIntent {
    let token = OutboxToken::new();
    OutboxIntent {
        body: format!("Queued as qi_1.\n\n{}", marker(&token)),
        token,
        purpose: OutboxPurpose::Reply,
        item: None,
        run: None,
        target: GithubRef {
            host: GH_HOST.into(),
            repo: "o/r".into(),
            number: 5,
            kind: GithubKind::Issue,
        },
    }
}

/// Record `intent` due at once, as a restart finds a row whose lease ran out.
fn record_due(store: &Mutex<Store>, intent: &OutboxIntent) -> OutboxRow {
    let now = archon_core::now();
    store
        .try_lock()
        .unwrap()
        .enqueue_outbox(intent, &now, &now)
        .unwrap()
}

fn state_of(store: &Mutex<Store>, token: &OutboxToken) -> OutboxState {
    store
        .try_lock()
        .unwrap()
        .outbox_row(token)
        .unwrap()
        .unwrap()
        .state
}

fn soon() -> Instant {
    Instant::now() + REPLY_BUDGET
}

/// The acceptance case: archond dies after GitHub took the comment and before
/// the outbox recorded it. The restarted archond finds it and posts nothing.
#[tokio::test]
async fn a_crash_after_the_post_and_before_its_commit_does_not_post_twice() {
    let gh = StubGh::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let intent = reply_intent();
    {
        let store = Mutex::new(Store::open(&path).unwrap());
        record_due(&store, &intent);
        let posted = create_comment(&gh.path(), &intent.target, &intent.body, soon()).await;
        assert_eq!(posted.unwrap().comment_id, 1);
        // The crash: the store goes away before `mark_outbox_posted` runs.
    }
    let store = Mutex::new(Store::open(&path).unwrap());
    assert!(matches!(
        state_of(&store, &intent.token),
        OutboxState::Pending { attempts: 0, .. }
    ));
    assert_eq!(pass(&store, &gh.path()).await.unwrap(), 1);
    assert_eq!(
        gh.posts(),
        std::slice::from_ref(&intent.body),
        "posted once"
    );
    let OutboxState::Posted {
        comment_id, url, ..
    } = state_of(&store, &intent.token)
    else {
        panic!("the lookup marks it posted");
    };
    assert_eq!(
        (comment_id, url.as_deref()),
        (1, Some("https://github.com/o/r/issues/5#issuecomment-1"))
    );
    assert_eq!(
        pass(&store, &gh.path()).await.unwrap(),
        0,
        "nothing left due"
    );
    assert_eq!(gh.posts().len(), 1);
}

/// A crash after the intent and before the post: the restart posts it, once.
#[tokio::test]
async fn a_crash_before_the_post_posts_it_once_on_restart() {
    let gh = StubGh::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archon.db");
    let intent = reply_intent();
    record_due(&Mutex::new(Store::open(&path).unwrap()), &intent);
    let store = Mutex::new(Store::open(&path).unwrap());
    assert_eq!(pass(&store, &gh.path()).await.unwrap(), 1);
    assert_eq!(pass(&store, &gh.path()).await.unwrap(), 0);
    assert_eq!(gh.posts(), std::slice::from_ref(&intent.body));
    assert!(matches!(
        state_of(&store, &intent.token),
        OutboxState::Posted { comment_id: 1, .. }
    ));
}

/// GitHub took the comment but its answer was lost: the sender is told not
/// to send again, and the worker's lookup settles the row without a post.
#[tokio::test]
async fn a_lost_answer_is_settled_by_the_lookup_not_a_second_post() {
    let gh = StubGh::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    gh.set("lose-answer", true);
    let intent = reply_intent();
    let refused = send(&store, &gh.path(), intent.clone(), soon()).await;
    let Err(ArchonError::Unavailable(why)) = refused else {
        panic!("a lost answer gave {refused:?}");
    };
    assert!(
        why.contains("never send it again") && why.contains(intent.token.as_str()),
        "{why}"
    );
    let OutboxState::Pending {
        attempts,
        next_attempt_at,
        last_error,
    } = state_of(&store, &intent.token)
    else {
        panic!("an unconfirmed post stays pending");
    };
    assert_eq!(attempts, 1);
    assert!(next_attempt_at > archon_core::now(), "backed off");
    assert!(last_error.unwrap().contains("connection reset"));
    assert_eq!(pass(&store, &gh.path()).await.unwrap(), 0, "not due yet");
    store
        .try_lock()
        .unwrap()
        .mark_outbox_failed(&intent.token, "test: due now", &archon_core::now())
        .unwrap();
    gh.set("lose-answer", false);
    assert_eq!(pass(&store, &gh.path()).await.unwrap(), 1);
    assert_eq!(gh.posts().len(), 1, "found by its marker, not posted again");
}

/// A post GitHub refused stays pending and listed, and a lookup that fails
/// never posts: whether the comment is there is unknown.
#[tokio::test]
async fn failures_stay_pending_and_a_failed_lookup_never_posts() {
    let gh = StubGh::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    gh.set("fail-post", true);
    let intent = reply_intent();
    assert!(
        send(&store, &gh.path(), intent.clone(), soon())
            .await
            .is_err()
    );
    assert!(gh.posts().is_empty());
    let (pending, total) = store.try_lock().unwrap().pending_outbox(10).unwrap();
    assert_eq!((pending.len(), total), (1, 1), "visible, not dropped");
    gh.set("fail-post", false);
    gh.set("fail-get", true);
    let row = record_due(&store, &reply_intent());
    assert!(reconcile(&store, &gh.path(), &row).await.is_err());
    assert!(gh.posts().is_empty(), "no post without a lookup");
    assert!(matches!(
        state_of(&store, &row.intent.token),
        OutboxState::Pending { attempts: 1, .. }
    ));
}

#[tokio::test]
async fn a_send_posts_once_and_records_githubs_id() {
    let gh = StubGh::new();
    let store = Mutex::new(Store::open_in_memory().unwrap());
    let intent = reply_intent();
    let posted = send(&store, &gh.path(), intent.clone(), soon())
        .await
        .unwrap();
    assert_eq!(posted.comment_id, 1);
    assert_eq!(gh.posts(), std::slice::from_ref(&intent.body));
    assert!(matches!(
        state_of(&store, &intent.token),
        OutboxState::Posted { comment_id: 1, .. }
    ));
    let (pending, total) = store.try_lock().unwrap().pending_outbox(10).unwrap();
    assert!(pending.is_empty() && total == 0);
}

#[test]
fn every_call_goes_to_github_com_and_only_reads_or_posts() {
    let command = gh_api(
        Path::new("gh"),
        "POST",
        &["repos/o/r/issues/5/comments", "--input", "-"],
    );
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
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
    let patch = std::panic::catch_unwind(|| gh_api(Path::new("gh"), "PATCH", &[]));
    assert!(patch.is_err(), "no other method");
}

#[test]
fn markers_in_supplied_text_are_defused_and_backoff_is_bounded() {
    let token = OutboxToken::new();
    let pasted = format!("see {} and {}", marker(&token), &MARKER_PREFIX[..8]);
    let stripped = strip_markers(&format!("{}{pasted}", &MARKER_PREFIX[..5]));
    assert!(!stripped.contains(MARKER_PREFIX), "{stripped}");
    assert_eq!(backoff(1), FIRST_BACKOFF);
    assert_eq!(backoff(2), FIRST_BACKOFF * 2);
    assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
}

/// The production source of every crate: test files, and everything after a
/// file's `#[cfg(test)] mod tests`, are left out.
fn production_sources() -> Vec<(PathBuf, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut pending = vec![crates.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap();
            if path.is_dir() {
                if !matches!(name, "tests" | "target" | "node_modules" | "web") {
                    pending.push(path);
                }
            } else if name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let production = text
                    .split("#[cfg(test)]\nmod tests")
                    .next()
                    .unwrap()
                    .to_owned();
                sources.push((path, production));
            }
        }
    }
    assert!(
        sources.len() > 100,
        "the workspace's sources were found: {}",
        sources.len()
    );
    sources
}

/// The patterns `sources` show, each with the file it is in.
fn found(sources: &[(PathBuf, String)], patterns: &[&str]) -> Vec<String> {
    let mut hits = Vec::new();
    for (path, text) in sources {
        for pattern in patterns {
            if text.contains(pattern) {
                hits.push(format!("{}: {pattern}", path.display()));
            }
        }
    }
    hits
}

/// This module is the only code that creates a comment.
#[test]
fn only_the_outbox_creates_github_comments() {
    let sources = production_sources();
    let outbox = Path::new("archon-daemon/src/github/outbox.rs");
    let elsewhere: Vec<_> = sources
        .iter()
        .filter(|(path, _)| !path.ends_with(outbox))
        .cloned()
        .collect();
    assert_eq!(
        elsewhere.len() + 1,
        sources.len(),
        "the outbox itself was found"
    );
    let comment_calls = [
        "/comments",
        "\"comment\"",
        "issue comment",
        "pr comment",
        "issuecomment",
    ];
    let hits = found(&elsewhere, &comment_calls);
    assert!(
        hits.is_empty(),
        "comments created outside the outbox: {hits:#?}"
    );
}

/// No code path closes, reopens, locks or edits an issue.
#[test]
fn nothing_closes_or_edits_an_issue() {
    let sources = production_sources();
    let edits = [
        "state=closed",
        "\"state\": \"closed\"",
        "\"state\":\"closed\"",
        "\"PATCH\"",
        "--method PATCH",
        "-X PATCH",
        "method=PATCH",
        "\"issue\", \"close\"",
        "\"issue\", \"reopen\"",
        "\"issue\", \"lock\"",
        "\"issue\", \"edit\"",
        "issue close",
        "issue reopen",
        "issue lock",
        "issue edit",
        "/lock\"",
    ];
    let hits = found(&sources, &edits);
    assert!(hits.is_empty(), "an issue edit: {hits:#?}");
}

/// A `gh` that stops reading its stdin, or never answers, is stopped at the
/// deadline: the outbox then keeps the row, as the post may have gone through.
#[tokio::test]
async fn a_stalled_gh_is_stopped_at_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    for script in ["exec sleep 30", "cat > /dev/null\nexec sleep 30"] {
        let gh = dir.path().join("gh");
        archon_core::testing::write_script(&gh, &format!("#!/bin/sh\n{script}\n"));
        // Far more than a pipe buffer, so a write blocks until `gh` dies.
        let body = "é".repeat(65_000);
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let result = create_comment(&gh, &reply_intent().target, &body, deadline).await;
        assert!(
            matches!(result, Err(ArchonError::Unavailable(_))),
            "{result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
