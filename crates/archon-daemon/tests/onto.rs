//! Work queued `onto` an existing pull request: enqueue resolves the target
//! with `gh` and freezes it, refuses one Archon could not push to, and the
//! item's branch starts from the frozen head. A bare `origin` and a fake `gh`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::protocol::{EnqueueRequest, ItemPatch, Reply, RepoRef, Request};
use archon_core::pull_request::{OntoPullRequest, OntoTarget, PullRequestRef};
use archon_core::queue::QueueItem;
use common::delivery::{add_origin, origin_of};
use common::{Harness, PIPELINE, REPO_NAME, git, plan};
use serde_json::{Value, json};

/// `pr view <number|url>` shows `pr-<number>.json`; `pr list --head <branch>`
/// lists `branch-<branch>.json`, else none.
const GH: &str = r#"#!/bin/sh
state="$(dirname "$0")/gh-onto"
printf '%s\n' "$*" >> "$state/calls"
case "$1 $2" in
"pr view")
    [ "$4" = "--json" ] || exit 2
    shown="$state/pr-${3##*/}.json"
    [ -f "$shown" ] || { echo "GraphQL: Could not resolve to a PullRequest" >&2; exit 1; }
    cat "$shown"
    ;;
"pr list")
    if [ "$3 $4 $5 $6 $7" = "--state open --limit 1000 --json" ]; then printf '[]'; exit 0; fi
    [ "$3 $5 $6 $7 $8 $9" = "--head --state open --limit 20 --json" ] || exit 2
    listed="$state/branch-$(printf '%s' "$4" | tr '/' '_').json"
    if [ -f "$listed" ]; then cat "$listed"; else printf '[]'; fi
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
async fn a_closed_merged_or_fork_target_is_refused_at_enqueue_with_the_reason() {
    let (h, gh, head) = setup().await;
    gh.show(101, &head, json!({"state": "CLOSED"}));
    gh.show(102, &head, json!({"state": "MERGED"}));
    gh.show(
        103,
        &head,
        json!({"isCrossRepository": true, "headRepositoryOwner": {"login": "fork"}}),
    );
    gh.show(104, &head, json!({"headRepository": null}));

    for (number, reason) in [
        (101, "it is closed"),
        (102, "it is merged"),
        (103, "its head `fork/r` is a fork"),
        (104, "its head repository is gone"),
        (105, "Could not resolve to a PullRequest"),
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
