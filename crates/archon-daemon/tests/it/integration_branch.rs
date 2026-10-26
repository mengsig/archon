//! Verification tiers (issue #82): a new item forks from `origin`'s
//! integration branch and its pull request targets it, a missing integration
//! branch is created from the release branch, a committed `.archon/repo.toml`
//! sets the tiers under the registry's, and every agent's contract names the
//! fast check and forbids the full one. A bare `origin` and a fake `gh`.

use crate::common;

use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::model::ModelPlan;
use archon_core::notify::NotificationKind;
use archon_core::ports::SpawnRequest;
use archon_core::protocol::{
    Clearable, EnqueueRequest, Reply, RepoPatch, RepoRef, Request, TiersPatch,
};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use common::delivery::{FAKE_LOGIN, FakeGh, add_origin, origin_of, pass_by, pushed, shipped_by};
use common::{Harness, REPO_NAME, REVIEW_MODEL, git};
use serde_json::json;

const CHECKS: &str = "\n## Checks\n";

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// Enqueue a `direct` change whose review owes one reviewer.
async fn enqueue(h: &Harness, title: &str) -> QueueItem {
    let request = EnqueueRequest {
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: title.into(),
        brief: "brief".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: ModelPlan::default().into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review: Some(ReviewLevel::Standard),
            ..AutonomyPatch::default()
        },
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.expect("enqueue") {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

async fn update(h: &Harness, patch: RepoPatch) {
    let request = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(request).await.expect("repo update");
}

async fn integrate_into(h: &Harness, branch: &str) {
    let tiers = TiersPatch {
        integration_branch: Clearable::Set(branch.into()),
        ..TiersPatch::default()
    };
    update(
        h,
        RepoPatch {
            tiers,
            ..RepoPatch::default()
        },
    )
    .await;
}

/// Commit `file` on `origin`'s `branch`, made from `main` when it is new,
/// from a clone of its own; the harness repository stays where it was.
fn advance_origin(h: &Harness, branch: &str, file: &str) -> String {
    let clone = h.dir.path().join(format!("upstream-{file}"));
    let origin = origin_of(h);
    let (origin, to) = (origin.to_str().unwrap(), clone.to_str().unwrap());
    git(h.dir.path(), &["clone", "--quiet", origin, to]);
    if pushed(&origin_of(h), branch).is_some() {
        git(&clone, &["checkout", "--quiet", branch]);
    }
    std::fs::write(clone.join(file), "upstream\n").unwrap();
    git(&clone, &["add", file]);
    git(&clone, &["commit", "--quiet", "-m", "Upstream change"]);
    git(
        &clone,
        &["push", "--quiet", "origin", &format!("HEAD:{branch}")],
    );
    git(&clone, &["rev-parse", "HEAD"])
}

fn write_repo_file(h: &Harness, toml: &str) {
    std::fs::write(h.repo.join(".archon/repo.toml"), toml).expect("repo.toml");
}

fn requests_of(h: &Harness, item: &QueueItem) -> Vec<SpawnRequest> {
    let mut requests = h.backend.requests();
    requests.retain(|request| request.labels["archon.item"] == item.id.as_str());
    requests
}

/// The running lead commits a change and passes it with one reviewer; the
/// dispatcher then ships it.
async fn settle_reviewed(h: &Harness) {
    let lead = h.running().await.remove(0);
    let agent = lead.aether_agent.clone().unwrap();
    let reviewer = h.backend.delegate(&agent, REVIEW_MODEL);
    let built = shipped_by(
        h,
        &lead,
        "Built; reviewed",
        pass_by(std::slice::from_ref(&reviewer)),
    )
    .await;
    let head = match &built {
        archon_core::run::StageOutcome::Completed {
            structured: Some(record),
            ..
        } => record["commit"].clone(),
        other => panic!("a completed change: {other:?}"),
    };
    let verdict = json!({"verdict": "pass", "head": head, "findings": []});
    h.backend.complete_delegated(&reviewer, verdict);
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
}

/// The local `next` is stale; the item forks from `origin/next` and its
/// pull request targets `next`, while the operator's branches stay put.
#[tokio::test]
async fn an_item_forks_from_origin_s_integration_branch_and_its_pull_request_targets_it() {
    let (h, gh) = delivering().await;
    git(&h.repo, &["branch", "next", "main"]);
    git(&h.repo, &["push", "--quiet", "origin", "next"]);
    let stale = git(&h.repo, &["rev-parse", "next"]);
    let upstream = advance_origin(&h, "next", "integrated.txt");
    integrate_into(&h, "next").await;

    let item = enqueue(&h, "onto next").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), upstream);
    assert_eq!(git(&h.repo, &["rev-parse", "next"]), stale, "never moved");
    assert!(h.notices(NotificationKind::Decision).await.is_empty());

    settle_reviewed(&h).await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let created: Vec<String> = gh
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("pr create"))
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
    assert!(
        created[0].starts_with("pr create --base next --head "),
        "{created:?}"
    );
}

/// The local `next` is stale (issue #275): the lead hands reviewers the fork
/// from `origin/next`, never the local `next`, whose diff would hold every
/// commit merged upstream since.
#[tokio::test]
async fn a_stale_local_base_leaves_the_prompt_naming_the_fork_commit() {
    let (h, _gh) = delivering().await;
    git(&h.repo, &["branch", "next", "main"]);
    git(&h.repo, &["push", "--quiet", "origin", "next"]);
    let stale = git(&h.repo, &["rev-parse", "next"]);
    let upstream = advance_origin(&h, "next", "integrated.txt");
    integrate_into(&h, "next").await;

    let item = enqueue(&h, "diff from the fork").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let fork = git(
        &run.workspace,
        &["merge-base", "HEAD", "next", "origin/next"],
    );
    assert_eq!(fork, upstream, "the branch forked from origin/next");
    let requests = requests_of(&h, &item);
    let prompt = &requests[0].objective;
    assert!(
        prompt.contains(&format!("the base (`{upstream}`) and head commits")),
        "{prompt}"
    );
    assert!(!prompt.contains(&stale), "{prompt}");
    assert!(!prompt.contains("merge-base next"), "{prompt}");
    assert_eq!(git(&h.repo, &["rev-parse", "next"]), stale, "never moved");
}

/// `origin` has no integration branch: it is made from `origin`'s release
/// branch, ahead of the local one, the operator hears of it, and the item
/// forks from it.
#[tokio::test]
async fn a_missing_integration_branch_is_created_on_origin_from_the_release_branch() {
    let (h, _gh) = delivering().await;
    let local = git(&h.repo, &["rev-parse", "main"]);
    let main = advance_origin(&h, "main", "released.txt");
    integrate_into(&h, "next").await;

    let item = enqueue(&h, "first onto next").await;
    h.dispatch().await;
    assert_eq!(pushed(&origin_of(&h), "next"), Some(main.clone()));
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), main);
    assert_eq!(git(&h.repo, &["rev-parse", "main"]), local, "never moved");
    assert!(
        !git(&h.repo, &["branch", "--list", "next"]).contains("next"),
        "no local branch is made in pr mode"
    );
    let said = h.notices(NotificationKind::Decision).await;
    let created = |n: &&String| n.contains("Created the integration branch `origin/next`");
    assert_eq!(said.iter().filter(created).count(), 1, "{said:?}");
}

/// A repository that runs as a GitHub login makes its missing integration
/// branch only as that login: an `origin` off github.com gets none, with no
/// fallback to the daemon's account, and the operator hears why.
#[tokio::test]
async fn a_missing_integration_branch_is_never_made_as_the_daemon_for_a_login() {
    let (h, gh) = delivering().await;
    integrate_into(&h, "next").await;
    let login = RepoPatch {
        gh_user: Clearable::Set(FAKE_LOGIN.parse().unwrap()),
        ..RepoPatch::default()
    };
    update(&h, login).await;

    enqueue(&h, "first onto next as a login").await;
    h.dispatch().await;
    assert_eq!(pushed(&origin_of(&h), "next"), None);
    let said = h.notices(NotificationKind::Decision).await;
    let blocked = |n: &&String| n.contains("BLOCKED: `origin` is") && n.contains(FAKE_LOGIN);
    assert_eq!(said.iter().filter(blocked).count(), 1, "{said:?}");
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}

/// In `local` mode the integration branch is a local one, made from the
/// release branch when missing.
#[tokio::test]
async fn a_local_repository_creates_its_integration_branch_locally() {
    let h = Harness::new().await;
    let main = git(&h.repo, &["rev-parse", "main"]);
    update(
        &h,
        RepoPatch {
            ship: Some(ShipMode::Local),
            ..RepoPatch::default()
        },
    )
    .await;
    integrate_into(&h, "next").await;

    let item = enqueue(&h, "local onto next").await;
    h.dispatch().await;
    assert_eq!(git(&h.repo, &["rev-parse", "next"]), main);
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), main);
}

/// The committed file sets the integration branch and the checks, every
/// agent's contract names them, and the registry's values win over it.
#[tokio::test]
async fn a_committed_repo_file_is_read_and_the_registry_overrides_it() {
    let (h, _gh) = delivering().await;
    let next = advance_origin(&h, "next", "next.txt");
    let staging = advance_origin(&h, "staging", "staging.txt");
    write_repo_file(
        &h,
        "integration_branch = \"next\"\n\n[checks]\nfast = \"make quick\"\nfull = \"make ci\"\n",
    );

    let first = enqueue(&h, "from the file").await;
    h.dispatch().await;
    let run = h.run(&first.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), next);
    let lead = requests_of(&h, &first).remove(0);
    assert!(lead.instructions.contains(CHECKS), "{}", lead.instructions);
    assert!(lead.instructions.contains("fast check: `make quick`"));
    assert!(
        lead.instructions
            .contains("Do not run the full suite (`make ci`)")
    );

    let tiers = TiersPatch {
        integration_branch: Clearable::Set("staging".into()),
        fast_check: Clearable::Set("make fast".into()),
        ..TiersPatch::default()
    };
    update(
        &h,
        RepoPatch {
            tiers,
            ..RepoPatch::default()
        },
    )
    .await;
    let second = enqueue(&h, "from the registry").await;
    h.dispatch().await;
    let run = h.run(&second.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), staging);
    let lead = requests_of(&h, &second).remove(0);
    assert!(lead.instructions.contains("fast check: `make fast`"));
    assert!(
        lead.instructions
            .contains("Do not run the full suite (`make ci`)"),
        "the file's full check stays"
    );
}

/// A broken committed file stops the item with an error naming it, rather
/// than forking from a branch the repository did not mean.
#[tokio::test]
async fn a_broken_repo_file_is_refused_naming_it() {
    let (h, _gh) = delivering().await;
    write_repo_file(&h, "integration = \"next\"\n");
    let item = enqueue(&h, "broken file").await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    let said = format!("{state:?}");
    assert!(said.contains("repo.toml"), "{said}");
}

/// Without tiers an item forks from the base branch and its contract has
/// no Checks section, as before.
#[tokio::test]
async fn a_repository_without_tiers_behaves_as_before() {
    let (h, gh) = delivering().await;
    let main = git(&h.repo, &["rev-parse", "main"]);
    let item = enqueue(&h, "plain").await;
    h.dispatch().await;
    let run = h.run(&item.id).await;
    assert_eq!(git(&run.workspace, &["rev-parse", "HEAD"]), main);
    assert_eq!(pushed(&origin_of(&h), "next"), None);
    let lead = requests_of(&h, &item).remove(0);
    assert!(!lead.instructions.contains(CHECKS), "{}", lead.instructions);

    settle_reviewed(&h).await;
    let created: Vec<String> = gh
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("pr create"))
        .collect();
    assert!(
        created
            .first()
            .is_some_and(|c| c.starts_with("pr create --base main ")),
        "{created:?}"
    );
}

/// The registry refuses tiers git could misread or that fold the two branches.
#[tokio::test]
async fn the_registry_refuses_malformed_tiers() {
    let h = Harness::new().await;
    for tiers in [
        TiersPatch {
            integration_branch: Clearable::Set("--force".into()),
            ..TiersPatch::default()
        },
        TiersPatch {
            integration_branch: Clearable::Set("main".into()),
            ..TiersPatch::default()
        },
        TiersPatch {
            fast_check: Clearable::Set("a\nb".into()),
            ..TiersPatch::default()
        },
    ] {
        let request = Request::RepoUpdate {
            repo: RepoRef(REPO_NAME.into()),
            patch: RepoPatch {
                tiers: tiers.clone(),
                ..RepoPatch::default()
            },
        };
        assert!(h.cli(request).await.is_err(), "{tiers:?}");
    }
}

/// The two branches are compared in the tiers in force: the registry may
/// name `main` as its integration branch over a file releasing `trunk`.
#[tokio::test]
async fn the_registry_is_validated_over_the_committed_file() {
    let h = Harness::new().await;
    write_repo_file(&h, "release_branch = \"trunk\"\n");
    integrate_into(&h, "main").await;
    write_repo_file(&h, "");
    let folded = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch: RepoPatch::default(),
    };
    let refused = h.cli(folded).await.unwrap_err().to_string();
    assert!(refused.contains("must differ"), "{refused}");
}
