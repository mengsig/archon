//! Persisted operator settings: precedence, restart, validation, per-repo
//! budgets and default pipelines.

mod common;

use archon_core::autonomy::{AutonomyPatch, MergeMode, ToolsMode};
use archon_core::error::ArchonError;
use archon_core::protocol::{EnqueueRequest, Reply, RepoRef, Request};
use archon_core::queue::Caps;
use archon_core::settings::{OperatorSettings, SettingSource, SettingsView};
use common::{Harness, REPO_NAME, plan};

fn repo() -> Option<RepoRef> {
    Some(RepoRef(REPO_NAME.into()))
}

async fn view(h: &Harness, repo: Option<RepoRef>) -> SettingsView {
    match h.cli(Request::Settings { repo }).await {
        Ok(Reply::Settings(view)) => *view,
        other => panic!("settings replied {other:?}"),
    }
}

async fn set(
    h: &Harness,
    repo: Option<RepoRef>,
    settings: OperatorSettings,
) -> Result<Reply, ArchonError> {
    h.cli(Request::SetSettings { repo, settings }).await
}

fn enqueue(pipeline: Option<&str>, budget_usd: Option<f64>) -> Request {
    Request::Enqueue(EnqueueRequest {
        repo: repo(),
        pipeline: pipeline.map(str::to_owned),
        title: "t".into(),
        brief: "b".into(),
        priority: Default::default(),
        depends_on: Vec::new(),
        tags: Vec::new(),
        model_plan: plan().into(),
        model_reasons: None,
        stage_timeouts: Default::default(),
        budget_usd,
        idempotency_key: None,
        autonomy: Default::default(),
        origin: None,
    })
}

#[tokio::test]
async fn settings_and_set_caps_survive_a_restart_until_reset() {
    let mut h = Harness::new().await;
    let global = OperatorSettings {
        autonomy_max: AutonomyPatch::parse_list("merge=auto").unwrap(),
        budget_usd_per_item: Some(4.0),
        ..OperatorSettings::default()
    };
    set(&h, None, global.clone()).await.unwrap();
    let caps = Caps {
        max_parallel_agents: 3,
        ..Caps::default()
    };
    h.cli(Request::SetCaps { caps }).await.unwrap();
    h.restart();

    let after = view(&h, None).await;
    assert_eq!(
        after.scoped.autonomy_max, global.autonomy_max,
        "SetCaps keeps other fields"
    );
    assert_eq!(after.effective.caps.max_parallel_agents, 3);
    assert_eq!(
        after.effective.caps.budget_usd_per_item, 0.0,
        "SetCaps replaced it"
    );
    assert_eq!(after.effective.autonomy.ceiling.merge, MergeMode::Auto);
    assert_eq!(
        after.effective.sources["autonomy.max.merge"],
        SettingSource::Global
    );
    let Ok(Reply::Status(status)) = h.cli(Request::Status).await else {
        panic!("status");
    };
    assert_eq!(status.caps.max_parallel_agents, 3);
    assert_eq!(status.autonomy.ceiling.merge, MergeMode::Auto);

    h.cli(Request::ResetSettings { repo: None }).await.unwrap();
    h.restart();
    let reset = view(&h, None).await;
    assert!(reset.scoped.is_empty());
    assert_eq!(reset.effective.caps, h.ctx.config.limits.caps());
    assert_eq!(reset.effective.autonomy.ceiling.merge, MergeMode::Manual);
}

#[tokio::test]
async fn a_repository_overlay_beats_the_global_one_and_its_file_only_lowers() {
    let h = Harness::new().await;
    let global = OperatorSettings {
        autonomy_max: AutonomyPatch::parse_list("tools=guarded").unwrap(),
        ..OperatorSettings::default()
    };
    set(&h, None, global).await.unwrap();
    let overlay = OperatorSettings {
        autonomy_max: AutonomyPatch::parse_list("merge=auto,tools=yolo").unwrap(),
        autonomy_default: AutonomyPatch::parse_list("tools=yolo").unwrap(),
        ..OperatorSettings::default()
    };
    set(&h, repo(), overlay).await.unwrap();
    h.repo_config("[autonomy]\nmax = { merge = \"manual\" }\n");

    let seen = view(&h, repo()).await;
    let policy = seen.effective.autonomy;
    assert_eq!(
        policy.ceiling.tools,
        ToolsMode::Yolo,
        "the repo overlay raised it"
    );
    assert_eq!(policy.default.tools, ToolsMode::Yolo);
    assert_eq!(
        policy.ceiling.merge,
        MergeMode::Manual,
        "the file cannot be outvoted"
    );
    assert_eq!(
        seen.effective.sources["autonomy.max.merge"],
        SettingSource::RepoFile
    );
    assert_eq!(
        seen.effective.sources["autonomy.max.tools"],
        SettingSource::Repo
    );
    assert_eq!(
        view(&h, None).await.effective.autonomy.ceiling.tools,
        ToolsMode::Guarded
    );

    let Ok(Reply::Autonomy(autonomy)) = h.cli(Request::Autonomy { repo: repo() }).await else {
        panic!("autonomy");
    };
    assert_eq!(autonomy.policy, policy, "the autonomy view agrees");
}

#[tokio::test]
async fn budgets_switch_on_per_repository_while_off_globally() {
    let h = Harness::new().await;
    let off = OperatorSettings {
        budgets_enabled: Some(false),
        ..OperatorSettings::default()
    };
    set(&h, None, off).await.unwrap();
    let reply = h
        .cli(enqueue(Some(common::PIPELINE), Some(1.0)))
        .await
        .unwrap();
    assert!(matches!(reply, Reply::ItemBudgetsOff(_)), "{reply:?}");

    let on = OperatorSettings {
        budgets_enabled: Some(true),
        budget_usd_per_item: Some(2.0),
        ..OperatorSettings::default()
    };
    set(&h, repo(), on).await.unwrap();
    let reply = h
        .cli(enqueue(Some(common::PIPELINE), Some(1.0)))
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Item(_)), "{reply:?}");
    let seen = view(&h, repo()).await;
    assert!(seen.effective.caps.budgets_enabled);
    assert_eq!(
        seen.effective.sources["caps.budgets_enabled"],
        SettingSource::Repo
    );
    assert!(!view(&h, None).await.effective.caps.budgets_enabled);
}

#[tokio::test]
async fn the_repository_default_pipeline_is_used_at_enqueue() {
    let h = Harness::new().await;
    let overlay = OperatorSettings {
        default_pipeline: Some("no-such-pipeline".into()),
        ..OperatorSettings::default()
    };
    set(&h, repo(), overlay).await.unwrap();
    let refused = h.cli(enqueue(None, None)).await.unwrap_err();
    assert!(
        refused.to_string().contains("no-such-pipeline"),
        "{refused}"
    );
    h.cli(Request::ResetSettings { repo: repo() })
        .await
        .unwrap();
    let Reply::Item(item) = h.cli(enqueue(None, None)).await.unwrap() else {
        panic!("enqueue");
    };
    assert_eq!(
        item.pipeline,
        common::PIPELINE,
        "the registry default again"
    );
}

#[tokio::test]
async fn invalid_or_unauthorized_settings_change_nothing() {
    let h = Harness::new().await;
    let daily = OperatorSettings {
        budget_usd_daily: Some(1.0),
        ..OperatorSettings::default()
    };
    let refused = set(&h, repo(), daily).await.unwrap_err();
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    let full_auto = OperatorSettings {
        full_auto: Some(true),
        ..OperatorSettings::default()
    };
    let refused = set(&h, None, full_auto).await.unwrap_err();
    assert!(refused.to_string().contains("full_auto"), "{refused}");
    let missing = set(
        &h,
        Some(RepoRef("nope".into())),
        OperatorSettings::default(),
    )
    .await;
    assert!(
        matches!(missing, Err(ArchonError::NotFound(_))),
        "{missing:?}"
    );
    let loosen = Request::SetSettings {
        repo: None,
        settings: OperatorSettings {
            budgets_enabled: Some(false),
            ..OperatorSettings::default()
        },
    };
    let refused = h.orchestrator(loosen).await.unwrap_err();
    assert!(matches!(refused, ArchonError::Forbidden(_)), "{refused:?}");
    assert!(view(&h, None).await.scoped.is_empty());
    assert!(view(&h, repo()).await.scoped.is_empty());
    assert!(
        h.orchestrator(Request::Settings { repo: repo() })
            .await
            .is_ok(),
        "reading is open"
    );
}
