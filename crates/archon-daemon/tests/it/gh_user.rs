//! A repository whose `gh` calls and pushes run as a GitHub login the
//! operator named (ARCHITECTURE §8.7), against a fake `gh` and a fake
//! token: the token reaches only `gh`, never a log, a stage agent, the store
//! or an error, and a commit under another identity is still refused.

use crate::common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use archon_core::model::ModelPlan;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{Clearable, RepoPatch, RepoRef, Request};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::StageOutcome;
use archon_daemon::executor::pull_requests;
use common::delivery::{
    FAKE_LOGIN, FAKE_TOKEN, FakeGh, add_origin, origin_of, pushed, record, shipped_by,
};
use common::{Harness, REPO_NAME, REVIEWED, REVIEWED_TOML, git};
use serde_json::json;

const GITHUB_ORIGIN: &str = "https://github.com/o/r.git";

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// The repository runs as `login` from now on.
async fn run_as(h: &Harness, login: &str) {
    let patch = RepoPatch {
        gh_user: Clearable::Set(login.parse().unwrap()),
        ..RepoPatch::default()
    };
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

/// An item on `direct`, dispatched: its agent runs.
async fn started(h: &Harness, title: &str) -> QueueItem {
    let item = h
        .enqueue_in("direct", title, ModelPlan::default())
        .await
        .expect("enqueue");
    h.dispatch().await;
    item
}

/// The running stage completes with `structured`; the dispatcher runs once.
async fn settle_with(h: &Harness, structured: serde_json::Value) {
    let outcome = StageOutcome::Completed {
        summary: "Changed it".into(),
        structured: Some(structured),
    };
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
}

async fn failed(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Failed { error, .. } => error,
        other => panic!("the item fails: {other:?}"),
    }
}

/// Everything logged, at any level, on this thread while it is held.
struct Logged {
    lines: Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logged {
    fn capture() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Sink(Arc::clone(&sink)))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        Self { lines, _guard }
    }

    fn text(&self) -> String {
        String::from_utf8(self.lines.lock().unwrap().clone()).unwrap()
    }
}

/// Every file under `root` holding `secret`, except those in `skip`.
fn holding(root: &Path, secret: &str, skip: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let (path, kind) = (entry.path(), entry.file_type().unwrap());
            if skip.contains(&path) || kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                todo.push(path);
            } else if std::fs::read(&path).is_ok_and(|bytes| {
                bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
            }) {
                found.push(path);
            }
        }
    }
    found
}

/// The acceptance case: once the repository runs as a login, every `gh` call
/// carries that login's token, for github.com only, and the token shows in
/// no log, stage agent, file of the daemon's or worktree, or error.
#[tokio::test]
async fn every_gh_call_runs_as_the_login_and_its_token_shows_nowhere_else() {
    let (h, gh) = delivering().await;
    let first = started(&h, "a change shipped as the daemon").await;
    let stage = h.running().await.remove(0);
    let built = shipped_by(&h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
    assert!(matches!(
        h.state(&first.id).await,
        QueueState::Succeeded { .. }
    ));
    let before = gh.calls().len();

    run_as(&h, FAKE_LOGIN).await;
    git(&h.repo, &["config", "remote.origin.pushurl", GITHUB_ORIGIN]);
    let logged = Logged::capture();
    started(&h, "a change made while the repository runs as a login").await;
    pull_requests::observe(&h.ctx).await.unwrap();
    let echoed = format!("HTTP 401: Bad credentials (token {FAKE_TOKEN})");
    gh.fail_next("pr list", &echoed);
    let unanswered = pull_requests::observe(&h.ctx).await.unwrap();
    let logged = logged.text();

    assert!(unanswered.unanswered > 0, "{unanswered:?}");
    let calls = &gh.calls_as()[before..];
    assert!(calls.iter().any(|(_, call)| call.starts_with("pr list")));
    let lookup = format!("auth token --hostname github.com --user {FAKE_LOGIN}");
    for (account, call) in calls {
        let expected = if *call == lookup {
            "unset unset".to_owned()
        } else {
            format!("{FAKE_TOKEN} github.com")
        };
        assert_eq!(*account, expected, "{call}");
    }
    assert!(!logged.contains(FAKE_TOKEN), "{logged}");
    assert!(logged.contains("[token]"), "gh's words are kept: {logged}");
    let spawned = format!("{:?}", h.backend.requests());
    assert_eq!(h.backend.requests().len(), 2);
    assert!(!spawned.contains(FAKE_TOKEN), "no stage agent is given it");
    assert!(std::env::vars().all(|(_, value)| !value.contains(FAKE_TOKEN)));
    let binary = h.ctx.config.github.binary.clone();
    let fake = [binary.with_file_name("gh-state"), binary];
    let root = h.dir.path().canonicalize().unwrap();
    assert_eq!(holding(&root, FAKE_TOKEN, &fake), Vec::<PathBuf>::new());
}

/// An `origin` that is not github.com's cannot be reached as the login, so
/// nothing is pushed, nothing falls back to the daemon's own account, and
/// the operator is told.
#[tokio::test]
async fn a_login_never_pushes_anywhere_but_github_com() {
    let (h, gh) = delivering().await;
    run_as(&h, FAKE_LOGIN).await;
    let item = started(&h, "a change for another host").await;
    let run = h.run(&item.id).await;
    let stage = h.running().await.remove(0);
    let built = shipped_by(&h, &stage, "Changed it", json!({})).await;
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;

    let error = failed(&h, &item).await;
    assert!(
        error.starts_with("stage ship: BLOCKED: `origin` is"),
        "{error}"
    );
    assert!(error.contains("not a github.com repository"), "{error}");
    assert!(error.contains(FAKE_LOGIN), "{error}");
    assert_eq!(h.notices(NotificationKind::Blocker).await.len(), 1);
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}

/// A commit under an identity the agent set itself is refused before any
/// push, naming the identity the repository expects.
#[tokio::test]
async fn a_commit_under_another_identity_is_refused_naming_the_expected_one() {
    let (h, gh) = delivering().await;
    run_as(&h, FAKE_LOGIN).await;
    let item = started(&h, "a change under the wrong identity").await;
    let run = h.run(&item.id).await;
    std::fs::write(run.workspace.join("change.txt"), "a change\n").unwrap();
    git(&run.workspace, &["add", "change.txt"]);
    let committed = Command::new("git")
        .arg("-C")
        .arg(&run.workspace)
        .args(["commit", "--quiet", "-m", "Change the thing"])
        .env("GIT_AUTHOR_EMAIL", "agent@elsewhere.invalid")
        .env("GIT_COMMITTER_EMAIL", "agent@elsewhere.invalid")
        .status()
        .unwrap();
    assert!(committed.success());
    settle_with(&h, record(&run.workspace)).await;

    let error = failed(&h, &item).await;
    let expected = "not made under this repository's git identity (<test@archon.invalid>)";
    assert!(error.contains(expected), "{error}");
    assert!(
        error.contains("author <agent@elsewhere.invalid>"),
        "{error}"
    );
    assert_eq!(pushed(&origin_of(&h), &run.branch), None);
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}
