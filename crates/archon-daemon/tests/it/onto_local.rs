//! Work queued `onto` an item or branch in a `ship = local` repository: with
//! no pull request to deliver into, it works on the target item's branch in
//! its worktree, as a fix of that item, and ships as a local commit there.

use crate::common;

use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::model::ModelPlan;
use archon_core::protocol::{EnqueueRequest, Reply, RepoAdd, RepoPatch, RepoRef, Request};
use archon_core::pull_request::OntoTarget;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::ShipMode;
use archon_core::run::PipelineRun;
use common::delivery::{pass_by, shipped_by};
use common::{Harness, REPO_NAME, REVIEW_MODEL, git};
use serde_json::json;

async fn local() -> Harness {
    let h = Harness::new().await;
    let patch = RepoPatch {
        ship: Some(ShipMode::Local),
        ..RepoPatch::default()
    };
    let local = Request::RepoUpdate {
        repo: RepoRef(REPO_NAME.into()),
        patch,
    };
    h.cli(local).await.unwrap();
    h
}

/// A `direct` change whose review owes one reviewer, `onto` the target given.
fn request(title: &str, onto: Option<OntoTarget>, depends_on: &[&QueueItem]) -> EnqueueRequest {
    EnqueueRequest {
        onto,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(REPO_NAME.into())),
        pipeline: Some("direct".into()),
        title: title.into(),
        brief: format!("brief for {title}"),
        priority: Default::default(),
        depends_on: depends_on.iter().map(|item| item.id.clone()).collect(),
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
    }
}

async fn enqueue(h: &Harness, request: EnqueueRequest) -> Result<QueueItem, ArchonError> {
    match h.cli(Request::Enqueue(request)).await? {
        Reply::Item(item) => Ok(item),
        other => panic!("enqueue replied {other:?}"),
    }
}

async fn enqueue_onto(
    h: &Harness,
    title: &str,
    onto: OntoTarget,
) -> Result<QueueItem, ArchonError> {
    enqueue(h, request(title, Some(onto), &[])).await
}

async fn refused(h: &Harness, onto: OntoTarget) -> String {
    let error = enqueue_onto(h, "refused", onto).await.unwrap_err();
    let refusal = matches!(
        error,
        ArchonError::Invalid(_) | ArchonError::InvalidTransition(_)
    );
    assert!(refusal, "{error:?}");
    error.to_string()
}

/// The running lead commits a change and passes it with one reviewer that
/// read the whole change; the dispatcher then runs.
async fn settle_reviewed(h: &Harness) {
    let lead = h.running().await.remove(0);
    let agent = lead.aether_agent.clone().unwrap();
    let reviewer = h.backend.delegate(&agent, REVIEW_MODEL);
    let extra = pass_by(std::slice::from_ref(&reviewer));
    let built = shipped_by(h, &lead, "Built; reviewed", extra).await;
    let archon_core::run::StageOutcome::Completed {
        structured: Some(record),
        ..
    } = &built
    else {
        panic!("a completed change: {built:?}");
    };
    let verdict = json!({"verdict": "pass", "head": record["commit"], "findings": []});
    h.backend.complete_delegated(&reviewer, verdict);
    h.settle_running(|_| built.clone()).await;
    h.dispatch().await;
}

async fn finished(h: &Harness, request: EnqueueRequest) -> (QueueItem, PipelineRun) {
    let item = enqueue(h, request).await.expect("queued");
    h.dispatch().await;
    settle_reviewed(h).await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let run = h.run(&item.id).await;
    (item, run)
}

async fn shipped(h: &Harness, title: &str) -> (QueueItem, PipelineRun) {
    finished(h, request(title, None, &[])).await
}

async fn owner_of(h: &Harness, id: &QueueItemId) -> Option<QueueItemId> {
    let fix = h.ctx.store.lock().await.stack_fix(id).unwrap();
    fix.map(|fix| fix.owner)
}

/// The commit the record stage of `id`'s run says its local delivery left.
async fn local_commit(h: &Harness, id: &QueueItemId) -> Option<String> {
    h.stages(id)
        .await
        .iter()
        .filter_map(|stage| stage.structured.as_ref())
        .find_map(|data| data["local_commit"].as_str().map(str::to_owned))
}

/// Queued onto a finished item, the work runs on that item's branch in its
/// worktree, is reviewed, and leaves its commit as that branch's tip.
#[tokio::test]
async fn onto_an_item_commits_on_its_branch_in_its_worktree() {
    let h = local().await;
    let (owner, run) = shipped(&h, "the change").await;
    let before = git(&h.repo, &["rev-parse", &run.branch]);

    let mut asked = request("follow-up", Some(OntoTarget::Item(owner.id.clone())), &[]);
    asked.idempotency_key = Some("follow-up".into());
    let item = enqueue(&h, asked.clone()).await.expect("queued");
    assert_eq!(item.onto, None, "no pull request is frozen");
    assert_eq!(
        (item.title.as_str(), item.brief.as_str()),
        ("follow-up", "brief for follow-up")
    );
    let fix = h.ctx.store.lock().await.stack_fix(&item.id).unwrap();
    let fix = fix.expect("a fix of the owner");
    assert_eq!((&fix.owner, &fix.branch), (&owner.id, &run.branch));
    assert!(fix.refresh.is_none() && !fix.team);
    assert_eq!(enqueue(&h, asked).await.unwrap().id, item.id, "a replay");
    let again = refused(&h, OntoTarget::Item(owner.id.clone())).await;
    assert!(again.contains("still to finish"), "{again}");

    h.dispatch().await;
    let working = h.run(&item.id).await;
    assert_eq!(working.branch, run.branch, "the owner's branch");
    assert_eq!(working.workspace, run.workspace, "the owner's worktree");
    assert_eq!(git(&working.workspace, &["rev-parse", "HEAD"]), before);
    let told = h.backend.requests().pop().unwrap().instructions;
    assert!(told.contains("\n## Existing branch\n"), "{told}");
    assert!(
        told.contains(&run.branch) && told.contains(owner.id.as_str()),
        "{told}"
    );
    settle_reviewed(&h).await;

    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Succeeded { .. }), "{state:?}");
    let tip = git(&h.repo, &["rev-parse", &run.branch]);
    assert_ne!(tip, before, "the branch moved on");
    assert_eq!(git(&h.repo, &["rev-parse", &format!("{tip}^")]), before);
    assert_eq!(local_commit(&h, &item.id).await, Some(tip));
    assert!(
        h.state(&owner.id).await.is_terminal(),
        "the owner did not run"
    );
}

/// A branch names the item whose last run left it, never a fix that ran on
/// it too; a branch no item left, or no plain branch name, is refused.
#[tokio::test]
async fn onto_a_branch_resolves_to_the_item_whose_last_run_left_it() {
    let h = local().await;
    let (owner, run) = shipped(&h, "the change").await;
    let first = enqueue_onto(&h, "first", OntoTarget::Item(owner.id.clone()))
        .await
        .unwrap();
    h.dispatch().await;
    settle_reviewed(&h).await;
    assert!(matches!(
        h.state(&first.id).await,
        QueueState::Succeeded { .. }
    ));

    let item = enqueue_onto(&h, "second", OntoTarget::Branch(run.branch.clone()))
        .await
        .unwrap();
    assert_eq!(owner_of(&h, &item.id).await, Some(owner.id.clone()));

    let none = refused(&h, OntoTarget::Branch("no-such-branch".into())).await;
    assert!(none.contains("no item of"), "{none}");
    let odd = refused(&h, OntoTarget::Branch("-x".into())).await;
    assert!(odd.contains("no plain branch name"), "{odd}");
}

/// What has no branch of a finished item to work on is refused, and why.
#[tokio::test]
async fn onto_is_refused_without_a_finished_branch_of_this_repository_s_own() {
    let h = local().await;
    let pr = refused(&h, OntoTarget::Pr(1)).await;
    assert!(
        pr.contains("ships locally") && pr.contains("name the item"),
        "{pr}"
    );

    let (owner, _) = shipped(&h, "the change").await;
    let fix = enqueue_onto(&h, "fix", OntoTarget::Item(owner.id.clone()))
        .await
        .unwrap();
    let nested = refused(&h, OntoTarget::Item(fix.id.clone())).await;
    assert!(
        nested.contains(&format!("queue onto {} instead", owner.id)),
        "{nested}"
    );
    h.dispatch().await;
    let running = refused(&h, OntoTarget::Item(owner.id.clone())).await;
    assert!(running.contains("still to finish"), "{running}");
    settle_reviewed(&h).await;

    let queued = enqueue(&h, request("queued", None, &[])).await.unwrap();
    let unfinished = refused(&h, OntoTarget::Item(queued.id.clone())).await;
    assert!(unfinished.contains("only a finished item"), "{unfinished}");

    let other = other_repo_item(&h).await;
    let elsewhere = refused(&h, OntoTarget::Item(other.id.clone())).await;
    assert!(elsewhere.contains("is not in"), "{elsewhere}");

    let with_dependency = request("both", Some(OntoTarget::Item(owner.id.clone())), &[&owner]);
    let both = enqueue(&h, with_dependency).await.unwrap_err().to_string();
    assert!(both.contains("cannot also stack on dependencies"), "{both}");
}

/// An item of a second, local repository, queued and never run.
async fn other_repo_item(h: &Harness) -> QueueItem {
    let repo = common::init_repo(&h.dir.path().join("other"));
    let add = RepoAdd {
        path: repo,
        name: Some("other".into()),
        ship: Some(ShipMode::Local),
        base_branch: Some("main".into()),
        default_pipeline: None,
        auto_merge: false,
        github_watch: false,
    };
    h.cli(Request::RepoAdd(add)).await.expect("repo add");
    let mut asked = request("elsewhere", None, &[]);
    asked.repo = Some(RepoRef("other".into()));
    enqueue(h, asked).await.expect("queued")
}

/// An item stacked on an unmerged parent, or whose last run was a team's,
/// is refused as a target.
#[tokio::test]
async fn onto_a_stacked_or_team_item_is_refused() {
    let h = local().await;
    let (parent, _) = shipped(&h, "the parent").await;
    let (child, _) = finished(&h, request("the child", None, &[&parent])).await;
    let stacked = refused(&h, OntoTarget::Item(child.id.clone())).await;
    assert!(stacked.contains("is stacked on"), "{stacked}");

    let (team, _) = shipped(&h, "a team's").await;
    let mut run = h.run(&team.id).await;
    run.team_root = Some(run.workspace.clone());
    h.ctx.store.lock().await.update_run(&run).unwrap();
    let teamed = refused(&h, OntoTarget::Item(team.id.clone())).await;
    assert!(teamed.contains("ran as a team"), "{teamed}");
}
