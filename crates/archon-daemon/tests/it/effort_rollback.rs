//! `[models] effort_sizing = false` is the rollback to 1.6.3: for the same
//! inputs the daemon issues byte for byte what 1.6.3 issued. The files under
//! `effort_rollback/` were written by 1.6.3's code (`ARCHON_BLESS=1` there).
use std::path::{Path, PathBuf};

use crate::common::{self, Harness, MODEL, REVIEW_MODEL, choice};
use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::config::Config;
use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::SpawnRequest;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::QueueItem;

/// The 1.6.3 configuration this file pins: sizing off, as 1.6.3 shipped it.
fn rolled_back() -> Config {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config.models.effort_sizing = false;
    config
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/it/effort_rollback")
        .join(name)
}

/// `actual` equals the bytes 1.6.3 wrote for `name`; `ARCHON_BLESS=1` writes them.
fn same_as_1_6_3(name: &str, actual: &str) {
    assert!(!actual.is_empty(), "{name}: nothing was rendered");
    let path = fixture(name);
    if std::env::var_os("ARCHON_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        actual == expected,
        "{name} differs from what 1.6.3 issued:\n--- 1.6.3\n{expected}\n--- now\n{actual}"
    );
}

/// `text` with what differs between runs named instead: paths, ids, times.
fn stable(h: &Harness, text: &str, request: Option<&SpawnRequest>) -> String {
    let root = h.dir.path().canonicalize().unwrap();
    let mut text = text.replace(&root.display().to_string(), "<root>");
    let config = archon_core::paths::config_dir();
    text = text.replace(&config.display().to_string(), "<config>");
    if let Some(request) = request {
        text = text.replace(&request.idempotency_key, "<stage-run>");
        if let Some(deadline) = request.deadline {
            let shown = deadline.format("%Y-%m-%d %H:%M UTC").to_string();
            text = text.replace(&shown, "<deadline>");
        }
        if let Some(item) = request.labels.get("archon.item") {
            text = text.replace(item, "<item>");
        }
    }
    text
}

async fn enqueue(
    h: &Harness,
    pipeline: &str,
    plan: ModelPlan,
    review: Option<ReviewLevel>,
) -> QueueItem {
    let request = EnqueueRequest {
        intent: None,
        onto: None,
        stage_timeouts: Default::default(),
        repo: Some(RepoRef(common::REPO_NAME.into())),
        pipeline: Some(pipeline.into()),
        title: format!("a {pipeline} change"),
        brief: format!("brief for a {pipeline} change"),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan.into(),
        model_reasons: None,
        budget_usd: None,
        idempotency_key: None,
        autonomy: AutonomyPatch {
            review,
            ..Default::default()
        },
        origin: None,
    };
    match h.cli(Request::Enqueue(request)).await.unwrap() {
        Reply::Item(item) => item,
        other => panic!("enqueue replied {other:?}"),
    }
}

/// The orchestrator's prompt, without and with the GitHub section.
#[tokio::test]
async fn the_orchestrator_prompt_is_1_6_3_s_with_sizing_off() {
    let h = Harness::with_config(rolled_back()).await;
    let workspace = h.dir.path().canonicalize().unwrap().join("orchestrator");
    for (github, name) in [(false, "prompt.txt"), (true, "prompt-github.txt")] {
        let text =
            archon_daemon::supervisor::orchestrator_instructions(&h.ctx.config, &workspace, github)
                .unwrap();
        same_as_1_6_3(name, &stable(&h, &text, None));
    }
}

/// Every built-in pipeline as `list_pipelines` serves it, nudges included.
#[tokio::test]
async fn the_served_catalog_is_1_6_3_s_with_sizing_off() {
    let h = Harness::with_config(rolled_back()).await;
    let listed = h
        .orchestrator(Request::ListPipelines {
            repo: None,
            pipeline: None,
        })
        .await
        .unwrap();
    let Reply::Catalog(listing) = listed else {
        panic!("a catalog: {listed:?}");
    };
    assert!(listing.pipelines.iter().any(|p| p.name == "adaptive"));
    let text = serde_json::to_string_pretty(&listing).unwrap();
    same_as_1_6_3("catalog.json", &stable(&h, &text, None));
}

/// What an enqueue naming no model stores for the built-in pipelines.
#[tokio::test]
async fn filled_plans_are_1_6_3_s_with_sizing_off() {
    let h = Harness::with_config(rolled_back()).await;
    let mut plans = String::new();
    for pipeline in ["adaptive", "direct", "swarm", "refactor", "review"] {
        let item = enqueue(&h, pipeline, ModelPlan::default(), None).await;
        let plan = serde_json::to_string_pretty(item.model_plan.plan()).unwrap();
        plans.push_str(&format!("{pipeline}: {plan}\n"));
    }
    same_as_1_6_3("plans.txt", &plans);
}

/// A lead's contract with the plan naming its reviewers (a floor in 1.6.3).
#[tokio::test]
async fn a_named_reviewer_contract_is_1_6_3_s_with_sizing_off() {
    let mut config = rolled_back();
    config.models.max_effort = Some(Effort::XHigh);
    let h = Harness::with_config(config).await;
    let plan = ModelPlan(
        [
            ("build".to_owned(), vec![choice(MODEL, Effort::Medium)]),
            (
                "build.review".to_owned(),
                vec![choice(REVIEW_MODEL, Effort::Medium)],
            ),
        ]
        .into(),
    );
    enqueue(&h, "adaptive", plan, Some(ReviewLevel::Strict)).await;
    h.dispatch().await;
    let build = h.backend.requests()[0].clone();
    assert_eq!(build.labels["archon.stage"], "build");
    same_as_1_6_3(
        "contract-named.txt",
        &stable(&h, &build.instructions, Some(&build)),
    );
}

/// A lead's contract with reviewers from `[review]`, which asks for low.
#[tokio::test]
async fn a_profile_reviewer_contract_is_1_6_3_s_with_sizing_off() {
    let mut config = rolled_back();
    config.review.default_effort = Effort::Low;
    let h = Harness::with_config(config).await;
    let plan = ModelPlan([("build".to_owned(), vec![choice(MODEL, Effort::Medium)])].into());
    enqueue(&h, "adaptive", plan, Some(ReviewLevel::Standard)).await;
    h.dispatch().await;
    let build = h.backend.requests()[0].clone();
    assert_eq!(build.labels["archon.stage"], "build");
    same_as_1_6_3(
        "contract-profile.txt",
        &stable(&h, &build.instructions, Some(&build)),
    );
}
