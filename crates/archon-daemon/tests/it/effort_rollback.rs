//! `[models] effort_sizing = false` is the rollback to 1.6.3: for the same
//! inputs the daemon issues byte for byte what 1.6.3 issued. The files under
//! `effort_rollback/` were written by 1.6.3's code (`ARCHON_BLESS=1` there),
//! then re-blessed for the review-carry wording (C3), which is not effort sizing.
use std::path::{Path, PathBuf};

use crate::common::{self, Harness, MODEL, REVIEW_MODEL, choice};
use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::catalog::{CatalogListing, PipelineSource};
use archon_core::config::Config;
use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::SpawnRequest;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::QueueItem;

/// The 1.6.3 configuration this file pins: sizing off, as 1.6.3 shipped it.
fn rolled_back() -> Config {
    let mut config = Config::default();
    config.review.auto_repair = false;
    config.models.effort_sizing = Some(false);
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
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        actual == expected,
        "{name} differs from what 1.6.3 issued:\n--- 1.6.3\n{expected}\n--- now\n{actual}"
    );
}

/// Set in the child process a pinned test reruns itself in.
const HERMETIC: &str = "ARCHON_TEST_EMPTY_CONFIG_DIR";

/// Whether this process may run pinned `test`: one with an empty config
/// directory may. Any other reruns it in such a child and asserts it passed,
/// since the operator's own pipelines there would override built-ins.
fn in_empty_config_dir(test: &str) -> bool {
    let config = archon_core::paths::config_dir();
    if std::env::var_os(HERMETIC).is_some() {
        let mut entries = std::fs::read_dir(&config).unwrap();
        assert!(entries.next().is_none(), "{} is empty", config.display());
        return true;
    }
    let empty = tempfile::tempdir().unwrap();
    let name = format!("effort_rollback::{test}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--test-threads=1", "--nocapture"])
        .env(HERMETIC, "1")
        .env(archon_core::paths::CONFIG_DIR_VAR, empty.path())
        .output()
        .unwrap();
    let (out, err) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{name} failed:\n{out}\n{err}");
    assert!(out.contains("1 passed"), "{name} ran once:\n{out}");
    false
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
    if !in_empty_config_dir("the_orchestrator_prompt_is_1_6_3_s_with_sizing_off") {
        return;
    }
    let h = Harness::with_config(rolled_back()).await;
    let workspace = h.dir.path().canonicalize().unwrap().join("orchestrator");
    for (github, name) in [(false, "prompt.txt"), (true, "prompt-github.txt")] {
        let text =
            archon_daemon::supervisor::orchestrator_instructions(&h.ctx.config, &workspace, github)
                .unwrap();
        same_as_1_6_3(name, &stable(&h, &text, None));
    }
}

/// The pipelines `list_pipelines` serves, all built in.
async fn served_builtins(h: &Harness) -> CatalogListing {
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
    let builtin = |source: &PipelineSource| *source == PipelineSource::Builtin;
    assert!(listing.pipelines.iter().all(|p| builtin(&p.source)));
    assert!(listing.problems.iter().all(|p| builtin(&p.source)));
    assert!(listing.pipelines.iter().any(|p| p.name == "adaptive"));
    listing
}

/// Every built-in pipeline as `list_pipelines` serves it, nudges included.
#[tokio::test]
async fn the_served_catalog_is_1_6_3_s_with_sizing_off() {
    if !in_empty_config_dir("the_served_catalog_is_1_6_3_s_with_sizing_off") {
        return;
    }
    let h = Harness::with_config(rolled_back()).await;
    let listing = served_builtins(&h).await;
    let text = serde_json::to_string_pretty(&listing).unwrap();
    same_as_1_6_3("catalog.json", &stable(&h, &text, None));
}

/// On, as by default, the catalog is 1.6.3's but for the stage nudges the
/// rubric rewords, so none contradicts it.
#[tokio::test]
async fn the_served_catalog_differs_from_1_6_3_s_only_by_reworded_nudges() {
    if !in_empty_config_dir("the_served_catalog_differs_from_1_6_3_s_only_by_reworded_nudges") {
        return;
    }
    let mut config = Config::default();
    config.review.auto_repair = false;
    let h = Harness::with_config(config).await;
    let listing = served_builtins(&h).await;
    let text = stable(&h, &serde_json::to_string_pretty(&listing).unwrap(), None);
    let mut served: serde_json::Value = serde_json::from_str(&text).unwrap();
    let written = std::fs::read_to_string(fixture("catalog.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    let mut reworded = Vec::new();
    let pipelines = served["pipelines"].as_array_mut().unwrap();
    for (pipeline, was) in pipelines
        .iter_mut()
        .zip(written["pipelines"].as_array().unwrap())
    {
        assert_eq!(pipeline["name"], was["name"], "the same pipelines in order");
        let name = pipeline["name"].as_str().unwrap().to_owned();
        let stages = pipeline["stages"].as_array_mut().unwrap();
        for (stage, had) in stages.iter_mut().zip(was["stages"].as_array().unwrap()) {
            if stage["nudge"] != had["nudge"] {
                reworded.push(format!("{name}.{}", stage["id"].as_str().unwrap()));
                stage["nudge"] = had["nudge"].clone();
            }
        }
    }
    assert_eq!(
        reworded,
        ["adaptive.build", "code-audit.audit", "direct.implement"]
    );
    assert_eq!(served, written, "nothing else differs");
}

/// What an enqueue naming no model stores for the built-in pipelines.
#[tokio::test]
async fn filled_plans_are_1_6_3_s_with_sizing_off() {
    if !in_empty_config_dir("filled_plans_are_1_6_3_s_with_sizing_off") {
        return;
    }
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
    if !in_empty_config_dir("a_named_reviewer_contract_is_1_6_3_s_with_sizing_off") {
        return;
    }
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
    let spawn = h.stored_spawn(&build).await;
    let named = serde_json::json!([choice(REVIEW_MODEL, Effort::Medium)]);
    assert_eq!(spawn["issued_reviewers"], named, "what the contract named");
    assert_eq!(spawn["effort_contract"], "floor");
}

/// A lead's contract with reviewers from `[review]`, which asks for low.
#[tokio::test]
async fn a_profile_reviewer_contract_is_1_6_3_s_with_sizing_off() {
    if !in_empty_config_dir("a_profile_reviewer_contract_is_1_6_3_s_with_sizing_off") {
        return;
    }
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

/// An operator's own `direct` overrides the built-in one where they run
/// Archon; the pinned catalog is still the built-ins 1.6.3 served.
#[test]
fn an_operator_s_pipeline_never_reaches_the_pinned_catalog() {
    if std::env::var_os(HERMETIC).is_some() {
        return;
    }
    let config = tempfile::tempdir().unwrap();
    let ours = config.path().join("pipelines/direct");
    std::fs::create_dir_all(&ours).unwrap();
    let builtin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pipelines/direct");
    for file in ["pipeline.toml", "implement.md"] {
        let text = std::fs::read_to_string(builtin.join(file)).unwrap();
        let text = text.replace("description = \"", "description = \"Ours: ");
        std::fs::write(ours.join(file), text).unwrap();
    }
    let name = "effort_rollback::the_served_catalog_is_1_6_3_s_with_sizing_off";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1"])
        .env(archon_core::paths::CONFIG_DIR_VAR, config.path())
        .env_remove("ARCHON_BLESS")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("1 passed"), "{out}");
}

/// A lead's create whose reply was lost is replayed, after a restart that
/// flipped `effort_sizing`, exactly as it was issued: the same request under
/// the same key, its contract and its recorded rule the issuing config's.
#[tokio::test]
async fn a_lost_spawn_is_replayed_as_issued_across_the_switch() {
    use archon_core::model::EffortContract;
    for sizing in [true, false] {
        let mut config = Config::default();
        config.review.auto_repair = false;
        config.models.max_effort = Some(Effort::XHigh);
        config.models.effort_sizing = Some(sizing);
        let mut h = Harness::with_config(config.clone()).await;
        h.backend.lose_next_reply();
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
        let issued = h.backend.requests()[0].clone();
        assert_eq!(issued.labels["archon.stage"], "build");
        let mode = EffortContract::of(sizing);
        let stated = archon_pipeline::contract::stated_effort_contract(&issued.instructions);
        assert_eq!(stated, Some(mode), "issued under sizing {sizing}");

        config.models.effort_sizing = Some(!sizing);
        h.restart_with(config);
        archon_daemon::executor::recover(&h.ctx)
            .await
            .expect("recover");
        h.dispatch().await;
        let requests = h.backend.requests();
        assert_eq!(requests.len(), 2, "the lost create is replayed once");
        assert_eq!(requests[1], issued, "byte-identical, under the same key");
        assert_eq!(h.backend.created(), 1, "no second agent");
        let spawn = h.stored_spawn(&issued).await;
        assert_eq!(spawn["effort_contract"], serde_json::json!(mode));
    }
}
