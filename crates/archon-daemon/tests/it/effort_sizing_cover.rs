//! Effort sizer coverage of acceptance checks 2, 3, 5 and 7 (A11), on the
//! helpers of [`crate::effort_sizing`].
#![allow(unused_imports)]

use crate::effort_sizing::*;
use archon_core::config::Config;
use archon_core::effort_sizing::RiskClass;
use archon_core::model::Effort::{Low, Medium};
use archon_core::protocol::{Reply, Request};
use archon_core::queue::QueueItem;
use archon_core::steer::{InsertStage, Steer};

use archon_core::outcome::{DiffSize, DiffStat, Outcome, OutcomeResult};
use archon_core::run::PipelineRun;

/// Record, for the item's only run, an outcome that measured a large diff.
async fn record_large_diff(h: &Harness, queued: &QueueItem) -> PipelineRun {
    let run = h.run(&queued.id).await;
    let outcome = Outcome {
        run: run.id.clone(),
        item: queued.id.clone(),
        repo: queued.repo.clone(),
        pipeline: SIZED.into(),
        diff: Some(DiffStat {
            files: 40,
            added: 900,
            deleted: 100,
        }),
        stages: Vec::new(),
        review_rounds: 0,
        result: OutcomeResult::Cancelled,
        failure: None,
        failure_source: None,
        review_pr: None,
        corrections: Vec::new(),
        feedback: Vec::new(),
        context: None,
        merged: None,
        cost_usd: 0.0,
        cost_provisional: true,
        count_estimates: true,
        wall_secs: 1,
        finished_at: run.finished_at.expect("a cancelled run finished"),
    };
    h.ctx.store.lock().await.record_outcome(&outcome).unwrap();
    let measured = h.ctx.store.lock().await.outcome(&run.id).unwrap();
    assert_eq!(measured.and_then(|o| o.size()), Some(DiffSize::Large));
    run
}

/// Check 2: a retry carries the frozen sizing; a diff size measured by the
/// item's own run changes nothing, though a plan sized first would read it.
#[tokio::test]
async fn a_retry_ignores_the_diff_size_its_run_measured() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    h.dispatch().await;
    cancel(&h, &queued.id).await;
    record_large_diff(&h, &queued).await;

    let retried = retry(&h, &queued.id).await.unwrap();
    assert_eq!(efforts(&retried), efforts(&queued));
    assert_eq!(class(&retried), Some(RiskClass::Ordinary));
    for key in ["implement", "review", "ship"] {
        assert_eq!(effort_source(&retried, key), effort_source(&queued, key));
        assert_eq!(reason(&retried, key), reason(&queued, key));
    }
    assert_eq!(retried.model_plan, queued.model_plan, "models and sizing");
    assert_eq!(retried.model_plan.sizing().unwrap().size, None);
}

/// Check 2, the other side: the measured size does count for a plan sized
/// for the first time, so the test above is not vacuous.
#[tokio::test]
async fn a_plan_sized_for_the_first_time_reads_the_size_its_run_measured() {
    let h = harness(Config::default()).await;
    let queued = enqueue(&h, "Add a button", &[]).await;
    h.dispatch().await;
    cancel(&h, &queued.id).await;
    record_large_diff(&h, &queued).await;

    let request = Request::RetryItem {
        item: queued.id.clone(),
        model_plan: Some(models()),
        model_reasons: None,
        fresh: false,
        brief_append: None,
        stage_timeouts: Default::default(),
    };
    let Reply::Item(replanned) = h.cli(request).await.unwrap() else {
        panic!("retry replied");
    };
    assert_eq!(
        replanned.model_plan.sizing().unwrap().size,
        Some(DiffSize::Large)
    );
    assert_eq!(class(&replanned), Some(RiskClass::Elevated));
}

const CHECKER: &str = "sized-checker";
const CHECKER_TOML: &str = r#"[pipeline]
name = "sized-checker"
description = "a review stage to borrow"
version = 1
workspace = "worktree"
requires_repo = true

[[stage]]
id = "audit"
kind = "review"
prompt = "implement.md"
effort = "low"
gate = "all_pass"
timeout_minutes = 30
"#;

/// Check 3, stage kind: a review stage inserted after a work stage is sized
/// from its own default (low, not implement's medium) and records that it checks.
#[tokio::test]
async fn an_inserted_review_stage_is_sized_and_recorded_with_its_own_kind() {
    let h = harness(Config::default()).await;
    h.add_pipeline(CHECKER, CHECKER_TOML);
    let queued = enqueue(&h, "Add a button", &[]).await;
    let insert = Steer {
        insert_stage: Some(InsertStage {
            stage: "audit".into(),
            from: Some(CHECKER.into()),
            after: Some("implement".into()),
        }),
        ..Steer::default()
    };
    let added = steer(&h, &queued.id, insert).await;
    let all = efforts(&added);
    let carried: Vec<_> = all.iter().filter(|(key, _)| *key != "audit").collect();
    assert_eq!(carried, efforts(&queued).iter().collect::<Vec<_>>());
    let audit = all.iter().find(|(key, _)| *key == "audit");
    assert_eq!(audit, Some(&("audit", Low)), "its own default, not medium");
    assert_eq!(reason(&added, "audit"), "sizer: ordinary -> low");
    let sizing = added.model_plan.sizing().unwrap();
    let facts = sizing.keys["audit"].facts;
    assert_eq!((facts.base, facts.checks), (Low, true));
    let donor = sizing.keys["implement"].facts;
    assert_eq!((donor.base, donor.checks), (Medium, false));
}

/// Check 7, the unnamed-effort boundary: a draft naming models only starts at
/// each stage's default; over a lower policy cap, sizing off refuses it as
/// 1.6.3 does, and sizing on lowers it within the cap, with the reason.
#[tokio::test]
async fn an_unnamed_effort_over_the_policy_cap_is_refused_off_and_capped_on() {
    let off = harness(sizing_off()).await;
    off.repo_config("[models]\nmax_effort = \"low\"\n");
    let refused = off
        .cli(Request::Enqueue(request("Add a button", &[], models())))
        .await
        .expect_err("a default above the cap is refused with sizing off");
    assert!(refused.to_string().contains("max_effort"), "{refused}");

    let on = harness(Config::default()).await;
    on.repo_config("[models]\nmax_effort = \"low\"\n");
    let queued = enqueue(&on, "Add a button", &[]).await;
    let all = efforts(&queued);
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|(_, effort)| *effort <= Low), "{all:?}");
    assert!(
        !reason(&queued, "implement").is_empty(),
        "the reason is kept"
    );
}
