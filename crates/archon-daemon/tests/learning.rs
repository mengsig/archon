mod common;
use archon_core::model::{Effort, ModelChoice};
use archon_core::outcome::{Outcome, OutcomeResult, OutcomeStage};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueState;
use archon_core::run::{PipelineRun, RunState};
use common::{Harness, PIPELINE, plan};

async fn samples(h: &Harness, model: &str, cost: f64, count: usize) {
    samples_of(h, &common::choice(model, Effort::Low), cost, count).await;
}

/// `count` finished runs of the test pipeline whose `implement` ran `choice`.
async fn samples_of(h: &Harness, choice: &ModelChoice, cost: f64, count: usize) {
    for n in 0..count {
        let item = h.enqueue(&format!("{} {n}", choice.model)).await;
        let now = archon_core::now();
        let mut store = h.ctx.store.lock().await;
        store
            .transition_item(&item.id, QueueState::Dispatching, now)
            .unwrap();
        let mut run = PipelineRun {
            id: archon_core::id::PipelineRunId::new(),
            item: item.id.clone(),
            pipeline: PIPELINE.into(),
            shipped: false,
            aether_run: None,
            workspace: h.repo.clone(),
            team_root: None,
            branch: "archon/test".into(),
            state: RunState::Running {
                stage_id: "implement".into(),
                attempt: 1,
            },
            cost_usd: cost,
            started_at: now,
            finished_at: None,
        };
        store.insert_run(&run).unwrap();
        store
            .transition_item(
                &item.id,
                QueueState::Running {
                    run: run.id.clone(),
                },
                now,
            )
            .unwrap();
        run.finish(
            RunState::Succeeded {
                summary: "done".into(),
            },
            now,
        )
        .unwrap();
        store
            .finish_run(
                &run,
                QueueState::Succeeded {
                    run: run.id.clone(),
                    summary: "done".into(),
                },
                now,
            )
            .unwrap();
        store
            .record_outcome(&sample_outcome(&run, &item, choice, cost, now))
            .unwrap();
    }
}

fn sample_outcome(
    run: &PipelineRun,
    item: &archon_core::queue::QueueItem,
    choice: &ModelChoice,
    cost: f64,
    now: archon_core::Timestamp,
) -> Outcome {
    assert_eq!(run.item, item.id);
    assert!(run.state.is_terminal());
    Outcome {
        run: run.id.clone(),
        item: item.id.clone(),
        repo: item.repo.clone(),
        pipeline: PIPELINE.into(),
        diff: None,
        stages: vec![OutcomeStage {
            stage: "implement".into(),
            choice: choice.clone(),
            cost_usd: cost,
            wall_secs: 30,
            attempts: 1,
            passed: Some(true),
        }],
        review_rounds: 0,
        result: OutcomeResult::Succeeded,
        failure: None,
        corrections: Vec::new(),
        merged: None,
        cost_usd: cost,
        wall_secs: 30,
        finished_at: now,
    }
}

#[tokio::test]
async fn seeded_outcomes_fill_the_plan_and_why_retains_sample_sizes() {
    let h = Harness::new().await;
    samples(&h, "codex/expensive", 2.0, 5).await;
    samples(&h, "codex/cheap", 0.2, 5).await;
    let mut partial = plan();
    partial.0.remove("implement");
    let item = h.enqueue_with("learned default", partial).await.unwrap();
    assert_eq!(
        item.model_plan.0["implement"][0].model.to_string(),
        "codex/cheap"
    );
    let Reply::LessonsWhy(why) = h
        .cli(Request::LessonsWhy {
            item: item.id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("why")
    };
    assert!(why.routing["implement"].contains("n=5"));
    assert!(why.routing["implement"].contains("repository evidence"));
    let explicit = h.enqueue("explicit choice").await;
    assert_eq!(explicit.model_plan, plan());
}

#[tokio::test]
async fn learned_default_respects_repository_policy_and_disabled_config() {
    let h = Harness::new().await;
    samples(&h, "codex/expensive", 2.0, 5).await;
    samples(&h, "codex/cheap", 0.2, 5).await;
    h.repo_config("[models]\ndeny = [\"codex/cheap\"]\n");
    let mut partial = plan();
    partial.0.remove("implement");
    let item = h
        .enqueue_with("permitted learned default", partial.clone())
        .await
        .unwrap();
    assert_eq!(
        item.model_plan.0["implement"][0].model.to_string(),
        "codex/expensive"
    );
    let mut config = archon_core::config::Config::default();
    config.learning.enabled = false;
    let disabled = Harness::with_config(config).await;
    samples(&disabled, "codex/cheap", 0.2, 5).await;
    assert!(
        disabled
            .ctx
            .store
            .lock()
            .await
            .outcomes(None)
            .unwrap()
            .is_empty()
    );
    let item = disabled.enqueue_with("disabled", partial).await.unwrap();
    let own = &disabled.ctx.config.orchestrator.model;
    assert_eq!(&item.model_plan.0["implement"][0].model, own, "the default");
}

/// An enqueue naming no model takes the default, which is the baseline: then
/// evidence picks a stage's model, and the saving is measured against it.
#[tokio::test]
async fn an_enqueue_naming_no_model_takes_the_default_until_evidence_picks_one() {
    let h = Harness::new().await;
    let own = h.ctx.config.orchestrator.model.clone();
    let unnamed = archon_core::model::ModelPlan::default;
    let plain = h.enqueue_with("nothing learned", unnamed()).await.unwrap();
    assert_eq!(plain.model_plan.0.len(), 3, "{:?}", plain.model_plan);
    let default = plain.model_plan.0["implement"][0].clone();
    for choices in plain.model_plan.0.values() {
        assert_eq!(
            choices,
            std::slice::from_ref(&default),
            "one default for every stage"
        );
    }
    assert_eq!(default.model, own);

    samples_of(&h, &default, 2.0, 5).await;
    samples(&h, "codex/cheap", 0.2, 5).await;
    let learned = h.enqueue_with("learned", unnamed()).await.unwrap();
    let implement = &learned.model_plan.0["implement"][0];
    assert_eq!(implement.model.to_string(), "codex/cheap");
    let review = &learned.model_plan.0["review"];
    assert_eq!(review, std::slice::from_ref(&default), "no evidence");
    let Reply::LessonStats(stats) = h.cli(Request::LessonStats { repo: None }).await.unwrap()
    else {
        panic!("stats");
    };
    assert_eq!(stats.pipelines[0].estimated_saving_usd, Some(1.8));
}

#[tokio::test]
async fn distillation_queues_once_per_window_and_never_learns_from_itself() {
    let mut config = archon_core::config::Config::default();
    config.learning.distill_every = 5;
    let h = Harness::with_config(config).await;
    samples(&h, "codex/cheap", 0.2, 5).await;
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    let items = h.ctx.store.lock().await.items(&Default::default()).unwrap();
    let errands: Vec<_> = items.iter().filter(|i| i.pipeline == "errand").collect();
    assert_eq!(errands.len(), 1);
    assert_eq!(errands[0].budget_usd, Some(1.0));
    h.dispatch().await;
    h.settle_running(|_| archon_core::run::StageOutcome::Completed {
        summary: "Distilled".into(),
        structured: Some(
            serde_json::json!({"lessons":[{"kind":"practice","text":"Run migration checks."}]}),
        ),
    })
    .await;
    archon_daemon::learned::maintenance(&h.ctx).await.unwrap();
    let store = h.ctx.store.lock().await;
    assert_eq!(store.outcomes(None).unwrap().len(), 5);
    let lessons = store.lessons(&Default::default()).unwrap();
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].state, archon_core::lesson::LessonState::Proposed);
    assert_eq!(
        store
            .items(&Default::default())
            .unwrap()
            .iter()
            .filter(|i| i.pipeline == "errand")
            .count(),
        1
    );
}

#[tokio::test]
async fn orchestrator_advice_is_frozen_per_generation_and_includes_repository_lessons() {
    let h = Harness::new().await;
    let Reply::Lesson(lesson) = h
        .cli(Request::RecordLesson(archon_core::lesson::LessonDraft {
            kind: archon_core::lesson::LessonKind::Practice,
            text: "Use the repo check command.".into(),
            repo: Some(archon_core::protocol::RepoRef(common::REPO_NAME.into())),
            pipeline: None,
            item: None,
        }))
        .await
        .unwrap()
    else {
        panic!("lesson");
    };
    let first =
        archon_daemon::learned::lessons_for_orchestrator(&*h.ctx.store.lock().await).unwrap();
    assert!(first.contains("Use the repo check command."));
    h.cli(Request::UpdateLesson {
        lesson: lesson.id,
        patch: archon_core::lesson::LessonPatch {
            text: Some("Use the new repo check command.".into()),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let mut store = h.ctx.store.lock().await;
    assert_eq!(
        archon_daemon::learned::lessons_for_orchestrator(&store).unwrap(),
        first
    );
    store
        .kv_set(archon_queue::store::keys::ORCHESTRATOR_GENERATION, "next")
        .unwrap();
    assert!(
        archon_daemon::learned::lessons_for_orchestrator(&store)
            .unwrap()
            .contains("new repo")
    );
}

#[tokio::test]
async fn escaped_lesson_text_fits_the_rendered_contract_and_orchestrator_limits() {
    use archon_core::lesson::{LessonDraft, LessonKind};
    let h = Harness::new().await;
    for n in 0..10 {
        h.cli(Request::RecordLesson(LessonDraft {
            kind: LessonKind::Practice,
            text: format!("{n} {}", "\\\"".repeat(140)),
            repo: None,
            pipeline: None,
            item: None,
        }))
        .await
        .unwrap();
    }
    let store = h.ctx.store.lock().await;
    let advice = archon_daemon::learned::lessons_for_orchestrator(&store).unwrap();
    assert!(!advice.is_empty() && advice.len() <= 4096);
    let lessons = store.select_lessons(None, Some(PIPELINE), 6, 1200).unwrap();
    assert!(!lessons.is_empty());
    let contract = archon_pipeline::contract::lesson_rules(&lessons);
    assert!(contract.len() <= 1200);
    assert!(contract.contains("never widen permissions"));
}

#[tokio::test]
async fn savings_require_a_measured_static_baseline() {
    let h = Harness::new().await;
    samples(&h, "codex/expensive", 2.0, 5).await;
    samples(&h, "codex/cheap", 0.2, 5).await;
    let store = h.ctx.store.lock().await;
    let repo = store
        .repo(&archon_core::protocol::RepoRef(common::REPO_NAME.into()))
        .unwrap();
    assert_eq!(
        store
            .estimated_model_saving(Some(&repo.id), PIPELINE, &Default::default())
            .unwrap(),
        None
    );
    let baseline = archon_core::model::ModelPlan(std::collections::BTreeMap::from([(
        "implement".into(),
        vec![ModelChoice {
            model: "codex/expensive".parse().unwrap(),
            effort: Effort::Low,
        }],
    )]));
    store
        .record_static_plan(Some(&repo.id), PIPELINE, &baseline)
        .unwrap();
    assert!(
        (store
            .estimated_model_saving(Some(&repo.id), PIPELINE, &Default::default())
            .unwrap()
            .unwrap()
            - 1.8)
            .abs()
            < 1e-9
    );
    drop(store);
    let Reply::LessonStats(stats) = h.cli(Request::LessonStats { repo: None }).await.unwrap()
    else {
        panic!("stats");
    };
    assert_eq!(stats.pipelines[0].estimated_saving_usd, Some(1.8));
}

#[tokio::test]
async fn a_known_diff_size_never_pools_evidence_from_another_size() {
    let h = Harness::new().await;
    samples(&h, "codex/large", 2.0, 5).await;
    samples(&h, "codex/small", 0.2, 5).await;
    {
        let store = h.ctx.store.lock().await;
        for mut outcome in store.outcomes(None).unwrap() {
            let added = if outcome.stages[0].choice.model.model == "large" {
                500
            } else {
                10
            };
            outcome.diff = Some(archon_core::outcome::DiffStat {
                files: 1,
                added,
                deleted: 0,
            });
            store.record_outcome(&outcome).unwrap();
        }
    }
    let mut partial = plan();
    partial.0.remove("implement");
    let request = |size: &str| {
        Request::Enqueue(archon_core::protocol::EnqueueRequest {
            repo: Some(archon_core::protocol::RepoRef(common::REPO_NAME.into())),
            pipeline: Some(PIPELINE.into()),
            title: format!("size {size}"),
            brief: "Sized task".into(),
            priority: Default::default(),
            depends_on: vec![],
            tags: vec![format!("diff:{size}")],
            model_plan: partial.clone(),
            model_reasons: None,
            budget_usd: None,
            idempotency_key: None,
            autonomy: Default::default(),
            origin: None,
        })
    };
    let Reply::Item(item) = h.cli(request("large")).await.unwrap() else {
        panic!("item");
    };
    assert_eq!(
        item.model_plan.0["implement"][0].model.to_string(),
        "codex/large"
    );
    let Reply::Item(medium) = h.cli(request("medium")).await.unwrap() else {
        panic!("item");
    };
    assert_eq!(
        medium.model_plan.0["implement"][0].model, h.ctx.config.orchestrator.model,
        "a bucket with no evidence takes the default, not another size's choice"
    );
}
