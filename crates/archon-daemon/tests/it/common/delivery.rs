//! What archond's own delivery needs of a test: an `origin` to push to, a
//! `gh` that keeps the pull requests it is asked for, and the ship record a
//! stage leaves with its commit.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::aether_ids::AetherAgentId;
use archon_core::run::{StageOutcome, StageRun};
use serde_json::{Value, json};

use super::{Harness, git};

/// The token the fake `gh` keeps for [`FAKE_LOGIN`]; no real account's.
pub const FAKE_TOKEN: &str = "gho_FAKEdevfakeFAKE0123456789";
pub const FAKE_LOGIN: &str = "devfake";

/// A `gh` keeping a file per head branch's pull request and logging calls
/// and the `GH_TOKEN` and `GH_HOST` each had; it knows [`FAKE_LOGIN`]'s token;
/// `fail-<command>` fails it once, `lose-create-reply` loses a create's reply,
/// `checks` and `annotations` are what `pr checks` and the annotations API say.
/// `pr view` shows the `view-*` files; a merge at that head marks it merged.
/// Labels: `labels` lists the repository's, `labels-<n>` and `body-<n>` hold
/// pull request n's, and `refuse-labels` makes every `label create` fail.
const STATEFUL_GH: &str = r#"#!/bin/sh
state="$(dirname "$0")/gh-state"
printf '%s\n' "$*" >> "$state/calls"
printf '%s %s\n' "${GH_TOKEN-unset}" "${GH_HOST-unset}" >> "$state/tokens"
failing="$state/fail-$1-$2"
if [ -f "$failing" ]; then
    cat "$failing" >&2
    rm "$failing"
    exit 1
fi
slot() { printf '%s/pr-%s' "$state" "$(printf '%s' "$1" | tr '/' '_')"; }
names() { python3 -c 'import json,sys; print(json.dumps([{"name": n} for n in open(sys.argv[1]).read().splitlines()]))' "$1"; }
if [ "$1 $2" = "pr edit" ] && [ "$4" != "--base" ]; then
    n="${3##*/}"
    shift 3
    while [ $# -gt 0 ]; do
        case "$1" in
        --body-file) cat > "$state/body-$n" ;;
        --add-label)
            grep -qxF "$2" "$state/labels" 2>/dev/null || { echo "'$2' not found" >&2; exit 1; }
            printf '%s\n' "$2" >> "$state/labels-$n" ;;
        --remove-label)
            grep -vxF "$2" "$state/labels-$n" > "$state/labels-$n.new"
            mv "$state/labels-$n.new" "$state/labels-$n" ;;
        *) exit 2 ;;
        esac
        shift 2
    done
    exit 0
fi
if [ "$1 $2 $4 $5" = "pr view --json labels,body" ]; then
    n="${3##*/}"
    touch "$state/labels-$n" "$state/body-$n"
    python3 -c 'import json,sys; print(json.dumps({"labels": [{"name": l} for l in open(sys.argv[1]).read().splitlines()], "body": open(sys.argv[2]).read()}))' "$state/labels-$n" "$state/body-$n"
    exit 0
fi
if [ "$1 $2 $4 $5" = "pr view --json id,number,url,state,headRefName,headRefOid,baseRefName,body,isDraft,reviewDecision,isCrossRepository,headRepository,headRepositoryOwner" ]; then
    shown="$state/headed-${3##*/}.json"
    [ -f "$shown" ] || { echo "GraphQL: Could not resolve to a PullRequest" >&2; exit 1; }
    now=OPEN
    [ -f "$state/merged" ] && now=MERGED
    [ -f "$state/closed" ] && now=CLOSED
    python3 -c 'import json,sys; pr=json.load(open(sys.argv[1])); pr["state"]=sys.argv[2]; pr["headRefOid"]=open(sys.argv[3]).read() if sys.argv[3] else pr["headRefOid"]; print(json.dumps(pr))' \
        "$shown" "$now" "$([ -f "$state/view-head" ] && printf '%s' "$state/view-head")"
    exit 0
fi
case "$1 $2" in
"label list")
    [ "$3 $4 $5 $6" = "--json name --limit 1000" ] || exit 2
    touch "$state/labels"
    names "$state/labels"
    ;;
"label create")
    [ "$4 $6 $#" = "--color --description 7" ] || exit 2
    [ -f "$state/refuse-labels" ] && { cat "$state/refuse-labels" >&2; exit 1; }
    grep -qxF "$3" "$state/labels" 2>/dev/null && { echo "label with name \"$3\" already exists" >&2; exit 1; }
    printf '%s\n' "$3" >> "$state/labels"
    printf '%s|%s|%s\n' "$3" "$5" "$7" >> "$state/label-specs"
    ;;
"auth token")
    [ "$3 $4 $5 $6" = "--hostname github.com --user @LOGIN@" ] || {
        echo "no oauth token found for github.com account $6" >&2
        exit 1
    }
    echo "@TOKEN@"
    ;;
"pr list")
    if [ "$3 $4 $5 $6 $7" = "--state open --limit 1000 --json" ]; then
        sep=''
        printf '['
        for pr in "$state"/pr-*; do
            [ -f "$pr" ] || continue
            read -r number base head < "$pr"
            printf '%s{"url":"https://github.com/o/r/pull/%s","number":%s,"headRefName":"%s","state":"OPEN","baseRefName":"%s"}' \
                "$sep" "$number" "$number" "$head" "$base"
            sep=','
        done
        printf ']'
        exit 0
    fi
    [ "$3 $5 $7 $8 $9" = "--head --state --limit 20 --json" ] || exit 2
    pr="$(slot "$4")"
    [ -f "$pr" ] || { printf '[]'; exit 0; }
    read -r number base head < "$pr"
    printf '[{"url":"https://github.com/o/r/pull/%s","number":%s,"headRefName":"%s","state":"OPEN","baseRefName":"%s"}]' \
        "$number" "$number" "$head" "$base"
    ;;
"pr create")
    [ "$3 $5 $7 $9 ${10}" = "--base --head --title --body-file -" ] || exit 2
    pr="$(slot "$6")"
    [ -f "$pr" ] && { echo "a pull request for branch \"$6\" already exists" >&2; exit 1; }
    number=$(($(ls "$state" | grep -c '^pr-') + 1))
    printf '%s %s %s\n' "$number" "$4" "$6" > "$pr"
    printf '%s' "$8" > "$state/title"
    cat > "$state/body"
    cp "$state/body" "$state/body-$number"
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
        read -r number base head < "$pr"
        [ "https://github.com/o/r/pull/$number" = "$3" ] && printf '%s %s %s\n' "$number" "$5" "$head" > "$pr"
    done
    ;;
"repo view")
    [ -f "$state/checks-after-repo-view" ] && mv "$state/checks-after-repo-view" "$state/view-checks"
    printf '{"squashMergeAllowed":true,"mergeCommitAllowed":true,"rebaseMergeAllowed":false}'
    ;;
"pr view")
    [ "$4 $5" = "--json state,isDraft,headRefOid,baseRefName,mergeable,statusCheckRollup,mergedBy,autoMergeRequest" ] || exit 2
    [ -f "$state/view-head" ] || { echo "no pull request at $3" >&2; exit 1; }
    shown=OPEN
    [ -f "$state/merged" ] && shown=MERGED
    [ -f "$state/closed" ] && shown=CLOSED
    actor=null
    [ -f "$state/actor" ] && actor=$(cat "$state/actor")
    auto=null
    [ -f "$state/queued" ] && auto='{}'
    printf '{"state":"%s","isDraft":false,"headRefOid":"%s","baseRefName":"main","mergeable":"%s","statusCheckRollup":%s,"mergedBy":%s,"autoMergeRequest":%s}' \
        "$shown" "$(cat "$state/view-head")" "$(cat "$state/view-mergeable")" "$(cat "$state/view-checks")" "$actor" "$auto"
    ;;
"pr merge")
    if [ "$4" = "--auto" ]; then
        [ "$4 $5" = "--auto --squash" ] || exit 2
        exit 0
    fi
    [ "$# $4 $6" = "6 --match-head-commit --squash" ] || exit 2
    [ "$5" = "$(cat "$state/view-head")" ] || { echo "Head branch was modified" >&2; exit 1; }
    [ -f "$state/queue-next" ] && { touch "$state/queued"; exit 0; }
    touch "$state/merged"
    if [ -f "$state/lose-merge-reply" ]; then
        rm "$state/lose-merge-reply"
        echo "HTTP 502: Bad Gateway" > "$state/fail-pr-view"
    fi
    ;;
"pr checks")
    [ "$4 $5" = "--json name,bucket,startedAt,link" ] || exit 2
    [ -f "$state/checks" ] || { echo "no checks reported on the 'b' branch" >&2; exit 1; }
    cat "$state/checks"
    ;;
"api --hostname")
    case "$4" in repos/*/check-runs/*/annotations) ;; *) exit 2 ;; esac
    cat "$state/annotations" 2>/dev/null || printf '[]'
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
        let script = STATEFUL_GH
            .replace("@LOGIN@", FAKE_LOGIN)
            .replace("@TOKEN@", FAKE_TOKEN);
        std::fs::write(binary, script).expect("gh script");
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).expect("mode");
        Self { state }
    }

    /// Every call so far, as its arguments on one line.
    pub fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.state.join("calls")).unwrap_or_default();
        calls.lines().map(str::to_owned).collect()
    }

    /// The `GH_TOKEN` and `GH_HOST` of every call so far, `unset` if not
    /// set, beside its arguments.
    pub fn calls_as(&self) -> Vec<(String, String)> {
        let tokens = std::fs::read_to_string(self.state.join("tokens")).unwrap_or_default();
        let tokens: Vec<String> = tokens.lines().map(str::to_owned).collect();
        let calls = self.calls();
        assert_eq!(tokens.len(), calls.len(), "one line of each per call");
        tokens.into_iter().zip(calls).collect()
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
        std::fs::write(self.state.join(file), format!("7 {base} {branch}\n"))
            .expect("pull request");
    }

    /// What `gh pr view` shows of every pull request from now on: its head,
    /// `mergeable` (e.g. `MERGEABLE`) and its `statusCheckRollup`.
    pub fn show(&self, head: &str, mergeable: &str, checks: Value) {
        let write = |name: &str, text: &str| {
            std::fs::write(self.state.join(name), text).expect(name);
        };
        write("view-head", head);
        write("view-mergeable", mergeable);
        write("view-checks", &checks.to_string());
    }

    /// What `gh pr view <number> --json <head fields>` shows of pull request
    /// `number`; its state and head follow what [`Self::show`] last set.
    pub fn headed(&self, number: u64, pr: &Value) {
        let file = self.state.join(format!("headed-{number}.json"));
        std::fs::write(file, pr.to_string()).expect("pull request");
    }

    /// Whether a `gh pr merge` merged the pull request shown.
    pub fn merged(&self) -> bool {
        self.state.join("merged").exists()
    }

    /// The next merge goes through, but GitHub cannot be read just after.
    pub fn lose_next_merge_reply(&self) {
        std::fs::write(self.state.join("lose-merge-reply"), "").expect("marker");
    }

    pub fn queue_next_merge(&self) {
        std::fs::write(self.state.join("queue-next"), "").unwrap();
    }

    pub fn finish_queued(&self, actor: Option<&str>) {
        let marker = if actor.is_some() { "merged" } else { "closed" };
        std::fs::write(self.state.join(marker), "").unwrap();
        if let Some(actor) = actor {
            std::fs::write(
                self.state.join("actor"),
                json!({"login": actor}).to_string(),
            )
            .unwrap();
        }
    }

    /// `pr view` shows `checks` as its `statusCheckRollup` once the next
    /// `repo view` (the merge method's read) has run.
    pub fn checks_after_repo_view(&self, checks: &Value) {
        let file = self.state.join("checks-after-repo-view");
        std::fs::write(file, checks.to_string()).expect("checks");
    }

    /// What `gh pr checks --json` prints from now on.
    pub fn set_checks(&self, checks: &Value) {
        std::fs::write(self.state.join("checks"), checks.to_string()).expect("checks");
    }

    /// What the annotations of any check run are from now on.
    pub fn set_annotations(&self, annotations: &Value) {
        let raw = annotations.to_string();
        std::fs::write(self.state.join("annotations"), raw).expect("annotations");
    }

    /// Every `label create` fails from now on, saying `stderr`.
    pub fn refuse_labels(&self, stderr: &str) {
        std::fs::write(self.state.join("refuse-labels"), stderr).expect("refusal");
    }

    /// The repository has `labels` already.
    pub fn add_labels(&self, labels: &[&str]) {
        let text: String = labels.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(self.state.join("labels"), text).expect("labels");
    }

    /// Pull request `number` carries `labels` and `body` already.
    pub fn label_pr(&self, number: u64, labels: &[&str], body: &str) {
        let text: String = labels.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(self.state.join(format!("labels-{number}")), text).expect("labels");
        std::fs::write(self.state.join(format!("body-{number}")), body).expect("body");
    }

    /// The labels and the current body of pull request `number`.
    pub fn pr_labels(&self, number: u64) -> (Vec<String>, String) {
        let read =
            |name: String| std::fs::read_to_string(self.state.join(name)).unwrap_or_default();
        let labels = read(format!("labels-{number}"));
        (
            labels.lines().map(str::to_owned).collect(),
            read(format!("body-{number}")),
        )
    }

    /// Each label the repository created: its name, colour and description.
    pub fn created_labels(&self) -> Vec<String> {
        let specs = std::fs::read_to_string(self.state.join("label-specs")).unwrap_or_default();
        specs.lines().map(str::to_owned).collect()
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

/// A planted program that leaves `marker` when anything runs it.
fn planted(dir: &Path, marker: &Path) -> PathBuf {
    let script = dir.join("planted.sh");
    let body = format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", marker.display());
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Hooks, a filter, a merge driver and fsmonitor in the repository's config,
/// and hooks in `origin`: what an agent with the worktree could plant.
pub fn plant_everywhere(repo: &Path, origin: &Path, marker: &Path) {
    let program = planted(repo.parent().unwrap(), marker);
    let program = program.to_str().unwrap();
    let hooks = repo.parent().unwrap().join("planted-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    for hook in [
        "pre-push",
        "post-checkout",
        "post-merge",
        "reference-transaction",
    ] {
        std::fs::copy(program, hooks.join(hook)).unwrap();
    }
    for hook in ["pre-receive", "update", "post-receive"] {
        std::fs::copy(program, origin.join("hooks").join(hook)).unwrap();
    }
    for (key, value) in [
        ("core.hooksPath", hooks.to_str().unwrap()),
        ("core.fsmonitor", program),
        ("filter.planted.clean", program),
        ("filter.planted.smudge", program),
        ("filter.planted.required", "true"),
        ("merge.planted.driver", program),
        ("core.sshCommand", program),
        ("credential.helper", program),
    ] {
        git(repo, &["config", key, value]);
    }
}
