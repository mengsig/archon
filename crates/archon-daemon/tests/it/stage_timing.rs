use std::time::Duration;

use archon_core::id::{QueueItemId, StageRunId};
use archon_core::protocol::{ItemPatch, Reply, Request, RunPlan};
use archon_core::run::{StageRun, StageState};
use archon_core::settings::SettingSource;
use archon_daemon::executor;
use archon_daemon::triggers::{self, TriggerEvent};

use crate::common::{Harness, expire_deadline, stored_deadline};

#[tokio::test]
async fn overrun_news_is_queued_once_per_running_attempt() {
    let mut h = Harness::new().await;
    seed_history(&h, 1).await;
    let item = h.enqueue("overrun").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    assert!(overruns(&h).await.is_empty());

    wait_for_overrun(&stage).await;
    h.dispatch().await;
    let first = overruns(&h).await;
    assert!(matches!(&first[..], [TriggerEvent::StageOverrun {
        item: id, stage: key, stage_run, elapsed_seconds, p90_seconds: 1,
    }] if id == &item.id && key == "implement" && stage_run == &stage.id && *elapsed_seconds > 2));
    assert!(
        triggers::due(&h.ctx).await.unwrap(),
        "the alert wakes delivery"
    );
    assert_eq!(h.running().await[0].id, stage.id, "the alert is advisory");

    h.restart();
    executor::recover(&h.ctx).await.unwrap();
    h.dispatch().await;
    assert_eq!(overruns(&h).await, first, "the receipt survives restart");
}

#[tokio::test]
async fn an_expired_running_stage_still_queues_its_overrun() {
    let h = Harness::new().await;
    seed_history(&h, 1).await;
    let item = h.enqueue("overrun past deadline").await;
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    wait_for_overrun(&stage).await;
    expire_deadline(&h, &stage).await;
    assert!(overruns(&h).await.is_empty());

    h.dispatch().await;
    let first = overruns(&h).await;
    assert!(matches!(&first[..], [TriggerEvent::StageOverrun {
        item: id, stage_run, elapsed_seconds, p90_seconds: 1, ..
    }] if id == &item.id && stage_run == &stage.id && *elapsed_seconds > 2));
    assert!(h.stages(&item.id).await[0].state.is_terminal());
    h.dispatch().await;
    assert_eq!(overruns(&h).await, first);
}

async fn wait_for_overrun(stage: &StageRun) {
    assert_eq!(stage.state, StageState::Running);
    let threshold = stage.started_at + chrono::Duration::seconds(2);
    if let Ok(wait) = (threshold - archon_core::now()).to_std() {
        assert!(wait <= Duration::from_secs(2));
        tokio::time::sleep(wait + Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn learned_timeouts_are_opt_in_and_only_apply_to_new_attempts() {
    let mut h = Harness::new().await;
    seed_history(&h, 720).await;
    let old = start_with_override(&h).await;
    let configured = old.started_at + chrono::Duration::minutes(120);
    assert_eq!(stored_deadline(&h, &old).await, Some(configured));
    let old_run = h.ctx.store.lock().await.run(&old.run).unwrap();
    assert!(
        read_plan(&h, &old).await.settings[0]
            .learned_timeouts
            .is_empty()
    );
    assert_timeout_setting(&h, false, SettingSource::Builtin).await;

    let mut config = h.ctx.config.clone();
    config.limits.learned_stage_timeouts = Some(true);
    h.restart_with(config);
    executor::recover(&h.ctx).await.unwrap();
    h.dispatch().await;
    assert_eq!(stored_deadline(&h, &old).await, Some(configured));
    assert_timeout_setting(&h, true, SettingSource::Config).await;
    cancel(&h, old_run.item).await;

    let new = start_with_override(&h).await;
    assert_eq!(
        stored_deadline(&h, &new).await,
        Some(new.started_at + chrono::Duration::minutes(36))
    );
    let request = h.backend.requests().pop().unwrap();
    assert_eq!(request.deadline, stored_deadline(&h, &new).await);
    assert!(request.instructions.contains("36 minutes after your start"));
    let plan = read_plan(&h, &new).await;
    let setting = &plan.settings[0];
    assert_eq!(setting.timeout_minutes, Some(120));
    let [learned] = &setting.learned_timeouts[..] else {
        panic!("expected one learned timeout: {setting:?}");
    };
    assert_eq!(
        (
            learned.fan_index,
            learned.timeout_minutes,
            learned.p90_seconds
        ),
        (0, 36, 720)
    );
}

async fn assert_timeout_setting(h: &Harness, enabled: bool, source: SettingSource) {
    let Reply::Settings(settings) = h.cli(Request::Settings { repo: None }).await.unwrap() else {
        panic!("settings reply");
    };
    assert_eq!(settings.effective.learned_stage_timeouts, enabled);
    assert_eq!(settings.effective.sources["learned_stage_timeouts"], source);
}

async fn seed_history(h: &Harness, seconds: i64) {
    let item = h.enqueue("timing history").await;
    h.dispatch().await;
    let template = h.running().await.remove(0);
    let finished = template.started_at;
    {
        let mut store = h.ctx.store.lock().await;
        for fan_index in 1..=10 {
            let sample = StageRun {
                id: StageRunId::new(),
                fan_index,
                aether_agent: None,
                aether_task: None,
                state: StageState::Completed,
                started_at: finished - chrono::Duration::seconds(seconds),
                finished_at: Some(finished),
                ..template.clone()
            };
            store.upsert_stage_run(&sample).unwrap();
        }
        let run = store.run(&template.run).unwrap();
        assert_eq!(run.item, item.id);
        assert_eq!(
            store
                .stage_p90_seconds(&run.pipeline, &template.stage_id, template.choice.effort)
                .unwrap(),
            Some(u64::try_from(seconds).unwrap())
        );
    }
    cancel(h, item.id).await;
}

async fn start_with_override(h: &Harness) -> StageRun {
    let item = h.enqueue("stage with timeout override").await;
    h.cli(Request::UpdateItem {
        item: item.id.clone(),
        patch: ItemPatch {
            stage_timeouts: [("implement".to_owned(), 120)].into(),
            ..ItemPatch::default()
        },
    })
    .await
    .unwrap();
    h.dispatch().await;
    let stages = h.running().await;
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].run, h.run(&item.id).await.id);
    stages.into_iter().next().unwrap()
}

async fn cancel(h: &Harness, item: QueueItemId) {
    h.cli(Request::CancelItem { item, reason: None })
        .await
        .unwrap();
}

async fn overruns(h: &Harness) -> Vec<TriggerEvent> {
    triggers::held(&h.ctx)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event, TriggerEvent::StageOverrun { .. }))
        .collect()
}

async fn read_plan(h: &Harness, stage: &StageRun) -> RunPlan {
    let Reply::RunPlan(plan) = h
        .cli(Request::RunPlan {
            run: stage.run.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("run plan reply");
    };
    assert_eq!(plan.run, stage.run);
    assert_eq!(plan.settings[0].key, stage.stage_id);
    *plan
}
