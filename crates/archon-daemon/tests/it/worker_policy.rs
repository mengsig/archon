//! A delegating stage lead's workers: the delegation policy Archon sends
//! Aether, the worker rule in its contract, and the note `get_item` shows
//! when the model policy leaves the lead no permitted worker.

use archon_core::config::Config;
use archon_core::delegation::WorkerChoice;
use archon_core::model::{Effort, ModelChoice, ModelPlan, ModelRef};
use archon_core::model_tier::{CatalogModel, ModelCatalog, Tier};
use archon_core::ports::SpawnRequest;
use archon_core::protocol::{ItemDetail, Reply, Request};
use archon_core::queue::QueueItem;

use crate::common::delivery::add_origin;
use crate::common::{Harness, MODEL, REVIEW_MODEL, choice, completed};
use crate::executor::{DRAFTED_THEN_LED, adaptive_item, led_item, seed_review_catalog};

const FAST: &str = "codex/gpt-5-mini";
const OTHER_BALANCED: &str = "codex/gpt-5-balanced";

async fn detail(h: &Harness, item: &QueueItem) -> Box<ItemDetail> {
    let reply = h
        .cli(Request::GetItem {
            item: item.id.clone(),
        })
        .await;
    let Reply::ItemDetail(detail) = reply.unwrap() else {
        panic!("item detail expected");
    };
    detail
}

async fn set_catalog(h: &Harness, models: &[(&str, Tier)]) {
    let models = models
        .iter()
        .map(|(model, tier)| CatalogModel {
            model: model.parse().unwrap(),
            tier: Some(*tier),
            is_default: false,
        })
        .collect();
    h.set_catalog(ModelCatalog::new(models)).await;
}

fn allowing(models: &[&str]) -> Config {
    let mut config = Config::default();
    config.models.allow = models.iter().map(|m| m.parse().unwrap()).collect();
    config
}

/// Enqueue an adaptive item under `plan` and dispatch its build lead.
async fn build_lead(h: &Harness, plan: ModelPlan) -> (QueueItem, SpawnRequest) {
    add_origin(h);
    let item = h.enqueue_in("adaptive", "a change", plan).await.unwrap();
    h.dispatch().await;
    let requests = h.backend.requests();
    assert_eq!(requests.len(), 1, "the build lead spawned");
    assert!(!requests[0].leaf, "the build lead delegates");
    (item, requests[0].clone())
}

fn plan_of(entries: &[(&str, &str)]) -> ModelPlan {
    let entry =
        |(key, model): &(&str, &str)| ((*key).to_owned(), vec![choice(model, Effort::Medium)]);
    ModelPlan(entries.iter().map(entry).collect())
}

/// The `Workers` line of a lead's contract.
fn worker_line(request: &SpawnRequest) -> &str {
    let at = request
        .instructions
        .find("- Workers (")
        .expect("a worker rule");
    request.instructions[at..].lines().next().unwrap()
}

/// A delegating lead is sent its pipeline's worker default as Aether's
/// delegation policy, the balanced tier resolved to the model the allowlist
/// permits, its reviewers bounded to the model its contract names, and its
/// contract and `get_item` say so.
#[tokio::test]
async fn an_adaptive_lead_is_sent_balanced_workers_and_its_reviewer_as_its_policy() {
    let mut config = Config::default();
    config.models.allow = vec![MODEL.parse().unwrap(), REVIEW_MODEL.parse().unwrap()];
    config.models.max_effort = Some(Effort::High);
    let h = Harness::with_config(config).await;
    seed_review_catalog(&h).await;
    let item = adaptive_item(&h).await;
    let build = h.backend.requests()[0].clone();
    let policy = build
        .delegation_policy
        .expect("a delegating lead is bounded");
    let allowed = ModelChoice::pinned(MODEL.parse().unwrap(), Effort::Medium);
    assert_eq!(policy.workers, [WorkerChoice::from_plan(&allowed)]);
    assert_eq!(
        (policy.default_tier, policy.max_effort, policy.max_parallel),
        (Some(Tier::Balanced), Some(Effort::Medium), Some(2))
    );
    assert_eq!(policy.exempt_roles, ["reviewer"]);
    assert_eq!(policy.exempt_models.len(), 1, "{policy:?}");
    let named = policy.exempt_models[0].to_string();
    assert!(
        build
            .instructions
            .contains(&format!("on {named} at tier balanced")),
        "the contract names the reviewer the policy allows: {}",
        build.instructions
    );
    let rule = format!(
        "- Workers (every child but a reviewer): each worker on {MODEL} at effort medium, \
         at most 2 at once."
    );
    assert!(build.instructions.contains(&rule), "{}", build.instructions);
    let shown = &detail(&h, &item).await.worker_policies["build"];
    assert_eq!(shown.workers, policy.workers);
    assert!(
        shown.exempt_models.is_empty(),
        "reviewers are bounded at spawn"
    );
}

/// No permitted catalog model sits at or below the balanced worker default:
/// the workers fall back to a pinned permitted model, and the lead's policy
/// still exempts reviewers on the model its contract names, and only those.
#[tokio::test]
async fn a_worker_fallback_keeps_the_lead_s_reviewer_exemptions() {
    let mut config = Config::default();
    config.models.allow = vec![MODEL.parse().unwrap(), REVIEW_MODEL.parse().unwrap()];
    let h = Harness::with_config(config).await;
    let strong = |model: &str| CatalogModel {
        model: model.parse().unwrap(),
        tier: Some(Tier::Strong),
        is_default: false,
    };
    let catalog = ModelCatalog::new(vec![strong(MODEL), strong(REVIEW_MODEL)]);
    h.set_catalog(catalog).await;
    adaptive_item(&h).await;
    let build = h.backend.requests()[0].clone();
    let policy = build
        .delegation_policy
        .expect("a delegating lead is bounded");
    let allowed: [ModelRef; 2] = [MODEL.parse().unwrap(), REVIEW_MODEL.parse().unwrap()];
    assert_eq!(policy.workers.len(), 1, "{policy:?}");
    let pinned = policy.workers[0].model.as_ref().expect("a pinned fallback");
    assert!(allowed.contains(pinned) && policy.workers[0].tier.is_none());
    assert_eq!(
        pinned.to_string(),
        REVIEW_MODEL,
        "the permitted model but the lead's own"
    );
    assert_eq!(
        (policy.workers[0].effort, policy.max_parallel),
        (Effort::Low, Some(1)),
        "the narrowest policy: one worker at the lowest effort"
    );
    assert_eq!(policy.exempt_roles, ["reviewer"]);
    assert_eq!(policy.exempt_models.len(), 1, "{policy:?}");
    assert!(allowed.contains(&policy.exempt_models[0]), "{policy:?}");
    let named = policy.exempt_models[0].to_string();
    assert!(
        build
            .instructions
            .contains(&format!("with role `reviewer`, on {named} at")),
        "the contract names the reviewer the policy exempts: {}",
        build.instructions
    );
}

/// The repository denies the reviewer the item's plan named once its first
/// stage ran: the lead's spawn does not panic, and exempts no denied model.
#[tokio::test]
async fn a_named_reviewer_denied_mid_run_is_left_out_of_the_lead_s_policy() {
    let mut config = Config::default();
    config.models.allow = vec![MODEL.parse().unwrap(), REVIEW_MODEL.parse().unwrap()];
    let h = Harness::with_config(config).await;
    seed_review_catalog(&h).await;
    let bounded = DRAFTED_THEN_LED.replace(
        "[pipeline.review]",
        "[pipeline.workers]\ntier = \"balanced\"\n\n[pipeline.review]",
    );
    h.add_pipeline("drafted", &bounded);
    let planned = |stage: &str, model| (stage.to_owned(), vec![choice(model, Effort::Medium)]);
    let plan = ModelPlan(
        [
            planned("draft", MODEL),
            planned("build", MODEL),
            planned("build.review", REVIEW_MODEL),
        ]
        .into(),
    );
    let item = h
        .enqueue_in("drafted", "a reviewed change", plan)
        .await
        .unwrap();
    h.dispatch().await;
    assert_eq!(h.backend.requests().len(), 1, "the draft spawned");
    h.repo_config(&format!("[models]\ndeny = [\"{REVIEW_MODEL}\"]\n"));
    h.settle_running(|_| completed("drafted")).await;
    h.dispatch().await;
    let denied: ModelRef = REVIEW_MODEL.parse().unwrap();
    let requests = h.backend.requests();
    let state = h.state(&item.id).await;
    assert_eq!(requests.len(), 2, "the lead spawned: {state:?}");
    let policy = requests[1]
        .delegation_policy
        .clone()
        .expect("a bounded lead");
    assert_eq!(policy.exempt_roles, ["reviewer"]);
    assert!(!policy.exempt_models.contains(&denied), "{policy:?}");
}

/// A plan's `<stage>.workers` replaces the pipeline default.
#[tokio::test]
async fn a_planned_workers_key_replaces_the_default_and_no_default_sends_none() {
    let h = Harness::new().await;
    add_origin(&h);
    let low = choice(MODEL, Effort::Low);
    let plan = ModelPlan(
        [
            ("build".to_owned(), vec![choice(MODEL, Effort::Medium)]),
            (archon_core::model::workers_key("build"), vec![low.clone()]),
        ]
        .into(),
    );
    h.enqueue_in("adaptive", "a planned change", plan)
        .await
        .unwrap();
    h.dispatch().await;
    let build = h.backend.requests()[0].clone();
    let policy = build
        .delegation_policy
        .expect("a delegating lead is bounded");
    assert_eq!(policy.workers, [WorkerChoice::from_plan(&low)]);
    assert_eq!(policy.max_effort, Some(Effort::Low));
    assert_eq!(policy.max_parallel, Some(2), "the pipeline's cap stays");
}

/// An old item, or a pipeline of the user's own, with no worker default:
/// the lead is sent no policy and no worker rule, and gets no note, as before.
#[tokio::test]
async fn a_lead_with_no_worker_policy_keeps_the_current_behaviour() {
    let h = Harness::new().await;
    let item = led_item(&h).await;
    let lead = h.backend.requests()[0].clone();
    assert!(!lead.leaf, "the lead delegates");
    assert_eq!(lead.delegation_policy, None);
    assert!(
        !lead.instructions.contains("- Workers ("),
        "{}",
        lead.instructions
    );
    assert!(detail(&h, &item).await.stage_notes.is_empty());
}

/// The repository caps effort at low: the pipeline's medium worker default
/// is capped, in the policy Aether is sent and in the lead's contract alike.
#[tokio::test]
async fn a_low_effort_cap_clamps_the_medium_worker_default_in_policy_and_contract() {
    let mut config = Config::default();
    config.models.max_effort = Some(Effort::Low);
    let h = Harness::with_config(config).await;
    seed_review_catalog(&h).await;
    let plan = ModelPlan([("build".to_owned(), vec![choice(MODEL, Effort::Low)])].into());
    let (item, build) = build_lead(&h, plan).await;
    let policy = build
        .delegation_policy
        .clone()
        .expect("a delegating lead is bounded");
    assert_eq!(
        policy.workers,
        [WorkerChoice::of_tier(Tier::Balanced, Effort::Low)]
    );
    assert_eq!(policy.max_effort, Some(Effort::Low));
    let line = worker_line(&build);
    assert!(
        line.contains("each worker on the balanced tier at effort low, at most 2 at once"),
        "{line}"
    );
    assert!(!line.contains("medium"), "{line}");
    assert!(
        detail(&h, &item).await.stage_notes.is_empty(),
        "nothing fell back"
    );
}

/// An empty catalog (a failed refresh), an allowlist and pinned stage
/// models: no worker is permitted, yet the lead still spawns, exempting its
/// planned reviewer, and its contract and `get_item` say it works alone.
#[tokio::test]
async fn an_empty_catalog_under_an_allowlist_leaves_the_lead_alone_with_its_reviewers() {
    let h = Harness::with_config(allowing(&[MODEL, REVIEW_MODEL])).await;
    assert!(h.ctx.store.lock().await.model_catalog().is_empty());
    let plan = plan_of(&[("build", MODEL), ("build.review", REVIEW_MODEL)]);
    let (item, build) = build_lead(&h, plan).await;
    let policy = build
        .delegation_policy
        .clone()
        .expect("a lead alone is still bounded");
    assert!(
        policy.workers.is_empty(),
        "no worker, never the lead's own model: {policy:?}"
    );
    assert_eq!(
        (policy.max_effort, policy.max_parallel),
        (Some(Effort::Low), Some(1))
    );
    assert_eq!(policy.exempt_roles, ["reviewer"]);
    let reviewer: ModelRef = REVIEW_MODEL.parse().unwrap();
    assert_eq!(policy.exempt_models, [reviewer]);
    let line = worker_line(&build);
    assert!(line.contains("): none. Do the work yourself"), "{line}");
    assert!(
        line.contains("(the model catalog is empty; planned: "),
        "{line}"
    );
    let shown = detail(&h, &item).await;
    let build_run = shown.stages.iter().find(|s| s.stage_id == "build").unwrap();
    let note = &shown.stage_notes[&build_run.id];
    assert!(
        note.starts_with("no permitted worker model resolved (the model catalog is empty"),
        "{note}"
    );
    assert!(note.ends_with("; the lead works alone"), "{note}");
    assert!(
        line.contains(note.as_str()),
        "the contract and get_item agree: {line}"
    );
}

/// An allowlist that leaves the balanced model out: the balanced default
/// resolves to the permitted fast model, and the balanced model is neither
/// instructed nor authorized.
#[tokio::test]
async fn an_allowlist_without_the_balanced_model_neither_instructs_nor_authorizes_it() {
    let h = Harness::with_config(allowing(&[MODEL, FAST, REVIEW_MODEL])).await;
    set_catalog(
        &h,
        &[
            (OTHER_BALANCED, Tier::Balanced),
            (FAST, Tier::Fast),
            (MODEL, Tier::Strong),
        ],
    )
    .await;
    let (item, build) = build_lead(&h, plan_of(&[("build", MODEL)])).await;
    let policy = build
        .delegation_policy
        .clone()
        .expect("a delegating lead is bounded");
    let fast = ModelChoice::pinned(FAST.parse().unwrap(), Effort::Medium);
    assert_eq!(
        policy.workers,
        [WorkerChoice::from_plan(&fast)],
        "no bare tier Aether could place"
    );
    let wire = serde_json::to_string(&policy).unwrap();
    assert!(!wire.contains(OTHER_BALANCED), "{wire}");
    assert!(
        !build.instructions.contains(OTHER_BALANCED),
        "{}",
        build.instructions
    );
    let line = worker_line(&build);
    assert!(
        line.contains(&format!("each worker on {FAST} at effort medium")),
        "{line}"
    );
    assert!(detail(&h, &item).await.stage_notes.is_empty());
}

/// A lead on a model the catalog does not rank: its workers resolve from the
/// catalog as for any lead, and when nothing else is permitted it works
/// alone rather than cloning itself.
#[tokio::test]
async fn a_lead_with_no_catalog_tier_is_never_its_own_worker() {
    let h = Harness::with_config(allowing(&[MODEL, FAST])).await;
    set_catalog(&h, &[(FAST, Tier::Fast), (REVIEW_MODEL, Tier::Balanced)]).await;
    let (_, build) = build_lead(&h, plan_of(&[("build", MODEL)])).await;
    let policy = build
        .delegation_policy
        .clone()
        .expect("a delegating lead is bounded");
    let fast = ModelChoice::pinned(FAST.parse().unwrap(), Effort::Medium);
    assert_eq!(policy.workers, [WorkerChoice::from_plan(&fast)]);

    let h = Harness::with_config(allowing(&[MODEL])).await;
    set_catalog(&h, &[(FAST, Tier::Fast), (REVIEW_MODEL, Tier::Balanced)]).await;
    let (item, build) = build_lead(&h, plan_of(&[("build", MODEL)])).await;
    let policy = build
        .delegation_policy
        .clone()
        .expect("a lead alone is still bounded");
    assert!(
        policy.workers.is_empty(),
        "never the lead's own model: {policy:?}"
    );
    assert!(
        !worker_line(&build).contains(MODEL),
        "{}",
        worker_line(&build)
    );
    let shown = detail(&h, &item).await;
    let note = shown
        .stage_notes
        .values()
        .next()
        .expect("the fallback is noted");
    assert!(
        note.contains("(the model policy permits none of: "),
        "{note}"
    );
}

/// The lead's own model is the only permitted catalog model: the fallback
/// never picks it, so the lead works alone, reviewers still exempt.
#[tokio::test]
async fn a_lead_that_is_the_only_permitted_catalog_model_works_alone() {
    let h = Harness::with_config(allowing(&[MODEL, REVIEW_MODEL])).await;
    set_catalog(&h, &[(MODEL, Tier::Strong)]).await;
    let plan = plan_of(&[("build", MODEL), ("build.review", REVIEW_MODEL)]);
    let (item, build) = build_lead(&h, plan).await;
    let policy = build
        .delegation_policy
        .clone()
        .expect("a lead alone is still bounded");
    assert!(
        policy.workers.is_empty(),
        "never the lead's own model: {policy:?}"
    );
    assert_eq!(
        policy.default_tier,
        Some(Tier::Fast),
        "Aether's tier rule, narrowest"
    );
    assert_eq!(policy.exempt_roles, ["reviewer"]);
    assert!(worker_line(&build).contains("): none. Do the work yourself"));
    let shown = detail(&h, &item).await;
    let note = shown
        .stage_notes
        .values()
        .next()
        .expect("the fallback is noted");
    assert!(note.ends_with("; the lead works alone"), "{note}");
}
