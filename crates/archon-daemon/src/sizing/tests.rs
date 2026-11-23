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

/// A config that opts into raising: the sizer may reach `x_high`.
fn raising_config() -> Config {
    let mut config = Config::default();
    config.models.sizer_max_effort = Some(Effort::XHigh);
    config
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
        let writer = written(&item, &direct);
        item.model_plan = finish((config, policy), &direct, || Ok(None), writer).unwrap();
        item.settle_sizing(archon_core::now());
        item
    };
    let open = ModelPolicy::default();
    assert_eq!(effort_of(&finished(&Config::default(), &open)), base);
    let mut raising = Config::default();
    raising.models.sizer_max_effort = Some(Effort::XHigh);
    assert_eq!(effort_of(&finished(&raising, &open)), Effort::XHigh);
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
    let mut held = finished(&raising, &open);
    assert!(clip_to_policy(&capped, &mut held));
    assert_eq!(effort_of(&held), Effort::Medium);
    assert!(
        !clip_to_policy(&capped, &mut held),
        "already within the cap"
    );
    assert!(!clip_to_policy(&open, &mut held), "no cap clips nothing");
}

/// The measured diff size is read for a plan's first sizing only: never with
/// sizing off, and never for a plan that carries a sizing already, so a
/// retry cannot size again from a size learned since.
#[test]
fn the_measured_size_is_read_only_when_a_plan_is_first_sized() {
    let direct = spec("direct");
    let item = critical_item();
    let open = ModelPolicy::default();
    let unreadable = || Err(ArchonError::Corrupt("diff-size hint".into()));
    let on = Config::default();
    let first = finish((&on, &open), &direct, unreadable, written(&item, &direct));
    assert!(
        matches!(first, Err(ArchonError::Corrupt(_))),
        "a first sizing reads it"
    );
    let mut off = Config::default();
    off.models.effort_sizing = Some(false);
    let as_1_6_3 = finish((&off, &open), &direct, unreadable, written(&item, &direct));
    assert!(as_1_6_3.expect("never read").sizing().is_none());

    let mut sized = item.clone();
    let writer = written(&item, &direct);
    sized.model_plan = finish((&on, &open), &direct, || Ok(None), writer).unwrap();
    sized.settle_sizing(archon_core::now());
    let mut retry = PlanWriter::new(&item.id, archon_core::now());
    let cap = carried_cap(&on, &open);
    retry.carry(&sized.model_plan, &item.id, cap, |_| Some(usize::MAX));
    let carried = finish((&on, &open), &direct, unreadable, retry).expect("never read");
    assert_eq!(carried, sized.model_plan, "carried as it was sized");
}

fn spawn_of(model: &str, effort: Effort) -> SpawnPlan {
    SpawnPlan {
        only_reviews: false,
        stage_id: "implement".into(),
        kind: archon_core::catalog::StageKind::Work,
        attempt: 1,
        fan_index: 0,
        fan_count: 1,
        carried_slots: Default::default(),
        choice: ModelChoice::pinned(model.parse().unwrap(), effort),
        prompt: "do it".into(),
        instructions: "contract".into(),
        timeout_minutes: 30,
        allow_delegation: false,
        workspace: archon_core::catalog::WorkspaceMode::Worktree,
        team: None,
        keep: false,
        resume: None,
        reviewers: None,
        workers: None,
        reports_requirements: false,
        record_repair: archon_core::run::RecordRepair::Open,
    }
}

fn hold_all(item: &QueueItem, plans: &mut [SpawnPlan]) {
    for plan in plans.iter_mut() {
        hold_to_plan(item, plan);
    }
}

/// A slot moved to an engine with no x_high and back (a quota round trip)
/// is stored at high; a driver that read x_high before runs high, as a
/// recovered one would, and its attempt keeps the class and the reason.
#[test]
fn a_spawn_never_runs_above_the_effort_its_stored_plan_holds_now() {
    let direct = spec("direct");
    let mut item = critical_item();
    let (on, open) = (raising_config(), ModelPolicy::default());
    let writer = written(&item, &direct);
    item.model_plan = finish((&on, &open), &direct, || Ok(None), writer).unwrap();
    item.settle_sizing(archon_core::now());
    let read_by_driver = spawn_of("codex/gpt-6.2-sol", Effort::XHigh);
    let by = (
        archon_core::plan_origin::Authority::Operator,
        archon_core::now(),
    );
    for model in ["gemini/gemini-pro", "codex/gpt-6.2-sol"] {
        let model: archon_core::model::ModelRef = model.parse().unwrap();
        item.model_plan
            .steer(&item.id, "implement", (Some(&model), None), by);
    }
    assert_eq!(
        item.model_plan.choices_for("implement").unwrap()[0].effort,
        Effort::High
    );
    let mut plans = vec![read_by_driver.clone()];
    hold_all(&item, &mut plans);
    assert_eq!(
        plans[0].choice.effort,
        Effort::High,
        "the stored plan, not the driver"
    );
    let sized = attempt(&item, &plans[0]);
    assert_eq!(sized.risk_class, Some(effort_sizing::RiskClass::Critical));
    assert_eq!(sized.effort_source, Some(PlanSource::Sizer));
    assert!(
        sized
            .effort_reasons
            .unwrap()
            .starts_with("sizer: critical -> x_high")
    );

    // A pinned slot is the operator's: the driver's effort stands.
    let pin = (None, Some(Effort::XHigh));
    let operator = (
        archon_core::plan_origin::Authority::Operator,
        archon_core::now(),
    );
    item.model_plan.steer(&item.id, "implement", pin, operator);
    let mut plans = vec![spawn_of("codex/gpt-6.2-sol", Effort::Max)];
    hold_all(&item, &mut plans);
    assert_eq!(
        plans[0].choice.effort,
        Effort::Max,
        "never lowered for a pin"
    );
}

/// A spawn whose model moved after its plan was read runs what the new
/// engine runs when the daemon sized its slot, and its attempt says why.
#[test]
fn a_sized_spawn_moved_to_an_engine_without_x_high_runs_high_and_says_why() {
    let direct = spec("direct");
    let mut item = critical_item();
    let (on, open) = (raising_config(), ModelPolicy::default());
    let writer = written(&item, &direct);
    item.model_plan = finish((&on, &open), &direct, || Ok(None), writer).unwrap();
    item.settle_sizing(archon_core::now());
    let mut plans = vec![
        spawn_of("claude-code/claude-opus-5-5", Effort::XHigh),
        spawn_of("gemini/gemini-pro", Effort::XHigh),
    ];
    hold_all(&item, &mut plans);
    let ran: Vec<Effort> = plans.iter().map(|plan| plan.choice.effort).collect();
    assert_eq!(ran, [Effort::XHigh, Effort::High]);
    let as_planned = attempt(&item, &plans[0]);
    assert_eq!(
        as_planned.effort_reasons.as_deref(),
        Some("sizer: critical -> x_high")
    );
    let moved = attempt(&item, &plans[1]);
    assert_eq!(moved.risk_class, Some(effort_sizing::RiskClass::Critical));
    assert_eq!(moved.effort_source, Some(PlanSource::Sizer));
    let why = "sizer: critical -> x_high; this attempt: gemini runs no more than high, so high";
    assert_eq!(moved.effort_reasons.as_deref(), Some(why));

    // The acceptance judge is held to the policy's cap as its spawn is reserved.
    let capped = attempt(&item, &spawn_of("codex/gpt-6.1-sol", Effort::Medium));
    let why = "sizer: critical -> x_high; this attempt: clipped from x_high to the cap medium";
    assert_eq!(capped.effort_reasons.as_deref(), Some(why));
    assert_eq!(capped.risk_class, moved.risk_class, "its class is kept");
}

/// A pinned `x_high` is the operator's on any engine, as 1.6.3 leaves it.
#[test]
fn a_pinned_x_high_is_left_alone_on_any_engine() {
    let direct = spec("direct");
    let mut item = critical_item();
    let (on, open) = (raising_config(), ModelPolicy::default());
    let writer = written(&item, &direct);
    item.model_plan = finish((&on, &open), &direct, || Ok(None), writer).unwrap();
    item.settle_sizing(archon_core::now());
    let by = (
        archon_core::plan_origin::Authority::Operator,
        archon_core::now(),
    );
    let pin = (None, Some(Effort::XHigh));
    item.model_plan.steer(&item.id, "implement", pin, by);
    let mut plans = vec![spawn_of("gemini/gemini-pro", Effort::XHigh)];
    hold_all(&item, &mut plans);
    assert_eq!(plans[0].choice.effort, Effort::XHigh);
    let slot = ("implement", 0);
    assert_eq!(engine_effort(&item, slot, &plans[0].choice), Effort::XHigh);
    let mut unsized_item = critical_item();
    unsized_item.model_plan = written(&item, &direct).finish(Sizing::Off { policy_cap: None });
    hold_all(&unsized_item, &mut plans);
    assert_eq!(plans[0].choice.effort, Effort::XHigh, "never sized");
}

/// A woken agent of a sized slot is requested and recorded as it runs, even
/// when its model moved after the driver read the plan (a quota move, an
/// acceptance judge); an unsized slot keeps the driver's choice, as in 1.6.3.
#[test]
fn a_woken_sized_slot_is_recorded_as_its_agent_runs_on_any_model() {
    let direct = spec("direct");
    let mut item = critical_item();
    let (on, open) = (raising_config(), ModelPolicy::default());
    let writer = written(&item, &direct);
    item.model_plan = finish((&on, &open), &direct, || Ok(None), writer).unwrap();
    item.settle_sizing(archon_core::now());
    let ran = ModelChoice::pinned("gemini/gemini-pro".parse().unwrap(), Effort::High);
    let mut woken = spawn_of("claude-code/claude-opus-5-5", Effort::XHigh);
    assert!(hold_to_woken(&item, &ran, &mut woken));
    assert_eq!(woken.choice, ran, "the agent's model and effort");
    let sized = attempt(&item, &woken);
    let why = "sizer: critical -> x_high; this attempt: gemini runs no more than high, so high";
    assert_eq!(sized.effort_reasons.as_deref(), Some(why));

    let mut unsized_item = critical_item();
    unsized_item.model_plan = written(&item, &direct).finish(Sizing::Off { policy_cap: None });
    let driver = spawn_of("claude-code/claude-opus-5-5", Effort::XHigh);
    let mut kept = driver.clone();
    assert!(!hold_to_woken(&unsized_item, &ran, &mut kept));
    assert_eq!(
        kept.choice, driver.choice,
        "never sized: the driver's choice"
    );
}
