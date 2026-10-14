//! What archond's own delivery needs of a test: an `origin` to push to, a
//! `gh` that keeps the pull requests it is asked for, and the ship record a
//! stage leaves with its commit.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::aether_ids::AetherAgentId;
use archon_core::run::{StageOutcome, StageRun};
use serde_json::{Value, json};

use super::{Harness, git};

/// A `gh` keeping a file per head branch's pull request and logging calls;
/// `fail-<command>` fails it once, `lose-create-reply` loses a create's reply.
const STATEFUL_GH: &str = r#"#!/bin/sh
state="$(dirname "$0")/gh-state"
printf '%s\n' "$*" >> "$state/calls"
failing="$state/fail-$1-$2"
if [ -f "$failing" ]; then
    cat "$failing" >&2
    rm "$failing"
    exit 1
fi
slot() { printf '%s/pr-%s' "$state" "$(printf '%s' "$1" | tr '/' '_')"; }
case "$1 $2" in
"pr list")
    [ "$3 $5 $6 $7 $8 $9" = "--head --state open --limit 2 --json" ] || exit 2
    pr="$(slot "$4")"
    [ -f "$pr" ] || { printf '[]'; exit 0; }
    read -r number base < "$pr"
    printf '[{"url":"https://github.com/o/r/pull/%s","number":%s,"headRefName":"%s","state":"OPEN","baseRefName":"%s"}]' \
        "$number" "$number" "$4" "$base"
    ;;
"pr create")
    [ "$3 $5 $7 $9 ${10}" = "--base --head --title --body-file -" ] || exit 2
    pr="$(slot "$6")"
    [ -f "$pr" ] && { echo "a pull request for branch \"$6\" already exists" >&2; exit 1; }
    number=$(($(ls "$state" | grep -c '^pr-') + 1))
    printf '%s %s\n' "$number" "$4" > "$pr"
    printf '%s' "$8" > "$state/title"
    cat > "$state/body"
    if [ -f "$state/lose-create-reply" ]; then
        rm "$state/lose-create-reply"
        echo "HTTP 502: Bad Gateway" | tee "$state/fail-pr-list" >&2
        exit 1
    fi
    printf 'https://github.com/o/r/pull/%s\n' "$number"
    ;;
"pr edit")
    [ "$4" = "--base" ] || exit 2
    for pr in "$state"/pr-*; do
        read -r number base < "$pr"
        [ "https://github.com/o/r/pull/$number" = "$3" ] && printf '%s %s\n' "$number" "$5" > "$pr"
    done
    ;;
"repo view")
    printf '{"squashMergeAllowed":true,"mergeCommitAllowed":true,"rebaseMergeAllowed":false}'
    ;;
"pr merge")
    [ "$4 $5" = "--auto --squash" ] || exit 2
    ;;
*)
    exit 2
    ;;
esac
"#;

/// The stateful `gh` of one harness.
pub struct FakeGh {
    state: PathBuf,
}

impl FakeGh {
    /// Put the stateful `gh` where the harness's daemon looks for `gh`.
    pub fn install(h: &Harness) -> Self {
        let binary = &h.ctx.config.github.binary;
        let state = binary.parent().expect("beside the binary").join("gh-state");
        std::fs::create_dir_all(&state).expect("gh state");
        std::fs::write(binary, STATEFUL_GH).expect("gh script");
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).expect("mode");
        Self { state }
    }

    /// Every call so far, as its arguments on one line.
    pub fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.state.join("calls")).unwrap_or_default();
        calls.lines().map(str::to_owned).collect()
    }

    /// How many calls so far began with `command`, e.g. `pr create`.
    pub fn count(&self, command: &str) -> usize {
        let calls = self.calls();
        calls.iter().filter(|c| c.starts_with(command)).count()
    }

    /// Make the next `command` (e.g. `pr create`) fail, saying `stderr`.
    pub fn fail_next(&self, command: &str, stderr: &str) {
        let file = format!("fail-{}", command.replace(' ', "-"));
        std::fs::write(self.state.join(file), stderr).expect("failure");
    }

    /// GitHub opens the next pull request, but its answer and the one to the
    /// lookup that follows are lost.
    pub fn lose_next_create_reply(&self) {
        std::fs::write(self.state.join("lose-create-reply"), "").expect("marker");
    }

    /// A pull request open already for `branch`, onto `base`.
    pub fn open(&self, branch: &str, base: &str) {
        let file = format!("pr-{}", branch.replace('/', "_"));
        std::fs::write(self.state.join(file), format!("7 {base}\n")).expect("pull request");
    }

    /// The title and body of the pull request last created.
    pub fn created(&self) -> (String, String) {
        let read = |name: &str| std::fs::read_to_string(self.state.join(name)).expect(name);
        (read("title"), read("body"))
    }
}

/// Where [`add_origin`] puts the harness repository's `origin`.
pub fn origin_of(h: &Harness) -> PathBuf {
    h.dir
        .path()
        .canonicalize()
        .expect("root")
        .join("origin.git")
}

/// Give the harness's repository an `origin` holding its `main`; returns the
/// bare repository's path.
pub fn add_origin(h: &Harness) -> PathBuf {
    let origin = origin_of(h);
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(
        &origin,
        &["init", "--quiet", "--bare", "--initial-branch=main"],
    );
    git(
        &h.repo,
        &["remote", "add", "origin", origin.to_str().expect("utf-8")],
    );
    git(&h.repo, &["push", "--quiet", "origin", "main"]);
    origin
}

/// The commit `origin` holds for `branch`, if it has the branch.
pub fn pushed(origin: &Path, branch: &str) -> Option<String> {
    let name = format!("refs/heads/{branch}");
    let listed = git(origin, &["for-each-ref", "--format=%(objectname)", &name]);
    (!listed.is_empty()).then_some(listed)
}

/// Commit one more change in `workspace` under the repository's identity, as
/// a stage agent does, and return the ship record that names it.
pub fn change(workspace: &Path) -> Value {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace.join("change.txt"))
        .expect("change file");
    writeln!(file, "a change").expect("write");
    git(workspace, &["add", "change.txt"]);
    git(workspace, &["commit", "--quiet", "-m", "Change the thing"]);
    record(workspace)
}

/// The ship record for `workspace` as it stands.
pub fn record(workspace: &Path) -> Value {
    json!({
        "commit": git(workspace, &["rev-parse", "HEAD"]),
        "tree": git(workspace, &["rev-parse", "HEAD^{tree}"]),
        "checks": ["cargo test"],
        "pr": {"title": "Change the thing", "body": "Why, and how it was verified."},
    })
}

/// A lead's pass naming the `reviewers` it rests on.
pub fn pass_by(reviewers: &[AetherAgentId]) -> Value {
    let named: Vec<&str> = reviewers.iter().map(AetherAgentId::as_str).collect();
    json!({"review": {"verdict": "pass", "reviewers": named}})
}

/// A reviewer's own result: `verdict` on the commit `built` records.
pub fn verdict_on(built: &StageOutcome, verdict: &str) -> Value {
    let StageOutcome::Completed {
        structured: Some(record),
        ..
    } = built
    else {
        panic!("a completed change: {built:?}")
    };
    json!({"verdict": verdict, "head": record["commit"], "findings": []})
}

/// `stage` completing with a new change, its ship record and a pass naming
/// the `reviewers` it delegated, each of which passed that commit.
pub async fn reviewed_by(
    h: &Harness,
    stage: &StageRun,
    summary: &str,
    reviewers: &[AetherAgentId],
) -> StageOutcome {
    let built = shipped_by(h, stage, summary, pass_by(reviewers)).await;
    for reviewer in reviewers {
        h.backend
            .complete_delegated(reviewer, verdict_on(&built, "pass"));
    }
    built
}

/// `stage` completing with a new change committed in its run's worktree and
/// the ship record for it, plus `extra` (e.g. its `review`).
pub async fn shipped_by(
    h: &Harness,
    stage: &StageRun,
    summary: &str,
    extra: Value,
) -> StageOutcome {
    let workspace = h
        .ctx
        .store
        .lock()
        .await
        .run(&stage.run)
        .expect("run")
        .workspace;
    let mut structured = change(&workspace);
    let more = extra.as_object().cloned().unwrap_or_default();
    structured.as_object_mut().expect("a record").extend(more);
    StageOutcome::Completed {
        summary: summary.into(),
        structured: Some(structured),
    }
}
