//! Work queued `onto` an existing pull request: enqueue resolves the target
//! with `gh` and freezes it, refuses one Archon could not push to, the
//! item's branch starts from the frozen head, and delivery fast-forwards the
//! pull request's branch. A bare `origin` and a fake `gh`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::model::{Effort, ModelPlan};
use archon_core::protocol::{EnqueueRequest, ItemPatch, Reply, RepoRef, Request};
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::queue::{QueueItem, QueueState};
use common::delivery::{add_origin, origin_of, pass_by, pushed, shipped_by, verdict_on};
use common::{Harness, MODEL, PIPELINE, REPO_NAME, REVIEW_MODEL, choice, git, plan};
use serde_json::{Value, json};

/// `pr view <number|url>` shows `pr-<number>.json`, its head the commit
/// `origin` holds for `feature` once `origin` names that repository;
/// `pr list --head <branch>` lists `branch-<branch>.json`, else none;
/// `repo view <owner/name>` grants the permission in
/// `permission-<owner>_<name>`, else WRITE.
const GH: &str = r#"#!/bin/sh
state="$(dirname "$0")/gh-onto"
printf '%s\n' "$*" >> "$state/calls"
case "$1 $2" in
"pr view")
    [ "$4" = "--json" ] || exit 2
    shown="$state/pr-${3##*/}.json"
    [ -f "$shown" ] || { echo "GraphQL: Could not resolve to a PullRequest" >&2; exit 1; }
    if [ -f "$state/origin" ]; then
        live=$(git --git-dir="$(cat "$state/origin")" rev-parse --verify -q refs/heads/feature)
        sed "s/\"headRefOid\":\"[0-9a-f]*\"/\"headRefOid\":\"$live\"/" "$shown"
    else
        cat "$shown"
    fi
    ;;
"pr list")
    if [ "$3 $4 $5 $6 $7" = "--state open --limit 1000 --json" ]; then printf '[]'; exit 0; fi
    [ "$3 $5 $6 $7 $8 $9" = "--head --state open --limit 20 --json" ] || exit 2
    listed="$state/branch-$(printf '%s' "$4" | tr '/' '_').json"
    if [ -f "$listed" ]; then cat "$listed"; else printf '[]'; fi
    ;;
"repo view")
    [ "$4 $5" = "--json viewerPermission" ] || exit 2
    granted="$state/permission-$(printf '%s' "$3" | tr '/' '_')"
    if [ -f "$granted" ]; then permission="$(cat "$granted")"; else permission=WRITE; fi
    printf '{"viewerPermission":"%s"}' "$permission"
    ;;
"pr edit") ;;
*) exit 2 ;;
esac
"#;

struct Gh {
    state: PathBuf,
}

impl Gh {
    fn install(h: &Harness) -> Self {
        let binary = &h.ctx.config.github.binary;
        let state = binary.parent().expect("beside the binary").join("gh-onto");
        std::fs::create_dir_all(&state).expect("gh state");
        std::fs::write(binary, GH).expect("gh script");
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).expect("mode");
        Self { state }
    }

    /// GitHub shows pull request `number` as `pr`'s fields over an open one
    /// of `feature` into `main` at `head`.
    fn show(&self, number: u64, head: &str, pr: Value) -> Value {
        let mut shown = json!({
            "id": format!("PR_node{number}"),
            "number": number,
            "url": format!("https://github.com/o/r/pull/{number}"),
            "state": "OPEN",
            "headRefName": "feature",
            "headRefOid": head,
            "baseRefName": "main",
            "isCrossRepository": false,
            "headRepository": {"name": "r"},
            "headRepositoryOwner": {"login": "o"},
        });
        let fields = shown.as_object_mut().expect("an object");
        fields.extend(pr.as_object().expect("an object").clone());
        let file = self.state.join(format!("pr-{number}.json"));
        std::fs::write(file, shown.to_string()).expect("pull request");
        shown
    }

    /// GitHub shows the head `origin` holds for `feature` from now on.
    fn follow_origin(&self, origin: &Path) {
        std::fs::write(self.state.join("origin"), origin.to_str().unwrap()).expect("origin");
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.state.join("calls")).unwrap_or_default()
    }

    /// GitHub lists `prs` as the open pull requests of `branch`.
    fn list(&self, branch: &str, prs: &[Value]) {
        let file = format!("branch-{}.json", branch.replace('/', "_"));
        std::fs::write(self.state.join(file), json!(prs).to_string()).expect("listing");
    }
}

/// `origin` gains the branch `feature`, one commit past `main`, pushed from
/// a clone of its own so the harness repository lacks it; returns its head.
fn feature_on_origin(h: &Harness) -> String {
    let origin = origin_of(h);
    let clone = h.dir.path().join("upstream");
    git(
        h.dir.path(),
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    std::fs::write(clone.join("feature.txt"), "feature\n").unwrap();
    git(&clone, &["add", "feature.txt"]);
    git(&clone, &["commit", "--quiet", "-m", "Feature"]);
    git(&clone, &["push", "--quiet", "origin", "HEAD:feature"]);
    git(&clone, &["rev-parse", "HEAD"])
}

/// Someone else pushes one more commit onto `feature` on `origin`; returns it.
fn move_feature(h: &Harness) -> String {
    let clone = h.dir.path().join("upstream");
    std::fs::write(clone.join("feature.txt"), "moved on\n").unwrap();
    git(
        &clone,
        &["commit", "--quiet", "-am", "Someone else's change"],
    );
    git(&clone, &["push", "--quiet", "origin", "HEAD:feature"]);
    git(&clone, &["rev-parse", "HEAD"])
}

async fn setup() -> (Harness, Gh, String) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = Gh::install(&h);
    let head = feature_on_origin(&h);
    (h, gh, head)
}

fn request(title: &str, onto: Option<OntoTarget>, depends_on: &[&QueueItem]) -> EnqueueRequest {
    EnqueueRequest {
        onto,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some(PIPELINE.into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: depends_on.iter().map(|item| item.id.clone()).collect(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    }
}

async fn enqueue_onto(
    h: &Harness,
    title: &str,
    onto: OntoTarget,
) -> Result<QueueItem, ArchonError> {
    match h
        .cli(Request::Enqueue(request(title, Some(onto), &[])))
        .await?
    {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

fn frozen(target: OntoTarget, number: u64, head: &str) -> Box<OntoPullRequest> {
    let url = format!("https://github.com/o/r/pull/{number}");
    Box::new(OntoPullRequest {
        target,
        pr: PullRequestRef::from_url(&url).unwrap(),
        node_id: Some(format!("PR_node{number}")),
        head_repository: "o/r".into(),
        head_ref: "feature".into(),
        base_ref: "main".into(),
        head_sha: head.into(),
    })
}

async fn item_count(h: &Harness) -> usize {
    let query = Default::default();
    h.ctx.store.lock().await.items(&query).unwrap().len()
}

fn head_of(workspace: &Path) -> String {
    git(workspace, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn onto_a_number_freezes_its_identity_head_and_base_and_get_item_shows_it() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));

    let item = enqueue_onto(&h, "onto 100", OntoTarget::Pr(100))
        .await
        .unwrap();

    let expected = frozen(OntoTarget::Pr(100), 100, &head);
    assert_eq!(item.onto.as_ref(), Some(&expected));
    assert_eq!(
        h.item(&item.id).await.onto,
        Some(expected.clone()),
        "stored"
    );
    let detail = match h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    {
        Reply::ItemDetail(detail) => detail,
        other => panic!("get_item replied {other:?}"),
    };
    assert_eq!(detail.item.onto, Some(expected));
}

#[tokio::test]
async fn a_closed_merged_fork_or_unpushable_target_is_refused_at_enqueue_with_the_reason() {
    let (h, gh, head) = setup().await;
    gh.show(101, &head, json!({"state": "CLOSED"}));
    gh.show(102, &head, json!({"state": "MERGED"}));
    gh.show(
        103,
        &head,
        json!({"isCrossRepository": true, "headRepositoryOwner": {"login": "fork"}}),
    );
    gh.show(104, &head, json!({"headRepository": null}));
    gh.show(106, &head, json!({"headRepository": {"name": "ro"}}));
    std::fs::write(gh.state.join("permission-o_ro"), "READ").unwrap();

    for (number, reason) in [
        (101, "it is closed"),
        (102, "it is merged"),
        (103, "its head `fork/r` is a fork"),
        (104, "its head repository is gone"),
        (105, "Could not resolve to a PullRequest"),
        (106, "no write access to `o/ro`"),
    ] {
        let refused = enqueue_onto(&h, "refused", OntoTarget::Pr(number)).await;
        let error = refused.expect_err("refused").to_string();
        assert!(error.contains(reason), "#{number}: {error}");
    }
    let zero = enqueue_onto(&h, "refused", OntoTarget::Pr(0)).await;
    assert!(zero.is_err(), "#0 names no pull request");
    assert_eq!(item_count(&h).await, 0, "nothing was queued");
}

#[tokio::test]
async fn a_replay_returns_the_frozen_item_after_its_pull_request_merged() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    let mut keyed = request("onto 100", Some(OntoTarget::Pr(100)), &[]);
    keyed.idempotency_key = Some("onto-key".into());
    let Reply::Item(first) = h.cli(Request::Enqueue(keyed.clone())).await.unwrap() else {
        panic!("enqueue replied no item");
    };
    gh.show(100, "f".repeat(40).as_str(), json!({"state": "MERGED"}));

    let Reply::Item(replayed) = h.cli(Request::Enqueue(keyed.clone())).await.unwrap() else {
        panic!("replay replied no item");
    };
    assert_eq!(replayed.id, first.id, "the same item");
    assert_eq!(replayed.onto, Some(frozen(OntoTarget::Pr(100), 100, &head)));
    keyed.onto = Some(OntoTarget::Pr(101));
    let other = h.cli(Request::Enqueue(keyed)).await;
    assert!(other.is_err(), "another target is no replay");
    assert_eq!(item_count(&h).await, 1);
}

#[tokio::test]
async fn onto_is_refused_for_a_pipeline_without_a_worktree() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    let mut ask = request("ask onto 100", Some(OntoTarget::Pr(100)), &[]);
    ask.pipeline = Some("ask".into());
    let mut stages = std::collections::BTreeMap::new();
    let answer = common::choice(common::MODEL, archon_core::model::Effort::Medium);
    stages.insert("answer".to_owned(), vec![answer]);
    ask.model_plan = archon_core::model::ModelPlan(stages).into();

    let error = h.cli(Request::Enqueue(ask)).await.expect_err("refused");
    assert!(
        error.to_string().contains("does not work in a worktree"),
        "{error}"
    );
    assert_eq!(item_count(&h).await, 0, "nothing was queued");
}

#[tokio::test]
async fn onto_with_dependencies_is_refused_before_github_is_asked() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    let first = h.enqueue("first").await;

    let asked = request("second", Some(OntoTarget::Pr(100)), &[&first]);
    let refused = h.cli(Request::Enqueue(asked)).await.expect_err("refused");

    assert!(
        refused
            .to_string()
            .contains("cannot also stack on dependencies"),
        "{refused}"
    );
    let calls = std::fs::read_to_string(gh.state.join("calls")).unwrap_or_default();
    assert!(!calls.contains("pr view"), "{calls}");
    assert_eq!(item_count(&h).await, 1);

    let onto = enqueue_onto(&h, "onto", OntoTarget::Pr(100)).await.unwrap();
    let patch = ItemPatch {
        add_depends_on: vec![first.id.clone()],
        ..ItemPatch::default()
    };
    let update = Request::UpdateItem {
        item: onto.id.clone(),
        patch,
    };
    let refused = h.cli(update).await.expect_err("refused");
    assert!(
        refused.to_string().contains("cannot also stack"),
        "{refused}"
    );
    assert!(h.item(&onto.id).await.dependencies.is_empty());
}

#[tokio::test]
async fn onto_an_item_resolves_to_the_pull_request_it_delivered_to() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    let undelivered = h.enqueue("not yet delivered").await;
    let refused = enqueue_onto(&h, "too early", OntoTarget::Item(undelivered.id.clone())).await;
    let error = refused.expect_err("no pull request yet").to_string();
    assert!(
        error.contains("has delivered no pull request yet"),
        "{error}"
    );

    h.dispatch().await;
    let run = h.run(&undelivered.id).await;
    let pr = PullRequestRef::from_url("https://github.com/o/r/pull/100").unwrap();
    let mut store = h.ctx.store.lock().await;
    store.record_run_pull_request(&run.id, &pr, None).unwrap();
    drop(store);

    let target = OntoTarget::Item(undelivered.id.clone());
    let item = enqueue_onto(&h, "onto the item", target.clone())
        .await
        .unwrap();
    assert_eq!(item.onto, Some(frozen(target, 100, &head)));

    let again = OntoTarget::Item(item.id.clone());
    let stacked = enqueue_onto(&h, "onto the onto item", again.clone())
        .await
        .unwrap();
    assert_eq!(
        stacked.onto,
        Some(frozen(again, 100, &head)),
        "its frozen target"
    );
}

#[tokio::test]
async fn onto_a_branch_needs_exactly_one_open_pull_request_of_it() {
    let (h, gh, head) = setup().await;
    let shown = gh.show(100, &head, json!({}));
    let other = gh.show(9, &head, json!({}));

    let none = enqueue_onto(&h, "none", OntoTarget::Branch("feature".into())).await;
    assert!(
        none.expect_err("no pull request")
            .to_string()
            .contains("GitHub lists 0")
    );
    gh.list("feature", &[shown.clone(), other]);
    let two = enqueue_onto(&h, "two", OntoTarget::Branch("feature".into())).await;
    assert!(
        two.expect_err("two pull requests")
            .to_string()
            .contains("GitHub lists 2")
    );
    let odd = enqueue_onto(&h, "odd", OntoTarget::Branch("--force".into())).await;
    assert!(
        odd.expect_err("no branch")
            .to_string()
            .contains("no plain branch name")
    );

    gh.list("feature", &[shown]);
    let target = OntoTarget::Branch("feature".into());
    let item = enqueue_onto(&h, "one", target.clone()).await.unwrap();
    assert_eq!(item.onto, Some(frozen(target, 100, &head)));
}

#[tokio::test]
async fn an_onto_items_worktree_starts_from_the_frozen_head_and_its_agents_are_told() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    let item = enqueue_onto(&h, "onto 100", OntoTarget::Pr(100))
        .await
        .unwrap();
    // The pull request moves on after enqueue; the item keeps what was frozen.
    gh.show(100, &"f".repeat(40), json!({}));
    let known = std::process::Command::new("git")
        .arg("-C")
        .arg(&h.repo)
        .args(["cat-file", "-e", &format!("{head}^{{commit}}")])
        .status()
        .unwrap();
    assert!(!known.success(), "the harness repository lacks the head");

    h.dispatch().await;

    let run = h.run(&item.id).await;
    assert_eq!(head_of(&run.workspace), head, "forked from the frozen head");
    assert_ne!(run.branch, "feature", "a branch of its own");
    let implement = h.backend.requests().remove(0);
    let told = &implement.instructions;
    assert!(told.contains("\n## Existing pull request\n"), "{told}");
    assert!(told.contains("https://github.com/o/r/pull/100"), "{told}");
    assert!(told.contains(&head), "{told}");
}

/// An item onto #100 through `direct`, dispatched, whose one stage commits a
/// change; returns the item and that change's commit once delivery ran.
async fn delivered_onto(h: &Harness, before_settle: impl FnOnce()) -> (QueueItem, String) {
    let mut asked = request("onto 100", Some(OntoTarget::Pr(100)), &[]);
    asked.pipeline = Some("direct".into());
    asked.model_plan = archon_core::model::ModelPlan::default().into();
    let Reply::Item(item) = h.cli(Request::Enqueue(asked)).await.unwrap() else {
        panic!("enqueue replied no item");
    };
    h.dispatch().await;
    before_settle();
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(h, &stage, "Changed it; cargo test passes", json!({})).await;
    let commit = head_of(&h.run(&item.id).await.workspace);
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    (item, commit)
}

#[tokio::test]
async fn an_onto_item_fast_forwards_the_pull_request_and_leaves_its_text_alone() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    let origin = origin_of(&h);
    gh.follow_origin(&origin);

    let (item, commit) = delivered_onto(&h, || {}).await;

    let QueueState::Succeeded { summary, .. } = h.state(&item.id).await else {
        panic!("the item ships: {:?}", h.state(&item.id).await);
    };
    let url = "https://github.com/o/r/pull/100";
    let line = format!("SHIPPED: updated #100 to {commit} ({url})");
    assert!(summary.ends_with(&line), "{summary}");
    assert_eq!(pushed(&origin, "feature"), Some(commit.clone()));
    let ancestry = [
        "merge-base",
        "--is-ancestor",
        head.as_str(),
        commit.as_str(),
    ];
    git(&origin, &ancestry);
    let run = h.run(&item.id).await;
    assert_eq!(pushed(&origin, &run.branch), None, "no branch of its own");
    let calls = gh.calls();
    for untouched in ["pr create", "pr edit", "pr merge", "pr ready"] {
        assert!(!calls.contains(untouched), "{untouched}: {calls}");
    }
    let stages = h.stages(&item.id).await;
    assert_eq!(archon_core::run::pr_url(stages.iter()), Some(url));
}

/// An `adaptive` item onto #100 whose lead is running, and the frozen head.
async fn adaptive_onto(h: &Harness, head: &str) -> (QueueItem, archon_core::run::StageRun) {
    let mut asked = request("reviewed onto 100", Some(OntoTarget::Pr(100)), &[]);
    asked.pipeline = Some("adaptive".into());
    let build = vec![choice(MODEL, Effort::Medium)];
    asked.model_plan = ModelPlan([("build".to_owned(), build)].into()).into();
    let Reply::Item(item) = h.cli(Request::Enqueue(asked)).await.unwrap() else {
        panic!("enqueue replied no item");
    };
    h.dispatch().await;
    let lead = h.running().await.remove(0);
    let workspace = h.run(&item.id).await.workspace;
    assert_eq!(
        head_of(&workspace),
        head,
        "the branch starts at the frozen head"
    );
    (item, lead)
}

/// The review gate reads an onto item's change from the frozen head: a
/// reviewer whose diff starts there read all of it, not part of the pull request's.
#[tokio::test]
async fn a_reviewer_reading_from_the_frozen_head_read_the_whole_onto_change() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    gh.follow_origin(&origin_of(&h));
    let (item, lead) = adaptive_onto(&h, &head).await;
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let built = shipped_by(&h, &lead, "Built", pass_by(std::slice::from_ref(&reviewer))).await;
    let mut verdict = verdict_on(&built, "pass");
    verdict["base"] = head.clone().into();
    h.backend.complete_delegated(&reviewer, verdict);
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;

    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
}

/// A reviewer who read only from a commit after the frozen head still needs an anchor.
#[tokio::test]
async fn a_reviewer_reading_from_after_the_frozen_head_still_needs_an_anchor() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    gh.follow_origin(&origin_of(&h));
    let (item, lead) = adaptive_onto(&h, &head).await;
    let reviewer = h
        .backend
        .delegate(lead.aether_agent.as_ref().unwrap(), REVIEW_MODEL);
    let first = shipped_by(&h, &lead, "Built", json!({})).await;
    let first_head = verdict_on(&first, "pass")["head"].clone();
    let built = shipped_by(
        &h,
        &lead,
        "Built, then fixed",
        pass_by(std::slice::from_ref(&reviewer)),
    )
    .await;
    let mut verdict = verdict_on(&built, "pass");
    verdict["base"] = first_head;
    h.backend.complete_delegated(&reviewer, verdict);
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the review is refused: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("read only from"), "{error}");
}

#[tokio::test]
async fn a_head_that_moved_since_enqueue_fails_delivery_without_a_force_push() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    gh.follow_origin(&origin_of(&h));
    let mut moved = String::new();

    let (item, commit) = delivered_onto(&h, || moved = move_feature(&h)).await;

    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("the item fails: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains(&head) && error.contains(&moved), "{error}");
    assert!(error.contains("never force-pushes"), "{error}");
    assert_eq!(pushed(&origin_of(&h), "feature"), Some(moved), "kept");
    assert_ne!(commit, head, "the item had a change of its own");
    assert!(!gh.calls().contains("pr edit"));
}

/// A delivered onto item has no branch of its own on `origin`: a child is
/// not stacked on it and it is not refreshed, both told to queue `onto` it.
#[tokio::test]
async fn a_delivered_onto_item_is_neither_stacked_on_nor_refreshed() {
    let (h, gh, head) = setup().await;
    gh.show(100, &head, json!({}));
    gh.follow_origin(&origin_of(&h));
    let (parent, _) = delivered_onto(&h, || {}).await;
    assert!(matches!(
        h.state(&parent.id).await,
        QueueState::Succeeded { .. }
    ));

    let refresh = Request::RefreshItem {
        item: parent.id.clone(),
    };
    let refused = h.orchestrator(refresh).await.unwrap_err().to_string();
    let advice = format!("`onto` {}", parent.id);
    assert!(refused.contains(&advice), "{refused}");
    let asked = request("child", None, &[&parent]);
    let Reply::Item(child) = h.cli(Request::Enqueue(asked)).await.unwrap() else {
        panic!("enqueue replied no item");
    };
    h.dispatch().await;

    let QueueState::Failed { error, .. } = h.state(&child.id).await else {
        panic!("the child does not start: {:?}", h.state(&child.id).await);
    };
    assert!(error.contains(&advice), "{error}");
    assert!(error.contains("https://github.com/o/r/pull/100"), "{error}");
}
