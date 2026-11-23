use archon_core::autonomy::{AutonomyPatch, ReviewLevel};
use archon_core::protocol::{ClientFeatures, EnqueueNotice, Reply, Request};
use archon_core::queue::{Caps, QueueState};

use crate::common::{Harness, PIPELINE, REPO_NAME, enqueue_request, plan};

#[tokio::test]
async fn direct_enqueues_advise_on_each_risk_category() {
    let h = Harness::new().await;
    let cases = [
        ("Fix a concurrency bug", "Concurrent writes lose updates."),
        (
            "Change the writer's locking order",
            "Cover the writer path.",
        ),
        (
            "Change account storage",
            "Add a transactional database migration.",
        ),
        (
            "Fix an authorization bypass",
            "Limit reads to the current tenant.",
        ),
        (
            "Change the client response",
            "Make a breaking change to the public JSON format.",
        ),
    ];
    for (title, brief) in cases {
        let mut request = enqueue_request(REPO_NAME, "direct", title, Default::default());
        request.brief = brief.into();
        request.autonomy = AutonomyPatch {
            review: Some(ReviewLevel::None),
            ..Default::default()
        };
        let reply = h.cli(Request::Enqueue(request)).await.unwrap();
        let Reply::EnqueueAdvised {
            item,
            notices,
            budgets_off: false,
        } = reply
        else {
            panic!("{title}: {reply:?}");
        };
        assert_eq!(notices, [EnqueueNotice::DirectRisk], "{title}");
        assert_eq!((item.title.as_str(), item.brief.as_str()), (title, brief));
        assert_eq!(item.pipeline, "direct");
        assert_eq!(item.autonomy.review, ReviewLevel::None);
        assert_eq!(item.state, QueueState::Queued);
        assert_eq!(h.item(&item.id).await, *item);
    }
}

#[tokio::test]
async fn distinct_fix_entries_or_labels_get_bundle_advice_on_any_pipeline() {
    let h = Harness::new().await;
    for brief in [
        "1. Fix the missing tooltip.\n2. Correct the stale icon.",
        "- Fix the empty form state.\n- Repair the broken footer link.",
        "R2-3: Restore the selector.\nR2-4: Correct the date.\nR2-5: Improve the empty state.",
        "### R2-3: Restore the selector.\n### R2-4: Correct the date.",
        "Fix R2-3..R2-5 from the last review.",
    ] {
        let mut request = enqueue_request(REPO_NAME, PIPELINE, "Review follow-ups", plan());
        request.brief = brief.into();
        let reply = h.cli(Request::Enqueue(request)).await.unwrap();
        let Reply::EnqueueAdvised { item, notices, .. } = reply else {
            panic!("{brief}: {reply:?}");
        };
        assert_eq!(notices, [EnqueueNotice::BundledFixes], "{brief}");
        assert_eq!(item.brief, brief);
        assert_eq!(item.pipeline, PIPELINE);
        assert_eq!(h.item(&item.id).await, *item);
    }
}

#[tokio::test]
async fn passing_risk_words_and_validation_steps_do_not_trigger_advice() {
    let h = Harness::new().await;
    for (pipeline, title, brief) in [
        (
            "direct",
            "Correct welcome text",
            "The docs mention security in passing.",
        ),
        (
            "direct",
            "Polish wording",
            "Fix the typo in the migration banner.",
        ),
        (
            "direct",
            "Fix one issue",
            "1. Fix the dropdown.\n2. Verify the dropdown.\n3. Run tests.",
        ),
        (
            "direct",
            "Fix R2-3",
            "R2-3: Fix the menu.\nSee R2-4 for context.",
        ),
        (
            PIPELINE,
            "Replace the writer's locking strategy",
            "Cover the writer path.",
        ),
    ] {
        let mut request = enqueue_request(REPO_NAME, pipeline, title, Default::default());
        request.brief = brief.into();
        let reply = h.cli(Request::Enqueue(request)).await.unwrap();
        let Reply::Item(item) = reply else {
            panic!("{title} on {pipeline}: {reply:?}");
        };
        assert_eq!(item.title, title);
        assert_eq!(item.brief, brief);
    }
}

#[tokio::test]
async fn notices_coexist_with_budget_advice_and_replay_on_the_same_item() {
    let h = Harness::new().await;
    let caps = Caps {
        budgets_enabled: false,
        ..Default::default()
    };
    h.cli(Request::SetCaps { caps }).await.unwrap();
    let mut request = enqueue_request(
        REPO_NAME,
        "direct",
        "Fix concurrency and labels",
        Default::default(),
    );
    request.brief = "- Fix the writer's locking order.\n- Fix a broken button.".into();
    request.budget_usd = Some(3.0);
    request.idempotency_key = Some("advice-replay".into());
    let request = Request::Enqueue(request);
    let first = h.cli(request.clone()).await.unwrap();
    let replay = h.cli(request).await.unwrap();
    assert_eq!(first, replay);
    let Reply::EnqueueAdvised {
        item,
        notices,
        budgets_off: true,
    } = &first
    else {
        panic!("expected both types of advice: {first:?}");
    };
    assert_eq!(
        notices,
        &[EnqueueNotice::DirectRisk, EnqueueNotice::BundledFixes]
    );
    assert_eq!(item.budget_usd, Some(3.0));
    let wire = serde_json::to_value(&first).unwrap();
    assert_eq!(wire["data"]["id"], item.id.as_str());
    assert_eq!(serde_json::from_value::<Reply>(wire).unwrap(), first);
    assert_eq!(
        ClientFeatures::default().adapt_reply(first.clone()),
        Reply::ItemBudgetsOff(*item.clone())
    );
    assert_eq!(ClientFeatures::all().adapt_reply(first.clone()), first);
}
