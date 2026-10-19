//! The risk label of every pull request archond delivers (ARCHITECTURE
//! §8.7): the repository gets the three labels it lacks, the pull request
//! the level its stages settled on and a "Risk" section at the top of its
//! body, and the item records the level. Strictly advisory: a label GitHub
//! refuses is told, and the delivery still succeeds.

use crate::common;

use archon_core::model::{Effort, ModelPlan};
use archon_core::ports::resumed_reply_outcome;
use archon_core::queue::{QueueItem, QueueState};
use archon_core::risk::RiskLevel;
use archon_core::run::{StageOutcome, StageRun};
use archon_daemon::executor;
use common::delivery::{FakeGh, add_origin, change, shipped_by};
use common::{Harness, MODEL, REVIEW_MODEL, REVIEWED, REVIEWED_TOML, choice};
use serde_json::{Value, json};

const PR_ONE: &str = "https://github.com/o/r/pull/1";
const START: &str = "<!-- archon-risk -->";

async fn delivering() -> (Harness, FakeGh) {
    let h = Harness::new().await;
    h.add_pipeline(REVIEWED, REVIEWED_TOML);
    add_origin(&h);
    let gh = FakeGh::install(&h);
    (h, gh)
}

/// A `direct` item whose one agent commits a change and completes with its
/// ship record plus `extra`; the dispatcher then delivers it.
async fn delivered(h: &Harness, extra: Value) -> QueueItem {
    let item = h
        .enqueue_in("direct", "a risky change", ModelPlan::default())
        .await
        .expect("enqueue");
    h.dispatch().await;
    let stage = h.running().await.remove(0);
    let outcome = shipped_by(h, &stage, "Changed it; cargo test passes", extra).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    item
}

async fn summary(h: &Harness, item: &QueueItem) -> String {
    match h.state(&item.id).await {
        QueueState::Succeeded { summary, .. } => summary,
        other => panic!("the item ships: {other:?}"),
    }
}

async fn recorded(h: &Harness, item: &QueueItem) -> Option<RiskLevel> {
    h.ctx.store.lock().await.item(&item.id).expect("item").risk
}

#[tokio::test]
async fn a_delivery_creates_the_missing_labels_and_labels_its_pull_request_with_evidence() {
    let (h, gh) = delivering().await;
    let risk = json!({"risk": {
        "level": "medium",
        "why": "Adds a flag behind the existing CLI; cargo test passes.",
        "captures": ["https://example.com/after.png", "/home/me/before.png"],
    }});
    let item = delivered(&h, risk).await;

    let summary = summary(&h, &item).await;
    assert!(
        summary.contains("RISK: medium, labelled `risk: medium`"),
        "{summary}"
    );
    assert!(
        summary
            .lines()
            .last()
            .unwrap()
            .starts_with(&format!("SHIPPED: {PR_ONE}"))
    );
    let created = gh.created_labels();
    assert_eq!(created.len(), 3, "{created:?}");
    for (level, colour) in [("low", "0e8a16"), ("medium", "fbca04"), ("high", "d73a4a")] {
        let spec = format!("risk: {level}|{colour}|");
        assert!(
            created.iter().any(|c| c.starts_with(&spec)),
            "{spec} in {created:?}"
        );
    }
    let (labels, body) = gh.pr_labels(1);
    assert_eq!(labels, ["risk: medium"]);
    assert!(
        body.starts_with(&format!("{START}\n## Risk: medium\n")),
        "{body}"
    );
    for said in [
        "Needs at least one human reviewer; agents must not merge it.",
        "`implement` proposed `medium`; no reviewer judged the level.",
        "Adds a flag behind the existing CLI; cargo test passes.",
        "Diff: 1 file(s), +1 −0",
        "Files: `change.txt`.",
        "Kind: touches code beyond tests and docs, by path.",
        "Checks run: `cargo test`",
        "Label: `risk: medium` is set.",
        "![capture 1](https://example.com/after.png)",
        "Why, and how it was verified.",
    ] {
        assert!(body.contains(said), "missing {said:?} in {body}");
    }
    assert!(!body.contains("before.png"), "never a local path: {body}");
    assert_eq!(recorded(&h, &item).await, Some(RiskLevel::Medium));
    assert_eq!(gh.count("pr merge"), 0, "the label merges nothing");
}

#[tokio::test]
async fn without_a_proposal_the_pull_request_is_labelled_high_with_the_labels_it_has() {
    let (h, gh) = delivering().await;
    gh.add_labels(&["risk: low", "risk: medium", "risk: high"]);
    let item = delivered(&h, json!({})).await;

    summary(&h, &item).await;
    assert_eq!(gh.count("label create"), 0, "{:?}", gh.calls());
    let (labels, body) = gh.pr_labels(1);
    assert_eq!(labels, ["risk: high"]);
    assert!(
        body.contains("proposed no valid level (no level was proposed)"),
        "{body}"
    );
    assert!(
        body.contains("Needs at least two human reviewers"),
        "{body}"
    );
    assert_eq!(recorded(&h, &item).await, Some(RiskLevel::High));
}

#[tokio::test]
async fn a_label_github_refuses_to_create_is_told_and_the_delivery_still_succeeds() {
    let (h, gh) = delivering().await;
    gh.refuse_labels("HTTP 403: Resource not accessible by integration");
    let risk = json!({"risk": {"level": "low", "why": "Only docs."}});
    let item = delivered(&h, risk).await;

    let summary = summary(&h, &item).await;
    assert!(
        summary.contains("RISK: low; the label could not be set"),
        "{summary}"
    );
    assert!(summary.contains("HTTP 403"), "{summary}");
    assert!(summary.ends_with(&format!("(commit {})", git_head(&h, &item).await)));
    let (labels, body) = gh.pr_labels(1);
    assert!(labels.is_empty(), "{labels:?}");
    assert!(
        body.starts_with(&format!("{START}\n## Risk: low\n")),
        "{body}"
    );
    assert!(
        body.contains("`risk: low` could not be set, so set it by hand"),
        "{body}"
    );
    assert_eq!(recorded(&h, &item).await, Some(RiskLevel::Low));
}

/// Labels it cannot see could be lowered or doubled: none is set blind.
#[tokio::test]
async fn a_pull_request_that_cannot_be_read_is_not_labelled_blind() {
    let (h, gh) = delivering().await;
    gh.fail_next("pr view", "HTTP 502: Bad Gateway");
    let item = delivered(&h, json!({"risk": {"level": "low", "why": "Only docs."}})).await;

    let summary = summary(&h, &item).await;
    assert!(
        summary
            .contains("RISK: low; the label could not be set: the pull request could not be read"),
        "{summary}"
    );
    assert!(
        summary.contains("risk section could not be written"),
        "{summary}"
    );
    let calls = gh.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("pr edit") || c.starts_with("label ")),
        "{calls:?}"
    );
    assert_eq!(gh.pr_labels(1).0, Vec::<String>::new());
}

async fn git_head(h: &Harness, item: &QueueItem) -> String {
    let run = h.run(&item.id).await;
    common::git(&h.repo, &["rev-parse", &run.branch])
}

fn fix_plan() -> ModelPlan {
    let mut plan = ModelPlan::default();
    plan.0
        .insert("implement".into(), vec![choice(MODEL, Effort::Medium)]);
    plan.0
        .insert("review".into(), vec![choice(REVIEW_MODEL, Effort::Medium)]);
    plan
}

/// The running review stage settles with `verdict` and a risk call of `level`.
async fn judged(h: &Harness, verdict: &str, level: &str) {
    let outcome = StageOutcome::Completed {
        summary: format!("judged\nVERDICT: {verdict}"),
        structured: Some(json!({
            "verdict": verdict,
            "findings": [],
            "risk": {"level": level, "why": "It reaches into the ship machinery."},
        })),
    };
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
}

/// The kept implementer answers its fix with a new record proposing `level`.
async fn fixes(h: &Harness, fix: &StageRun, level: &str) {
    let run = h.ctx.store.lock().await.run(&fix.run).expect("run");
    let mut record = change(&run.workspace);
    record["risk"] = json!({"level": level, "why": "The fix adds a test."});
    let outcome = resumed_reply_outcome(&format!("fixed\n\n```json\n{record}\n```"));
    let agent = fix.aether_agent.clone().expect("the kept agent");
    h.backend.answer(&agent, outcome);
    executor::turn_ended(&h.ctx, &agent)
        .await
        .expect("turn end");
    h.dispatch().await;
}

/// Through a fix loop, the level is the one proposed with the last fix, and
/// a later review stage's call stands over it.
#[tokio::test]
async fn through_a_fix_loop_the_last_proposal_counts_and_the_passing_review_changes_it() {
    let (h, gh) = delivering().await;
    let item = h.enqueue_in(REVIEWED, "fix it", fix_plan()).await.unwrap();
    h.dispatch().await;
    let first = h.running().await.remove(0);
    let low = json!({"risk": {"level": "low", "why": "A typo."}});
    let outcome = shipped_by(&h, &first, "first try", low).await;
    h.settle_running(|_| outcome.clone()).await;
    h.dispatch().await;
    judged(&h, "fail", "low").await;
    let fix = h.running().await.remove(0);
    assert_eq!((fix.stage_id.as_str(), fix.attempt), ("implement", 2));
    fixes(&h, &fix, "medium").await;
    judged(&h, "pass", "high").await;

    let summary = summary(&h, &item).await;
    assert!(summary.contains("RISK: high"), "{summary}");
    let (labels, body) = gh.pr_labels(1);
    assert_eq!(labels, ["risk: high"]);
    let decided = "`implement` proposed `medium`; stage `review` changed it to `high`.";
    assert!(body.contains(decided), "{body}");
    assert!(body.contains("The fix adds a test."), "{body}");
    assert!(
        body.contains("changing it: It reaches into the ship machinery."),
        "{body}"
    );
    assert!(
        !body.contains("A typo."),
        "the first attempt's call is gone: {body}"
    );
    assert_eq!(recorded(&h, &item).await, Some(RiskLevel::High));
}
