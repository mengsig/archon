use super::*;
use archon_core::config::Config;
use archon_core::plan_origin::PlanSource;
use archon_pipeline::Catalog;

fn spec(name: &str) -> PipelineSpec {
    Catalog::builtin().get(name).expect("built-in").clone()
}

/// A stage key, its reviewers key and its workers key each read their own
/// stage's default; only the reviewers and a review or test stage check work.
#[test]
fn each_plan_key_is_sized_from_its_own_stage() {
    let direct = spec("direct");
    let implement = direct.stage("implement").expect("the one stage");
    let lead = stage_facts(&direct, "implement").expect("a stage key");
    assert_eq!((lead.base, lead.checks), (implement.effort, false));
    let reviewers = direct.reviewers_key().expect("direct holds a review");
    let checking = stage_facts(&direct, &reviewers).expect("the reviewers key");
    assert_eq!((checking.base, checking.checks), (implement.effort, true));
    assert_eq!(stage_facts(&direct, "no-such-stage"), None);
    assert_eq!(stage_of("build.workers"), "build");
    assert_eq!(stage_of(&reviewers), "implement");
    assert_eq!(stage_of("implement"), "implement");
    for stage in &spec("adaptive").file.stages {
        let facts = stage_facts(&spec("adaptive"), &stage.id).expect("a stage key");
        assert_eq!(facts.base, stage.effort, "{}: its own default", stage.id);
    }
}

fn critical_item() -> QueueItem {
    let at = archon_core::now();
    QueueItem {
        intent: None,
        onto: None,
        id: archon_core::id::QueueItemId::new(),
        repo: None,
        pipeline: "direct".into(),
        title: "Add a button".into(),
        brief: String::new(),
        priority: archon_core::queue::Priority::Normal,
        dependencies: vec![],
        tags: vec![effort_sizing::CRITICAL_TAG.to_owned()],
        model_plan: Default::default(),
        model_reasons: None,
        budget_usd: None,
        autonomy: archon_core::autonomy::Autonomy::default(),
        origin: None,
        stage_timeouts: Default::default(),
        fresh_retries: 0,
        routed_provider: None,
        provider_switch: None,
        initiative: None,
        risk: None,
        state: archon_core::queue::QueueState::Queued,
        created_at: at,
        updated_at: at,
    }
}

fn written(item: &QueueItem, spec: &PipelineSpec) -> PlanWriter {
    let mut writer = PlanWriter::new(&item.id, archon_core::now());
    let choice = archon_core::model::ModelChoice::pinned(
        "claude-code/claude-opus-5-5".parse().unwrap(),
        spec.default_effort("implement").unwrap(),
    );
    let plan = archon_core::model::ModelPlan([("implement".to_owned(), vec![choice])].into());
    writer.filled(plan, (PlanSource::Fallback, PlanSource::Fallback));
    writer
}

/// `[models] effort_sizing` and the two caps are read here and nowhere else.
#[test]
fn the_switch_and_the_caps_decide_how_a_written_plan_is_finished() {
    let direct = spec("direct");
    let item = critical_item();
    let base = direct.default_effort("implement").unwrap();
    let effort_of = |item: &QueueItem| item.model_plan.choices_for("implement").unwrap()[0].effort;
    let finished = |config: &Config, policy: &ModelPolicy| {
        let mut item = item.clone();
        item.model_plan = finish((config, policy), &direct, None, written(&item, &direct));
        item.settle_sizing(archon_core::now());
        item
    };
    let open = ModelPolicy::default();
    assert_eq!(effort_of(&finished(&Config::default(), &open)), Effort::XHigh);
    let mut off = Config::default();
    off.models.effort_sizing = Some(false);
    let unsized_item = finished(&off, &open);
    assert_eq!(effort_of(&unsized_item), base);
    assert!(unsized_item.model_plan.sizing().is_none());
    let mut low = Config::default();
    low.models.sizer_max_effort = Some(Effort::High);
    assert_eq!(effort_of(&finished(&low, &open)), Effort::High);
    let capped = ModelPolicy {
        max_effort: Some(Effort::Medium),
        ..ModelPolicy::default()
    };
    assert_eq!(cap(&low, &capped), Effort::Medium);
    let mut held = finished(&Config::default(), &open);
    assert!(clip_to_policy(&capped, &mut held));
    assert_eq!(effort_of(&held), Effort::Medium);
    assert!(!clip_to_policy(&capped, &mut held), "already within the cap");
    assert!(!clip_to_policy(&open, &mut held), "no cap clips nothing");
}
