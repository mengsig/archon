use super::*;
use crate::catalog::StageKind;
use crate::model::ModelPlanDraft;

const CLAUDE: &str = "claude-code/claude-opus-5-5";

fn choice(effort: Effort) -> ModelChoice {
    ModelChoice::pinned(CLAUDE.parse().unwrap(), effort)
}

fn tags(named: &[&str]) -> Vec<String> {
    named.iter().map(|tag| (*tag).to_owned()).collect()
}

/// What an item with `title` and `tags` records of itself.
fn input<'a>(title: &'a str, tags: &'a [String]) -> RiskInput<'a> {
    RiskInput {
        title,
        brief: "",
        tags,
        review: ReviewLevel::Standard,
        size: None,
    }
}

/// Stage defaults that differ per key, so a borrowed one shows.
fn facts(key: &str) -> Option<StageFacts> {
    let base = match key {
        "plan" => Effort::High,
        "build" | "extra" => Effort::Medium,
        "check" => Effort::Low,
        _ => return None,
    };
    let kind = match key {
        "check" => StageKind::Review,
        _ => StageKind::Work,
    };
    Some(StageFacts::new(base, kind, false))
}

fn on(cap: Effort) -> Sizing<'static> {
    Sizing::On(EffortSizer {
        cap,
        size: None,
        facts: &facts,
    })
}

/// A writer that filled `keys`, each at `effort`, from `source`.
fn filled(item: &QueueItemId, keys: &[&str], effort: Effort, source: PlanSource) -> PlanWriter {
    let mut writer = PlanWriter::new(item, crate::now());
    add(&mut writer, keys, effort, source);
    writer
}

fn add(writer: &mut PlanWriter, keys: &[&str], effort: Effort, source: PlanSource) {
    let mut plan = writer.plan().clone();
    for key in keys {
        plan.0.insert((*key).to_owned(), vec![choice(effort)]);
    }
    writer.filled(plan, (source, source));
}

fn effort(planned: &PlannedPlan, key: &str) -> Effort {
    planned.choices_for(key).expect("a planned key")[0].effort
}

fn reason(planned: &PlannedPlan, item: &QueueItemId, key: &str) -> String {
    planned
        .slot_sizing(item, key, 0)
        .expect("a sized key")
        .reason
}

/// `keys` filled at `effort` and sized for an item titled `title`.
fn sized(item: &QueueItemId, keys: &[&str], effort: Effort, title: &str) -> PlannedPlan {
    let writer = filled(item, keys, effort, PlanSource::Fallback);
    let mut planned = writer.finish(on(Effort::XHigh));
    assert!(planned.has_unsized(), "recorded, not yet sized");
    assert!(planned.settle(item, input(title, &[]), crate::now()));
    planned
}

#[test]
fn each_new_key_is_sized_from_its_own_stage_default_whatever_filled_it() {
    let item = QueueItemId::new();
    // Every key was filled at x_high, as a donor slot or a pipeline fallback would.
    let planned = sized(
        &item,
        &["plan", "build", "check"],
        Effort::XHigh,
        "Add a button",
    );
    assert_eq!(effort(&planned, "plan"), Effort::High);
    assert_eq!(effort(&planned, "build"), Effort::Medium);
    assert_eq!(effort(&planned, "check"), Effort::Low);
    let origin = planned.origin(&item, "build", 0).effort;
    assert_eq!(
        (origin.source, origin.by),
        (PlanSource::Sizer, Authority::Daemon)
    );
    assert_eq!(origin.note.as_deref(), Some("was fallback: x_high"));
    assert_eq!(
        reason(&planned, &item, "build"),
        "sizer: ordinary -> medium"
    );
    let sizing = planned.sizing().expect("a record");
    assert_eq!(sizing.risk_class, Some(RiskClass::Ordinary));
    assert!(!planned.has_unsized());
}

#[test]
fn a_critical_item_goes_to_x_high_and_a_capped_one_says_so() {
    let item = QueueItemId::new();
    let critical = tags(&["risk:critical"]);
    let writer = filled(&item, &["build"], Effort::Medium, PlanSource::Fallback);
    let mut open = writer.finish(on(Effort::XHigh));
    open.settle(&item, input("Add a button", &critical), crate::now());
    assert_eq!(effort(&open, "build"), Effort::XHigh);
    assert_eq!(reason(&open, &item, "build"), "sizer: critical -> x_high");
    let writer = filled(&item, &["build"], Effort::Medium, PlanSource::Fallback);
    let mut capped = writer.finish(on(Effort::High));
    capped.settle(&item, input("Add a button", &critical), crate::now());
    assert_eq!(effort(&capped, "build"), Effort::High);
    let why = "sizer: critical -> x_high; capped at high; consider a stronger tier";
    assert_eq!(reason(&capped, &item, "build"), why);
}

#[test]
fn with_sizing_off_a_plan_is_stored_exactly_as_before() {
    let item = QueueItemId::new();
    let writer = filled(&item, &["build"], Effort::XHigh, PlanSource::Fallback);
    let off = writer.finish(Sizing::Off { policy_cap: None });
    assert!(off.sizing().is_none() && !off.has_unsized());
    assert_eq!(effort(&off, "build"), Effort::XHigh);
    let stored = serde_json::to_value(&off).unwrap();
    let keys: Vec<&String> = stored.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["model_origins", "model_plan"], "no sizing key");
    let mut settled = off.clone();
    assert!(!settled.settle(&item, input("Fix the mutex", &[]), crate::now()));
    assert_eq!(settled, off, "nothing recorded, nothing sized");
    assert!(off.slot_sizing(&item, "build", 0).is_none());
}

/// A pinned effort is never changed; the sizer's target is kept as advice.
#[test]
fn a_named_effort_stays_and_its_reason_carries_the_advice() {
    let item = QueueItemId::new();
    let asked: ModelPlanDraft = serde_json::from_value(
        serde_json::json!({"build": [{"model": CLAUDE, "effort": "medium"}],
            "plan": [{"model": CLAUDE}]}),
    )
    .unwrap();
    let resolved = ModelPlan(
        [
            ("build".to_owned(), vec![choice(Effort::Medium)]),
            ("plan".to_owned(), vec![choice(Effort::High)]),
        ]
        .into(),
    );
    let mut writer = PlanWriter::new(&item, crate::now());
    writer.request(&asked, resolved, Authority::Operator);
    let mut planned = writer.finish(on(Effort::XHigh));
    planned.settle(&item, input("x", &tags(&["risk:critical"])), crate::now());
    assert_eq!(effort(&planned, "build"), Effort::Medium, "the pin stands");
    let build = planned.slot_sizing(&item, "build", 0).unwrap();
    assert_eq!(build.effort_source, PlanSource::Explicit);
    assert_eq!(
        build.reason,
        "sizer: critical -> x_high; the plan pins medium"
    );
    // A model named without an effort is a pinned model with a sized effort.
    assert_eq!(effort(&planned, "plan"), Effort::XHigh);
    let plan = planned.origin(&item, "plan", 0);
    assert_eq!(plan.model.source, PlanSource::Explicit);
    assert_eq!(plan.effort.source, PlanSource::Sizer);
}

#[test]
fn a_learned_route_supplies_the_model_and_its_effort_is_advice() {
    let item = QueueItemId::new();
    let writer = filled(&item, &["build"], Effort::High, PlanSource::Learned);
    let mut planned = writer.finish(on(Effort::XHigh));
    planned.settle(&item, input("Add a button", &[]), crate::now());
    assert_eq!(effort(&planned, "build"), Effort::Medium);
    let origin = planned.origin(&item, "build", 0);
    assert_eq!(
        origin.model.source,
        PlanSource::Learned,
        "the route's model"
    );
    assert_eq!(origin.effort.source, PlanSource::Sizer);
    let why = "sizer: ordinary -> medium; the learned route advised high";
    assert_eq!(reason(&planned, &item, "build"), why);
}

/// A retry carries its plan: nothing is sized again, whatever the item or
/// the sizer says by then.
#[test]
fn a_carried_plan_keeps_its_frozen_sizing() {
    let item = QueueItemId::new();
    let first = sized(&item, &["plan", "build"], Effort::Medium, "Add a button");
    let mut retry = PlanWriter::new(&item, crate::now());
    retry.carry(&first, &item, None, |_| Some(usize::MAX));
    let larger = EffortSizer {
        cap: Effort::XHigh,
        size: Some(DiffSize::Large),
        facts: &|_| Some(StageFacts::new(Effort::Max, StageKind::Work, false)),
    };
    let mut carried = retry.finish(Sizing::On(larger));
    assert!(
        !carried.has_unsized(),
        "a carried key is not recorded again"
    );
    assert!(!carried.settle(
        &item,
        input("Fix the mutex", &tags(&["risk:critical"])),
        crate::now()
    ));
    assert_eq!(carried.plan(), first.plan());
    assert_eq!(
        carried.sizing(),
        first.sizing(),
        "the size it froze included"
    );
    assert_eq!(carried.origins(&item), first.origins(&item));
}

/// A plan carried whole and never sized stays unsized, and a stage added to
/// it is sized from its own default, not from the carried choices.
#[test]
fn a_stage_added_to_a_carried_plan_is_the_only_one_sized() {
    let item = QueueItemId::new();
    let writer = filled(
        &item,
        &["plan", "build"],
        Effort::XHigh,
        PlanSource::Fallback,
    );
    let never_sized = writer.finish(Sizing::Off { policy_cap: None });
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&never_sized, &item, None, |_| Some(usize::MAX));
    let whole = kept.finish(on(Effort::XHigh));
    assert!(whole.sizing().is_none(), "carried whole: never sized");
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&never_sized, &item, None, |_| Some(usize::MAX));
    // The new stage borrows the donor's choice, as the fill does.
    add(&mut kept, &["check"], Effort::XHigh, PlanSource::Fallback);
    let mut grown = kept.finish(on(Effort::XHigh));
    grown.settle(&item, input("Add a button", &[]), crate::now());
    assert_eq!(effort(&grown, "check"), Effort::Low, "its own default");
    assert_eq!(
        effort(&grown, "build"),
        Effort::XHigh,
        "carried, never sized"
    );
    assert!(grown.slot_sizing(&item, "build", 0).is_none());
    let keys: Vec<&String> = grown.sizing().unwrap().keys.keys().collect();
    assert_eq!(keys, ["check"]);
}

/// Sizing turned off between two plans: the frozen keys stay, and a stage
/// added afterwards is filled as 1.6.3 fills it, with no record.
#[test]
fn with_sizing_off_a_new_stage_gets_no_record_beside_frozen_ones() {
    let item = QueueItemId::new();
    let first = sized(&item, &["build"], Effort::Medium, "Fix the mutex");
    assert_eq!(effort(&first, "build"), Effort::High);
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&first, &item, None, |_| Some(usize::MAX));
    add(&mut kept, &["check"], Effort::XHigh, PlanSource::Fallback);
    let mut grown = kept.finish(Sizing::Off { policy_cap: None });
    assert!(!grown.settle(&item, input("Fix the mutex", &[]), crate::now()));
    assert_eq!(effort(&grown, "check"), Effort::XHigh, "as 1.6.3 filled it");
    assert!(
        grown.slot_sizing(&item, "check", 0).is_none(),
        "no stale class"
    );
    assert_eq!(effort(&grown, "build"), Effort::High, "frozen");
    assert_eq!(reason(&grown, &item, "build"), "sizer: elevated -> high");
}

fn critical_build(item: &QueueItemId) -> PlannedPlan {
    let writer = filled(item, &["build"], Effort::Medium, PlanSource::Fallback);
    let mut planned = writer.finish(on(Effort::XHigh));
    planned.settle(item, input("x", &tags(&["risk:critical"])), crate::now());
    assert_eq!(effort(&planned, "build"), Effort::XHigh);
    planned
}

/// A carried sized effort above a lowered cap is clipped with the reason; a
/// pinned one is left for the policy check to refuse.
#[test]
fn a_carried_sized_effort_is_clipped_to_the_cap_and_a_pin_is_not() {
    let item = QueueItemId::new();
    let first = critical_build(&item);
    let carry = |sizing: Sizing<'_>| {
        let mut retry = PlanWriter::new(&item, crate::now());
        retry.carry(&first, &item, None, |_| Some(usize::MAX));
        retry.finish(sizing)
    };
    for lowered in [
        on(Effort::High),
        Sizing::Off {
            policy_cap: Some(Effort::High),
        },
    ] {
        let clipped = carry(lowered);
        assert_eq!(effort(&clipped, "build"), Effort::High);
        let why = "sizer: critical -> x_high; clipped from x_high to the cap high";
        assert_eq!(reason(&clipped, &item, "build"), why);
        assert_eq!(
            clipped.origin(&item, "build", 0).effort.source,
            PlanSource::Sizer
        );
    }
    let unbounded = carry(Sizing::Off { policy_cap: None });
    assert_eq!(
        effort(&unbounded, "build"),
        Effort::XHigh,
        "no cap, no clip"
    );
    let mut pinned = first.clone();
    let by = (Authority::Operator, crate::now());
    pinned.steer(&item, "build", (None, Some(Effort::XHigh)), by);
    let mut retry = PlanWriter::new(&item, crate::now());
    retry.carry(&pinned, &item, None, |_| Some(usize::MAX));
    let kept = retry.finish(on(Effort::High));
    assert_eq!(
        effort(&kept, "build"),
        Effort::XHigh,
        "a pin is never clipped"
    );
}

#[test]
fn clipping_a_stored_plan_lowers_only_sized_efforts_and_records_why() {
    let item = QueueItemId::new();
    let mut planned = critical_build(&item);
    assert!(!planned.clip_sized(&item, None, crate::now()));
    assert!(!planned.clip_sized(&item, Some(Effort::XHigh), crate::now()));
    assert!(planned.clip_sized(&item, Some(Effort::Medium), crate::now()));
    assert_eq!(effort(&planned, "build"), Effort::Medium);
    let why = "sizer: critical -> x_high; clipped from x_high to the cap medium";
    assert_eq!(reason(&planned, &item, "build"), why);
    let mut pinned = critical_build(&item);
    let by = (Authority::Operator, crate::now());
    pinned.steer(&item, "build", (None, Some(Effort::XHigh)), by);
    assert!(!pinned.clip_sized(&item, Some(Effort::Medium), crate::now()));
    assert_eq!(effort(&pinned, "build"), Effort::XHigh);
    let unsized_plan = filled(&item, &["build"], Effort::XHigh, PlanSource::Fallback)
        .finish(Sizing::Off { policy_cap: None });
    let mut left = unsized_plan.clone();
    assert!(!left.clip_sized(&item, Some(Effort::Medium), crate::now()));
    assert_eq!(left, unsized_plan, "an effort nobody sized is as in 1.6.3");
}

/// A changed class sizes again the stages not yet started, and only those.
#[test]
fn a_changed_class_sizes_again_only_the_stages_not_started() {
    let item = QueueItemId::new();
    let mut planned = sized(
        &item,
        &["plan", "build", "check"],
        Effort::Medium,
        "Add a button",
    );
    let before = planned.clone();
    let now = (Effort::XHigh, crate::now());
    let none = |_: &str| false;
    assert!(!planned.resize(&item, input("Add a button", &[]), now, &none));
    assert_eq!(planned, before, "the same class changes nothing");
    let started = |key: &str| key == "plan";
    let risky = tags(&["risk:critical"]);
    assert!(planned.resize(&item, input("Add a button", &risky), now, &started));
    assert_eq!(
        effort(&planned, "plan"),
        Effort::High,
        "started: as it was sized"
    );
    assert_eq!(reason(&planned, &item, "plan"), "sizer: ordinary -> high");
    assert_eq!(effort(&planned, "build"), Effort::XHigh);
    assert_eq!(effort(&planned, "check"), Effort::XHigh);
    let classes = |key: &str| planned.slot_sizing(&item, key, 0).unwrap().risk_class;
    assert_eq!(classes("plan"), RiskClass::Ordinary);
    assert_eq!(classes("build"), RiskClass::Critical);
    assert_eq!(
        planned.sizing().unwrap().risk_class,
        Some(RiskClass::Critical)
    );
}

#[test]
fn a_resize_leaves_a_pin_and_a_key_never_sized_alone() {
    let item = QueueItemId::new();
    let writer = filled(&item, &["plan"], Effort::Max, PlanSource::Fallback);
    let never_sized = writer.finish(Sizing::Off { policy_cap: None });
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&never_sized, &item, None, |_| Some(usize::MAX));
    add(
        &mut kept,
        &["build", "check"],
        Effort::Medium,
        PlanSource::Fallback,
    );
    let mut planned = kept.finish(on(Effort::XHigh));
    planned.settle(&item, input("Add a button", &[]), crate::now());
    let by = (Authority::Operator, crate::now());
    planned.steer(&item, "check", (None, Some(Effort::Low)), by);
    let risky = tags(&["risk:critical"]);
    let now = (Effort::High, crate::now());
    assert!(planned.resize(&item, input("x", &risky), now, &|_| false));
    assert_eq!(effort(&planned, "plan"), Effort::Max, "never sized");
    assert_eq!(effort(&planned, "check"), Effort::Low, "pinned");
    let pinned =
        "sizer: critical -> x_high; capped at high; consider a stronger tier; the plan pins low";
    assert_eq!(reason(&planned, &item, "check"), pinned);
    assert_eq!(
        effort(&planned, "build"),
        Effort::High,
        "sized under the cap of now"
    );
}

/// A steer of the effort pins it from then on, and the record says so.
#[test]
fn an_operator_steer_pins_a_sized_effort_from_then_on() {
    let item = QueueItemId::new();
    let mut planned = critical_build(&item);
    let by = (Authority::Operator, crate::now());
    planned.steer(&item, "build", (None, Some(Effort::Medium)), by);
    let slot = planned.slot_sizing(&item, "build", 0).unwrap();
    assert_eq!(slot.effort_source, PlanSource::Explicit);
    assert_eq!(
        slot.reason,
        "sizer: critical -> x_high; the plan pins medium"
    );
    let reasons = planned.effort_reasons(&item);
    assert_eq!(reasons["build"], [slot.reason]);
}

/// A copy for another item keeps what was sized and sizes what was not from
/// the fields of the item it is stored for.
#[test]
fn a_copy_keeps_frozen_keys_and_sizes_pending_ones_for_its_own_item() {
    let (owner, copy) = (QueueItemId::new(), QueueItemId::new());
    let frozen = sized(&owner, &["build"], Effort::Medium, "Fix the mutex");
    let copied = frozen.copy_for(&owner, &copy, Some("copied"));
    assert_eq!(effort(&copied, "build"), Effort::High);
    assert_eq!(reason(&copied, &copy, "build"), "sizer: elevated -> high");
    let mut again = copied.clone();
    assert!(!again.settle(&copy, input("Add a button", &[]), crate::now()));
    let pending = filled(&owner, &["build"], Effort::Medium, PlanSource::Fallback);
    let mut copied = pending
        .finish(on(Effort::XHigh))
        .copy_for(&owner, &copy, None);
    assert!(copied.has_unsized());
    assert!(copied.settle(&copy, input("docs: tidy", &[]), crate::now()));
    assert_eq!(
        effort(&copied, "build"),
        Effort::Low,
        "the copy's own title"
    );
}

/// The sizer keeps what a key's writer said of it, and says what the effort
/// was once however often the key is sized again.
#[test]
fn a_sized_effort_s_origin_keeps_its_writer_s_note_and_one_was() {
    let (owner, copy) = (QueueItemId::new(), QueueItemId::new());
    let pending = filled(&owner, &["build"], Effort::Medium, PlanSource::Fallback);
    let said = Some("for the refresh");
    let mut copied = pending
        .finish(on(Effort::XHigh))
        .copy_for(&owner, &copy, said);
    let note = |planned: &PlannedPlan| planned.origin(&copy, "build", 0).effort.note.unwrap();
    assert!(copied.settle(&copy, input("Fix the mutex", &[]), crate::now()));
    assert_eq!(note(&copied), "for the refresh; was fallback: medium");
    let risky = tags(&["risk:critical"]);
    let now = (Effort::XHigh, crate::now());
    assert!(copied.resize(&copy, input("Fix the mutex", &risky), now, &|_| false));
    assert_eq!(note(&copied), "for the refresh; was sizer: high");
    let mut plain = sized(&owner, &["build"], Effort::Medium, "Fix the mutex");
    assert_eq!(
        plain.origin(&owner, "build", 0).effort.note.unwrap(),
        "was fallback: medium"
    );
    assert!(plain.resize(&owner, input("Fix the mutex", &risky), now, &|_| false));
    assert_eq!(
        plain.origin(&owner, "build", 0).effort.note.unwrap(),
        "was sizer: high"
    );
}

#[test]
fn removing_a_key_drops_its_sizing_and_a_record_that_does_not_fit_reads_as_none() {
    let item = QueueItemId::new();
    let mut planned = sized(&item, &["plan", "build"], Effort::Medium, "Add a button");
    assert!(planned.remove_key("plan"));
    let keys: Vec<&String> = planned.sizing().unwrap().keys.keys().collect();
    assert_eq!(keys, ["build"]);
    assert!(planned.remove_key("build"));
    assert!(planned.sizing().is_none());
    let whole = sized(&item, &["build"], Effort::Medium, "Add a button");
    let mut stored = serde_json::to_value(&whole).unwrap();
    assert_eq!(
        serde_json::from_value::<PlannedPlan>(stored.clone()).unwrap(),
        whole
    );
    stored["effort_sizing"]["keys"]["gone"] = stored["effort_sizing"]["keys"]["build"].clone();
    let misfit: PlannedPlan = serde_json::from_value(stored.clone()).unwrap();
    assert!(
        misfit.sizing().is_none(),
        "a record of another plan describes none"
    );
    stored["effort_sizing"] = serde_json::json!("garbled");
    let garbled: PlannedPlan = serde_json::from_value(stored).unwrap();
    assert!(garbled.sizing().is_none());
    assert_eq!(garbled.plan(), whole.plan());
}

/// The diff size is frozen with the first sizing; only a tag overrides it.
#[test]
fn the_diff_size_known_at_the_first_sizing_is_the_one_every_later_key_reads() {
    let item = QueueItemId::new();
    let sizer = |size| {
        Sizing::On(EffortSizer {
            cap: Effort::XHigh,
            size,
            facts: &facts,
        })
    };
    let writer = filled(&item, &["build"], Effort::Medium, PlanSource::Fallback);
    let mut first = writer.finish(sizer(Some(DiffSize::Small)));
    first.settle(&item, input("Add a button", &[]), crate::now());
    assert_eq!(
        reason(&first, &item, "build"),
        "sizer: ordinary -> low (small)"
    );
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&first, &item, None, |_| Some(usize::MAX));
    add(&mut kept, &["plan"], Effort::Medium, PlanSource::Fallback);
    let mut grown = kept.finish(sizer(Some(DiffSize::Large)));
    grown.settle(&item, input("Add a button", &[]), crate::now());
    assert_eq!(
        reason(&grown, &item, "plan"),
        "sizer: ordinary -> medium (small)"
    );
    assert_eq!(effort(&grown, "build"), Effort::Low, "not sized again");
}

/// Each key is stale by what it was sized with: sizing a stage added later
/// under new signals does not make the carried stages look current.
#[test]
fn sizing_a_new_key_hides_no_moved_signal_from_the_keys_sized_before() {
    let item = QueueItemId::new();
    let small = tags(&["diff:small"]);
    let small_input = RiskInput {
        size: Some(DiffSize::Small),
        ..input("Add a button", &small)
    };
    let writer = filled(&item, &["build"], Effort::Medium, PlanSource::Fallback);
    let mut first = writer.finish(on(Effort::XHigh));
    first.settle(&item, small_input, crate::now());
    assert_eq!(effort(&first, "build"), Effort::Low, "one step down");

    // The tag is gone and a stage is added: the new key is sized first.
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&first, &item, None, |_| Some(usize::MAX));
    add(&mut kept, &["plan"], Effort::Medium, PlanSource::Fallback);
    let mut grown = kept.finish(on(Effort::XHigh));
    let plain = input("Add a button", &[]);
    assert!(grown.settle(&item, plain, crate::now()));
    assert_eq!(effort(&grown, "plan"), Effort::High, "its own default");
    assert_eq!(
        effort(&grown, "build"),
        Effort::Low,
        "settling sizes no other"
    );
    let now = (Effort::XHigh, crate::now());
    assert!(grown.resize(&item, plain, now, &|_| false));
    assert_eq!(effort(&grown, "build"), Effort::Medium, "no longer small");
    assert_eq!(reason(&grown, &item, "build"), "sizer: ordinary -> medium");
    assert!(!grown.resize(&item, plain, now, &|_| false), "all current");
}

/// Whoever moves the model of a sized `x_high` (a steer naming a model, a
/// provider the item follows), the stored plan holds what the engine runs.
#[test]
fn a_sized_x_high_whose_model_moves_to_another_engine_is_stored_at_high() {
    let item = QueueItemId::new();
    let other: crate::model::ModelRef = "gemini/gemini-pro".parse().unwrap();
    let by = (Authority::Operator, crate::now());
    let mut steered = critical_build(&item);
    steered.steer(&item, "build", (Some(&other), None), by);
    assert_eq!(effort(&steered, "build"), Effort::High);
    let why = "sizer: critical -> x_high; gemini runs no more than high, so high";
    assert_eq!(reason(&steered, &item, "build"), why);
    assert_eq!(
        steered.origin(&item, "build", 0).effort.source,
        PlanSource::Sizer,
        "still the daemon's effort"
    );

    // An effort the steer names is a pin: it stays what was asked.
    let mut pinned = critical_build(&item);
    pinned.steer(&item, "build", (Some(&other), Some(Effort::XHigh)), by);
    assert_eq!(effort(&pinned, "build"), Effort::XHigh);
    assert!(pinned.origin(&item, "build", 0).effort.is_pin());
    assert!(!pinned.clip_sized(&item, None, crate::now()), "never a pin");
    assert!(!steered.clip_sized(&item, None, crate::now()), "said once");
}

/// A carried sized effort is held to the cap as it is carried, so the fill
/// step that borrows it for a new stage borrows nothing above the cap.
#[test]
fn a_carried_sized_effort_is_within_the_cap_before_any_fill_step_reads_it() {
    let item = QueueItemId::new();
    let first = critical_build(&item);
    let mut kept = PlanWriter::new(&item, crate::now());
    kept.carry(&first, &item, Some(Effort::High), |_| Some(usize::MAX));
    let borrowed = kept.plan().choices_for("build").unwrap()[0].clone();
    assert_eq!(borrowed.effort, Effort::High, "what a donor fill reads");
    let mut plan = kept.plan().clone();
    plan.0.insert("extra".to_owned(), vec![borrowed]);
    kept.filled(plan, (PlanSource::Fallback, PlanSource::Fallback));
    let grown = kept.finish(Sizing::Off {
        policy_cap: Some(Effort::High),
    });
    assert_eq!(effort(&grown, "extra"), Effort::High);
    let why = "sizer: critical -> x_high; clipped from x_high to the cap high";
    assert_eq!(reason(&grown, &item, "build"), why, "said once");
}
