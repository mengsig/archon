//! Replays of `archon learning` through archond's handlers: one explicit
//! quality correction moves a learned default once, and nothing else is
//! penalized. Histories are dated relative to now, archond's only clock.

use crate::common;
use crate::feedback_common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::catalog::StageKind;
use archon_core::config::Config;
use archon_core::error::ArchonError;
use archon_core::feedback::Credit;
use archon_core::id::QueueItemId;
use archon_core::outcome::CorrectionCause::{self, Budget, Infrastructure, Quality, Scope};
use archon_core::protocol::{ClientKind, Reply, Request};
use archon_daemon::handlers;
use common::{Harness, plan};
use feedback_common::*;

fn config(explore_rate: f64) -> Config {
    let mut config = Config::default();
    config.learning.explore_rate = explore_rate;
    config
}

fn named(model: &str, runs: u32, passed: u32) -> Option<(String, u32, u32)> {
    Some((model.into(), runs, passed))
}

#[tokio::test]
async fn one_quality_correction_moves_the_default_once_and_undo_restores_it() {
    let mut h = Harness::with_config(config(0.0)).await;
    let cheap = two_choices(&h).await;
    assert_eq!(default_model(&h).await, "codex/cheap");

    let wrong = request(&cheap[2], "implement", Quality);
    let receipt = correct(&h, wrong.clone()).await.unwrap();
    let id = receipt.view.feedback.id.clone();
    assert!(!receipt.replayed && receipt.view.penalizing);
    assert_eq!(
        standing(receipt.view.corrected.as_ref()),
        named("cheap", 5, 4)
    );
    assert_eq!(
        standing(receipt.view.selected.as_ref()),
        named("dear", 5, 5)
    );
    let (_, model, reason) = default_route(&h).await;
    assert_eq!(model, "codex/dear");
    let reason = reason.expect("a learned default says why");
    assert!(
        reason.contains(&format!("after operator corrections {id}")),
        "{reason}"
    );

    let again = correct(&h, wrong).await.unwrap();
    assert!(again.replayed && again.view.feedback.id == id);
    let corrected = standing(again.view.corrected.as_ref());
    assert_eq!(
        corrected,
        named("cheap", 5, 4),
        "a replay masks nothing more"
    );

    let undone = receipt_of(
        &h,
        Request::LearningUndo {
            feedback: id.clone(),
        },
    )
    .await;
    assert!(undone.view.feedback.revoked_at.is_some() && !undone.view.penalizing);
    assert_eq!(
        standing(undone.view.corrected.as_ref()),
        named("cheap", 5, 5)
    );
    assert_eq!(default_model(&h).await, "codex/cheap");

    h.restart();
    let why = learning_why(&h, &cheap[2].item).await;
    assert_eq!(why.feedback.len(), 1, "one event for the replayed request");
    assert_eq!(why.feedback[0].feedback.id, id);
    assert!(
        why.feedback[0].feedback.revoked_at.is_some(),
        "kept across a restart"
    );
}

async fn receipt_of(h: &Harness, request: Request) -> archon_core::feedback::FeedbackReceipt {
    receipt(h, request).await.unwrap()
}

#[tokio::test]
async fn informational_ambiguous_and_mismatched_corrections_penalize_no_model() {
    let h = Harness::with_config(config(0.0)).await;
    let cheap = two_choices(&h).await;
    let explicit = h.enqueue("explicit choice").await;
    let (frozen, _, _) = default_route(&h).await;
    for cause in [Scope, Infrastructure, Budget] {
        let receipt = correct(&h, request(&cheap[0], "implement", cause))
            .await
            .unwrap();
        let credit = &receipt.view.feedback.attribution.credit;
        assert!(
            matches!(credit, Credit::Model { .. }),
            "{cause:?} is attributed"
        );
        assert!(!receipt.view.penalizing && receipt.view.corrected.is_none());
    }
    let mut foreign = request(&cheap[0], "implement", Quality);
    foreign.item = cheap[1].item.clone();
    let absent = request(&cheap[0], "deploy", Quality);
    for wrong in [foreign, absent] {
        let error = correct(&h, wrong).await.unwrap_err();
        assert!(matches!(error, ArchonError::Invalid(_)), "{error:?}");
    }
    let at = archon_core::now();
    let reviewed = [("implement", &model("solo")), ("review", &model("cheap"))];
    let reviewed = sample_with(&h, &reviewed, 0.2, at, None).await;
    let mixed = [
        ("implement", &model("mixed-a")),
        ("implement", &model("mixed-b")),
    ];
    let mixed = sample_with(&h, &mixed, 0.2, at, None).await;
    let unattributed = [
        (request(&reviewed, "review", Quality), "only a work stage"),
        (request(&mixed, "implement", Quality), "more than one model"),
    ];
    for (wrong, why) in unattributed {
        let receipt = correct(&h, wrong).await.unwrap();
        let credit = &receipt.view.feedback.attribution.credit;
        assert!(
            matches!(credit, Credit::Unavailable { why: w } if w.contains(why)),
            "{credit:?}"
        );
        assert!(!receipt.view.penalizing);
    }
    assert_eq!(
        default_model(&h).await,
        "codex/cheap",
        "no model was penalized"
    );
    assert_eq!(
        h.item(&explicit.id).await.model_plan,
        plan(),
        "explicit choices hold"
    );
    assert_eq!(h.item(&frozen.id).await.model_plan, frozen.model_plan);
}

#[tokio::test]
async fn a_stage_is_credited_by_the_kind_it_ran_as_never_by_the_current_catalog() {
    let h = Harness::with_config(config(0.0)).await;
    two_choices(&h).await;
    let unrecorded = (None, "kind was not recorded");
    let reviewed = (Some(StageKind::Review), "only a work stage");
    for (kind, why_not) in [unrecorded, reviewed] {
        let ran = sample_ran_as(&h, &model("cheap"), 0.2, kind).await;
        let receipt = correct(&h, request(&ran, "implement", Quality))
            .await
            .unwrap();
        let credit = &receipt.view.feedback.attribution.credit;
        assert!(
            matches!(credit, Credit::Unavailable { why } if why.contains(why_not)),
            "the catalog's work stage is not asked: {credit:?}"
        );
        assert!(!receipt.view.penalizing, "{kind:?}");
    }
    assert_eq!(default_model(&h).await, "codex/cheap");
}

#[tokio::test]
async fn a_correction_in_another_task_cohort_leaves_this_cohort_alone() {
    let h = Harness::with_config(config(0.0)).await;
    two_choices(&h).await;
    let only = [("implement", &model("cheap"))];
    let docs = sample_with(&h, &only, 0.2, archon_core::now(), Some("docs")).await;
    let receipt = correct(&h, request(&docs, "implement", Quality))
        .await
        .unwrap();
    assert!(receipt.view.penalizing, "it counts in its own cohort");
    assert_eq!(
        receipt
            .view
            .feedback
            .attribution
            .context
            .as_ref()
            .unwrap()
            .task_class,
        "docs"
    );
    let (_, model, reason) = default_route(&h).await;
    assert_eq!(model, "codex/cheap");
    assert!(
        !reason.unwrap().contains("operator"),
        "the general cohort has no feedback"
    );
}

/// Enqueues until an item whose id hashes into exploration at 10%, and
/// returns whether its learned default explored.
async fn hashed_explorer_explores(h: &Harness) -> bool {
    for _ in 0..200 {
        let (item, _, reason) = default_route(h).await;
        if !explores_at_ten_percent(&item.id) {
            continue;
        }
        let reason = reason.expect("a learned default says why");
        return reason.contains("bounded uncertainty exploration");
    }
    panic!("no item id hashed into exploration in 200 tries")
}

/// archond's own hash of an item id against a 10% exploration rate.
fn explores_at_ten_percent(item: &QueueItemId) -> bool {
    let hash = item
        .as_str()
        .bytes()
        .fold(14695981039346656037u64, |hash, b| {
            (hash ^ u64::from(b)).wrapping_mul(1099511628211)
        });
    hash % 10_000 < 1_000
}

#[tokio::test]
async fn an_affected_cohort_explores_nothing_at_the_configured_ceiling() {
    let h = Harness::with_config(config(0.1)).await;
    let cheap = two_choices(&h).await;
    sample(&h, &model("trial"), 0.01).await;
    assert!(
        hashed_explorer_explores(&h).await,
        "an unaffected cohort explores"
    );
    let (before, _, _) = default_route(&h).await;

    correct(&h, request(&cheap[0], "implement", Quality))
        .await
        .unwrap();
    assert!(
        !hashed_explorer_explores(&h).await,
        "no trials under a correction"
    );
    let (after, model, reason) = default_route(&h).await;
    assert_eq!(model, "codex/dear");
    assert!(reason.unwrap().contains("(no exploration)"));
    assert_eq!(
        after.autonomy, before.autonomy,
        "the review duty is unchanged"
    );
    assert_eq!(after.model_plan.0["review"], before.model_plan.0["review"]);
}

#[tokio::test]
async fn samples_past_ninety_days_leave_a_corrected_cohort_on_the_static_fallback() {
    let h = Harness::with_config(config(0.0)).await;
    let aged = archon_core::now() - chrono::Duration::days(100);
    let cheap = two_choices_at(&h, aged).await;
    assert_eq!(
        default_model(&h).await,
        "codex/cheap",
        "no feedback, no window"
    );

    let receipt = correct(&h, request(&cheap[0], "implement", Quality))
        .await
        .unwrap();
    assert!(receipt.view.penalizing);
    assert_eq!(receipt.view.corrected, None, "no sample within 90 days");
    assert_eq!(receipt.view.selected, None);
    let fallback = h.ctx.config.orchestrator.model.to_string();
    assert_eq!(default_model(&h).await, fallback, "never the global pool");
    let id = receipt.view.feedback.id;
    receipt_of(
        &h,
        Request::LearningUndo {
            feedback: id.clone(),
        },
    )
    .await;
    assert_eq!(
        default_model(&h).await,
        fallback,
        "the window outlives the undo"
    );
    let why = learning_why(&h, &cheap[0].item).await;
    assert_eq!(why.feedback[0].feedback.id, id, "history stays inspectable");
}

/// Each way a request reaches archond for an agent: over MCP, or forwarded
/// for the current orchestrator or another agent by any client.
const AGENT_PATHS: [(ClientKind, Option<&str>); 6] = [
    (ClientKind::ArchonMcp, None),
    (ClientKind::ArchonMcp, Some(common::ORCHESTRATOR)),
    (ClientKind::ArchonCli, Some(common::ORCHESTRATOR)),
    (ClientKind::ArchonBench, Some(common::ORCHESTRATOR)),
    (ClientKind::ArchonCli, Some(HELPER)),
    (ClientKind::ArchonBench, Some(HELPER)),
];

/// An agent archond does not know as the orchestrator.
const HELPER: &str = "agt_helper";

async fn for_agent(
    h: &Harness,
    (client, agent): (ClientKind, Option<&str>),
    request: Request,
) -> Result<Reply, ArchonError> {
    let request = match agent {
        Some(caller) => Request::ForAgent {
            caller: AetherAgentId::parse(caller).unwrap(),
            request: Box::new(request),
        },
        None => request,
    };
    handlers::handle(&h.ctx, client, request).await
}

#[tokio::test]
async fn no_agent_corrects_or_curates_through_any_client() {
    let h = Harness::new().await;
    let run = sample(&h, &model("cheap"), 0.2).await;
    assert!(matches!(
        h.orchestrator(Request::LearningWhy {
            item: run.item.clone()
        })
        .await,
        Ok(Reply::LearningWhy(_))
    ));
    let recorded = correct(&h, request(&run, "implement", Quality))
        .await
        .unwrap();
    let feedback = recorded.view.feedback.id;
    let mut another = request(&run, "implement", Quality);
    another.reason = "An agent's own verdict".into();
    let mutations = [
        Request::LearningCorrect(another),
        Request::LearningUndo {
            feedback: feedback.clone(),
        },
        Request::LearningRestoreMemory {
            feedback: feedback.clone(),
        },
        Request::LearningRetryMemory { feedback },
    ];
    for mutation in mutations {
        for path in AGENT_PATHS {
            let answer = for_agent(&h, path, mutation.clone()).await;
            assert!(
                matches!(answer, Err(ArchonError::Forbidden(_))),
                "{path:?} {mutation:?}: {answer:?}"
            );
        }
    }
    let why = learning_why(&h, &run.item).await;
    assert_eq!(why.feedback.len(), 1, "no agent recorded a correction");
    let kept = &why.feedback[0];
    assert!(kept.penalizing && kept.feedback.revoked_at.is_none());
}

#[tokio::test]
async fn disabled_learning_records_no_correction() {
    let mut disabled = Config::default();
    disabled.learning.enabled = false;
    let off = Harness::with_config(disabled).await;
    let run = sample(&off, &model("cheap"), 0.2).await;
    let error = correct(&off, request(&run, "implement", CorrectionCause::Quality))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("learning is disabled"),
        "{error}"
    );
    assert!(learning_why(&off, &run.item).await.feedback.is_empty());
}
