//! `GetStage`: the stage runs of an item's latest run or one it names, one
//! stage's output whole and paged, the errors that name what exists, and
//! the clipped `item_finished` summary that points to it.

use crate::common;

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{ClientKind, Reply, Request, StageDetail, StageQuery};
use archon_core::run::StageOutcome;
use archon_daemon::handlers;
use archon_daemon::triggers::{self, TriggerEvent};
use common::{Harness, completed, verdict};
use serde_json::json;

fn query(item: &QueueItemId, stage: Option<&str>) -> StageQuery {
    StageQuery {
        item: item.clone(),
        run: None,
        stage: stage.map(str::to_owned),
        attempt: None,
        slot: None,
        offset: 0,
        limit: None,
    }
}

async fn stages(h: &Harness, query: StageQuery) -> StageDetail {
    match h.cli(Request::GetStage(query)).await {
        Ok(Reply::StageDetail(detail)) => *detail,
        other => panic!("get_stage replied {other:?}"),
    }
}

async fn not_found(h: &Harness, query: StageQuery) -> String {
    match h.cli(Request::GetStage(query)).await {
        Err(ArchonError::NotFound(what)) => what,
        other => panic!("expected not found, got {other:?}"),
    }
}

/// Implement once with `summary`, fail review in slot 0 with findings, and
/// start implement attempt 2.
async fn reviewed_once(h: &Harness, summary: &str) -> QueueItemId {
    let item = h.enqueue("add a greeting").await;
    h.dispatch().await;
    h.settle_running(|_| completed(summary)).await;
    h.dispatch().await;
    h.settle_running(|s| match s.fan_index {
        0 => StageOutcome::Completed {
            summary: "missing tests\nVERDICT: fail".into(),
            structured: Some(json!({"verdict": "fail", "findings": [
                {"path": "src/greet.rs", "line": 12, "note": "no test"}
            ]})),
        },
        _ => verdict(true),
    })
    .await;
    h.dispatch().await;
    item.id
}

#[tokio::test]
async fn unknown_items_stages_attempts_and_slots_name_what_exists() {
    let h = Harness::new().await;
    let unknown = not_found(&h, query(&QueueItemId::new(), None)).await;
    assert!(unknown.contains("queue item"), "{unknown}");
    let item = h.enqueue("not yet").await;
    let unstarted = not_found(&h, query(&item.id, None)).await;
    assert!(unstarted.contains("not started"), "{unstarted}");

    h.dispatch().await;
    let stage = not_found(&h, query(&item.id, Some("review"))).await;
    assert!(
        stage.contains("\"review\"") && stage.contains("stages that ran: implement"),
        "{stage}"
    );
    let attempt = StageQuery {
        attempt: Some(5),
        ..query(&item.id, Some("implement"))
    };
    let attempt = not_found(&h, attempt).await;
    assert!(
        attempt.contains("attempt 5") && attempt.contains("attempts: 1"),
        "{attempt}"
    );
    let slot = StageQuery {
        slot: Some(3),
        ..query(&item.id, Some("implement"))
    };
    let slot = not_found(&h, slot).await;
    assert!(
        slot.contains("slot 3") && slot.contains("slots: 0"),
        "{slot}"
    );
    let stageless = StageQuery {
        attempt: Some(1),
        ..query(&item.id, None)
    };
    let refused = h.cli(Request::GetStage(stageless)).await;
    assert!(
        matches!(&refused, Err(ArchonError::Invalid(why)) if why.contains("needs `stage`")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn the_listing_shows_every_stage_run_of_the_latest_run_without_output() {
    let h = Harness::new().await;
    let item = reviewed_once(&h, "implemented").await;
    let request = Request::GetStage(query(&item, None));
    let reply = handlers::handle(&h.ctx, ClientKind::ArchonMcp, request).await;
    let Ok(Reply::StageDetail(listing)) = reply else {
        panic!("the orchestrator may list stages: {reply:?}");
    };
    let runs: Vec<(&str, u32, u32)> = listing
        .stages
        .iter()
        .map(|s| (s.stage_id.as_str(), s.attempt, s.slot))
        .collect();
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert_eq!(runs[0], ("implement", 1, 0));
    assert!(runs.contains(&("review", 1, 0)) && runs.contains(&("review", 1, 1)));
    assert_eq!(runs[3], ("implement", 2, 0));
    assert!(listing.stages.iter().all(|s| s.output.is_none()));
    assert_eq!(listing.run, h.run(&item).await.id);
    assert_eq!(listing.stage, None);
}

#[tokio::test]
async fn a_stage_shows_its_whole_summary_and_structured_result() {
    let h = Harness::new().await;
    let long = format!("{}THE END", "implemented step. ".repeat(400));
    let item = reviewed_once(&h, &long).await;

    let latest = stages(&h, query(&item, Some("implement"))).await;
    assert_eq!(latest.stages.len(), 1);
    assert_eq!(latest.stages[0].attempt, 2, "the latest attempt by default");
    let running = &latest.stages[0].output.as_ref().expect("output").text;
    assert!(running.contains("summary:\n(none)"), "{running}");

    let first = StageQuery {
        attempt: Some(1),
        ..query(&item, Some("implement"))
    };
    let first = stages(&h, first).await;
    let output = first.stages[0].output.as_ref().expect("output");
    assert!(output.text.contains(&long), "the summary is whole");
    assert_eq!(output.next_offset(), None);

    let review = stages(&h, query(&item, Some("review"))).await;
    assert_eq!(review.stages.len(), 2, "every slot of the attempt");
    let failing = review.stages.iter().find(|s| s.slot == 0).expect("slot 0");
    let text = &failing.output.as_ref().expect("output").text;
    assert!(text.contains("summary:\nmissing tests"), "{text}");
    assert!(
        text.contains("structured:\n{\n") && text.contains("\"path\": \"src/greet.rs\""),
        "pretty-printed findings: {text}"
    );
    let one = StageQuery {
        slot: Some(1),
        ..query(&item, Some("review"))
    };
    assert_eq!(stages(&h, one).await.stages.len(), 1);
}

#[tokio::test]
async fn long_output_is_paged_without_losing_a_character() {
    let h = Harness::new().await;
    let long = "é".repeat(2_500);
    let item = reviewed_once(&h, &long).await;
    let mut offset = 0;
    let mut read = String::new();
    loop {
        let page = StageQuery {
            attempt: Some(1),
            offset,
            limit: Some(1_000),
            ..query(&item, Some("implement"))
        };
        let detail = stages(&h, page).await;
        let page = detail.stages[0].output.clone().expect("output");
        assert!(page.text.chars().count() <= 1_000);
        assert_eq!(page.offset, offset);
        read.push_str(&page.text);
        match page.next_offset() {
            Some(next) => offset = next,
            None => break,
        }
    }
    let whole = StageQuery {
        attempt: Some(1),
        ..query(&item, Some("implement"))
    };
    let whole = stages(&h, whole).await.stages[0]
        .output
        .clone()
        .expect("output");
    assert_eq!(read, whole.text, "the pages add up to the whole");
    assert!(read.contains(&long) && whole.next_offset().is_none());
}

#[tokio::test]
async fn a_named_run_is_read_instead_of_the_latest() {
    let h = Harness::new().await;
    let item = h.enqueue("try twice").await;
    h.dispatch().await;
    h.settle_running(|_| completed("the first run's work"))
        .await;
    let first = h.run(&item.id).await;
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: None,
    };
    h.cli(cancel).await.expect("cancel");
    let retry = Request::RetryItem {
        item: item.id.clone(),
        model_plan: None,
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    h.cli(retry).await.expect("retry");
    h.dispatch().await;

    let latest = stages(&h, query(&item.id, Some("implement"))).await;
    assert_ne!(latest.run, first.id, "the retry started a new run");
    let text = &latest.stages[0].output.as_ref().expect("output").text;
    assert!(!text.contains("the first run's work"), "{text}");
    let older = StageQuery {
        run: Some(first.id.clone()),
        ..query(&item.id, Some("implement"))
    };
    let older = stages(&h, older).await;
    assert_eq!((older.run, older.stages.len()), (first.id.clone(), 1));
    let text = &older.stages[0].output.as_ref().expect("output").text;
    assert!(text.contains("the first run's work"), "{text}");

    let other = h.enqueue("someone else").await;
    let foreign = StageQuery {
        run: Some(first.id.clone()),
        ..query(&other.id, None)
    };
    let foreign = not_found(&h, foreign).await;
    assert!(
        foreign.contains(&first.id.to_string()) && foreign.contains("none yet"),
        "{foreign}"
    );
}

fn finished_summary(news: &[TriggerEvent]) -> String {
    news.iter()
        .find_map(|event| match event {
            TriggerEvent::ItemFinished { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("item_finished")
}

#[tokio::test]
async fn a_long_failure_points_to_get_stage_for_its_run() {
    let h = Harness::new().await;
    let item = h.enqueue("long failure").await;
    h.dispatch().await;
    let error = "an error worth reading in full. ".repeat(200);
    h.settle_running(|_| StageOutcome::Failed {
        error: error.clone(),
        infra: false,
    })
    .await;
    assert_eq!(h.state(&item.id).await.kind(), "failed");
    let run = h.run(&item.id).await;
    let summary = finished_summary(&triggers::held(&h.ctx).await.unwrap());
    assert!(summary.len() < error.len(), "the trigger is bounded");
    let hint = format!("get_stage with item {}, run {}", item.id, run.id);
    assert!(
        summary.contains(&hint) && summary.contains("clipped"),
        "{summary}"
    );
}

#[tokio::test]
async fn a_long_cancel_reason_is_clipped_without_a_get_stage_hint() {
    let h = Harness::new().await;
    let item = h.enqueue("long goodbye").await;
    let reason = "a reason worth reading in full. ".repeat(200);
    let cancel = Request::CancelItem {
        item: item.id.clone(),
        reason: Some(reason.clone()),
    };
    h.cli(cancel).await.expect("cancel");
    let summary = finished_summary(&triggers::held(&h.ctx).await.unwrap());
    assert!(summary.len() < reason.len(), "the trigger is bounded");
    assert!(
        summary.chars().count() > 3_900,
        "about 4000 characters are kept"
    );
    assert!(
        summary.ends_with(" chars in full]") && !summary.contains("get_stage"),
        "a cancelled item has no stage output to point to: {summary}"
    );
    let outcomes = h.notices(NotificationKind::Outcome).await;
    assert!(
        outcomes.iter().all(|o| o.chars().count() <= 2_000),
        "{outcomes:?}"
    );
}
