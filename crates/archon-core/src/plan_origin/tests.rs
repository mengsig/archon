use super::*;
use crate::model_tier::Tier;

fn choice(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice::pinned(model.parse().unwrap(), effort)
}

fn tiered(model: &str, effort: Effort) -> ModelChoice {
    ModelChoice {
        tier: Some(TierChoice {
            requested: Tier::Strong,
            provider: None,
            resolved: Tier::Strong,
        }),
        ..choice(model, effort)
    }
}

fn plan(entries: &[(&str, ModelChoice)]) -> ModelPlan {
    ModelPlan(
        entries
            .iter()
            .map(|(key, choice)| ((*key).to_owned(), vec![choice.clone()]))
            .collect(),
    )
}

fn draft(json: serde_json::Value) -> ModelPlanDraft {
    serde_json::from_value(json).unwrap()
}

/// An enqueue naming `build`'s model only, the rest filled by the daemon.
fn enqueued(item: &QueueItemId) -> PlannedPlan {
    let mut writer = PlanWriter::new(item, crate::now());
    let asked = draft(serde_json::json!({"build": [{"model": "a/opus"}]}));
    writer.request(
        &asked,
        plan(&[("build", choice("a/opus", Effort::Medium))]),
        Authority::Operator,
    );
    let mut learned = writer.plan().clone();
    learned
        .0
        .insert("plan".into(), vec![choice("b/terra", Effort::Low)]);
    writer.filled(learned, (PlanSource::Learned, PlanSource::Learned));
    let mut tiers = writer.plan().clone();
    tiers
        .0
        .insert("review".into(), vec![tiered("b/sol", Effort::High)]);
    writer.filled(tiers, (PlanSource::Tier, PlanSource::Fallback));
    let mut fallen = writer.plan().clone();
    fallen
        .0
        .insert("ship".into(), vec![choice("a/opus", Effort::Medium)]);
    writer.filled(fallen, (PlanSource::Fallback, PlanSource::Fallback));
    writer.finish(Sizing::Off { policy_cap: None })
}

#[test]
fn a_named_model_is_explicit_and_every_other_field_names_the_step_that_filled_it() {
    let item = QueueItemId::new();
    let planned = enqueued(&item);
    let build = planned.origin(&item, "build", 0);
    assert_eq!(
        (build.model.source, build.model.by),
        (PlanSource::Explicit, Authority::Operator)
    );
    assert_eq!(
        (build.effort.source, build.effort.by),
        (PlanSource::Fallback, Authority::Daemon)
    );
    assert!(
        build.model.is_pin() && !build.effort.is_pin(),
        "only the named field is a pin"
    );
    let sources = |key| {
        let origin = planned.origin(&item, key, 0);
        (origin.model.source, origin.effort.source)
    };
    assert_eq!(sources("plan"), (PlanSource::Learned, PlanSource::Learned));
    assert_eq!(sources("review"), (PlanSource::Tier, PlanSource::Fallback));
    assert_eq!(
        sources("ship"),
        (PlanSource::Fallback, PlanSource::Fallback)
    );
    assert_eq!(planned.fault(&item), None);
    assert_eq!(planned.unattributed_fields(&item), 0);
}

#[test]
fn a_draft_naming_a_tier_leaves_the_model_to_the_daemon_and_names_who_asked() {
    let item = QueueItemId::new();
    let mut writer = PlanWriter::new(&item, crate::now());
    let asked = draft(serde_json::json!({"review": [{"tier": "strong", "effort": "high"}]}));
    let resolved = plan(&[("review", tiered("b/sol", Effort::High))]);
    writer.request(&asked, resolved, Authority::Orchestrator);
    let origin = writer
        .finish(Sizing::Off { policy_cap: None })
        .origin(&item, "review", 0);
    assert_eq!(
        (origin.model.source, origin.model.by),
        (PlanSource::Tier, Authority::Daemon)
    );
    let tier = origin
        .tier
        .as_ref()
        .expect("a tiered choice has a tier origin");
    assert_eq!(
        (tier.source, tier.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    assert_eq!(
        (origin.effort.source, origin.effort.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
}

#[test]
fn a_moved_tier_keeps_who_asked_for_it_however_often_it_moves() {
    let item = QueueItemId::new();
    let mut writer = PlanWriter::new(&item, crate::now());
    let asked = draft(serde_json::json!({"review": [{"tier": "strong"}]}));
    let resolved = plan(&[("review", tiered("b/sol", Effort::High))]);
    writer.request(&asked, resolved, Authority::Orchestrator);
    let mut planned = writer.finish(Sizing::Off { policy_cap: None });
    let asked = planned.origin(&item, "review", 0).tier;
    assert!(asked.is_some());
    for (model, why) in [("c/sol", "moved off b"), ("d/sol", "moved off c")] {
        let to = |_: &str, asked: &TierChoice| Ok(Some((model.parse().unwrap(), asked.clone())));
        planned
            .retier(&item, (crate::now(), Some(why), TierMove::InRun), to)
            .unwrap();
        let origin = planned.origin(&item, "review", 0);
        assert_eq!(origin.model.note.as_deref(), Some(why));
        assert_eq!(origin.tier, asked, "the tier still names who asked");
    }
}

#[test]
fn a_tier_the_daemon_chose_keeps_only_the_latest_move_s_note() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    assert_eq!(planned.origin(&item, "review", 0).model.note, None);
    for (model, why) in [
        ("c/sol", "moved to c"),
        ("b/sol", "moved to b"),
        ("c/sol", "moved to c"),
    ] {
        let to = |_: &str, asked: &TierChoice| Ok(Some((model.parse().unwrap(), asked.clone())));
        planned
            .retier(&item, (crate::now(), Some(why), TierMove::InRun), to)
            .unwrap();
        let note = planned.origin(&item, "review", 0).model.note;
        assert_eq!(note.as_deref(), Some(why));
    }
}

#[test]
fn a_steer_naming_the_stored_value_still_pins_it_and_leaves_the_other_field_alone() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    let before = planned.origin(&item, "build", 0);
    let named = (None, Some(Effort::Medium));
    let steered = planned.steer(
        &item,
        "build",
        named,
        (Authority::Orchestrator, crate::now()),
    );
    assert_eq!(
        steered.unwrap()[0],
        choice("a/opus", Effort::Medium),
        "the value is unchanged"
    );
    let after = planned.origin(&item, "build", 0);
    assert_eq!(
        after.model, before.model,
        "a steer of the effort leaves the model's origin"
    );
    assert_eq!(
        (after.effort.source, after.effort.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
    assert_eq!(after.effort.note.as_deref(), Some("was fallback: medium"));
    let model: ModelRef = "b/sol".parse().unwrap();
    planned.steer(
        &item,
        "review",
        (Some(&model), None),
        (Authority::Operator, crate::now()),
    );
    assert_eq!(
        planned.choices_for("review").unwrap()[0].tier,
        None,
        "a named model drops the tier"
    );
    let review = planned.origin(&item, "review", 0);
    assert_eq!(
        (review.model.source, review.model.by),
        (PlanSource::Explicit, Authority::Operator)
    );
    assert_eq!(review.effort.source, PlanSource::Fallback);
    assert!(
        planned
            .steer(&item, "nope", named, (Authority::Operator, crate::now()))
            .is_none()
    );
}

#[test]
fn a_kept_plan_carries_its_origins_and_a_copy_keeps_them_with_a_note() {
    let item = QueueItemId::new();
    let planned = enqueued(&item);
    let mut retry = PlanWriter::new(&item, crate::now());
    retry.carry(&planned, &item, None, |key| {
        (key != "ship").then_some(usize::MAX)
    });
    let retried = retry.finish(Sizing::Off { policy_cap: None });
    for key in ["build", "plan", "review"] {
        assert_eq!(
            retried.origin(&item, key, 0),
            planned.origin(&item, key, 0),
            "{key}"
        );
    }
    assert!(
        retried.choices_for("ship").is_none(),
        "a key the limit drops is gone"
    );
    let fix = QueueItemId::new();
    let copied = planned.copy_for(&item, &fix, Some("copied from x"));
    let origin = copied.origin(&fix, "build", 0);
    assert_eq!(
        (origin.model.source, origin.model.by),
        (PlanSource::Explicit, Authority::Operator)
    );
    assert_eq!(origin.model.note.as_deref(), Some("copied from x"));
    assert_eq!(copied.fault(&fix), None);
}

#[test]
fn a_retier_that_moves_a_model_records_when_and_why() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    let first = planned.origin(&item, "review", 0);
    let same = |_: &str, asked: &TierChoice| Ok(Some(("b/sol".parse().unwrap(), asked.clone())));
    planned
        .retier(&item, (crate::now(), Some("unused"), TierMove::InRun), same)
        .unwrap();
    assert_eq!(
        planned.origin(&item, "review", 0),
        first,
        "an unchanged model keeps its origin"
    );
    let moved = |_: &str, asked: &TierChoice| Ok(Some(("c/sol".parse().unwrap(), asked.clone())));
    planned
        .retier(
            &item,
            (crate::now(), Some("moved off b"), TierMove::InRun),
            moved,
        )
        .unwrap();
    let origin = planned.origin(&item, "review", 0);
    assert_eq!(origin.model.source, PlanSource::Tier);
    assert_eq!(origin.model.note.as_deref(), Some("moved off b"));
    assert_eq!(
        planned.origin(&item, "build", 0).model.source,
        PlanSource::Explicit,
        "a pin is never moved"
    );
}

#[test]
#[should_panic(expected = "a fill step keeps")]
fn a_fill_step_that_changes_an_entry_it_was_given_is_refused() {
    let item = QueueItemId::new();
    let mut writer = PlanWriter::new(&item, crate::now());
    writer.filled(
        plan(&[("build", choice("a/opus", Effort::Low))]),
        (PlanSource::Learned, PlanSource::Learned),
    );
    writer.filled(
        plan(&[("build", choice("a/opus", Effort::High))]),
        (PlanSource::Fallback, PlanSource::Fallback),
    );
}

#[test]
fn a_retier_that_fails_part_way_leaves_the_plan_and_its_origins_as_they_were() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    let mut tiers = planned.plan().clone();
    tiers
        .0
        .insert("verify".into(), vec![tiered("b/sol", Effort::Low)]);
    let mut writer = PlanWriter::new(&item, crate::now());
    writer.carry(&planned, &item, None, |_| Some(usize::MAX));
    writer.filled(tiers, (PlanSource::Tier, PlanSource::Fallback));
    planned = writer.finish(Sizing::Off { policy_cap: None });
    let before = planned.clone();
    let mut calls = 0;
    let failing = |_: &str, asked: &TierChoice| {
        calls += 1;
        match calls {
            1 => Ok(Some(("c/moved".parse().unwrap(), asked.clone()))),
            _ => Err(ArchonError::Invalid("no provider".into())),
        }
    };
    assert!(
        planned
            .retier(&item, (crate::now(), None, TierMove::InRun), failing)
            .is_err()
    );
    assert_eq!(
        planned, before,
        "the first move is not kept without its origin"
    );
}

/// Finding of an earlier review: a reroute of a tier the daemon chose records
/// the daemon and this move alone, never an earlier note as who asked.
#[test]
fn a_reroute_of_a_daemon_chosen_tier_records_the_daemon_afresh() {
    let item = QueueItemId::new();
    let mut stored = serde_json::to_value(enqueued(&item)).unwrap();
    let earlier = "moved off x; tier strong asked by the operator";
    stored["model_origins"]["entries"]["review"][0]["model"]["note"] = earlier.into();
    let mut planned: PlannedPlan = serde_json::from_value(stored).unwrap();
    let before = planned.origin(&item, "review", 0);
    let later = crate::now() + chrono::TimeDelta::minutes(5);
    let to = |_: &str, asked: &TierChoice| Ok(Some(("c/sol".parse().unwrap(), asked.clone())));
    planned
        .retier(&item, (later, Some("moved off b"), TierMove::InRun), to)
        .unwrap();
    let origin = planned.origin(&item, "review", 0);
    assert_eq!(
        (origin.model.source, origin.model.by),
        (PlanSource::Tier, Authority::Daemon)
    );
    assert_eq!(origin.model.note.as_deref(), Some("moved off b"));
    assert_eq!(origin.model.at, Some(later), "a fresh time");
    assert_eq!(
        origin.tier, before.tier,
        "the tier's own origin is untouched"
    );
    let tier = origin.tier.expect("a tiered choice has a tier origin");
    assert_eq!(
        (tier.source, tier.by, tier.note.as_deref()),
        (
            PlanSource::Fallback,
            Authority::Daemon,
            Some("the stage's tier")
        ),
        "nobody asked for a tier the daemon chose"
    );
}

#[test]
fn removing_a_key_drops_its_origins() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    assert!(planned.remove_key("ship"));
    assert!(!planned.remove_key("ship"));
    assert_eq!(planned.fault(&item), None, "origins still fit the plan");
    assert!(!planned.origins(&item).contains_key("ship"));
    assert_eq!(planned.unattributed_fields(&item), 0);
}

/// S1: origins that do not fit vouch for nothing after a key is dropped
/// either, whether the dropped key or another one held the fault.
#[test]
fn removing_a_key_never_turns_faulty_origins_into_fitting_ones() {
    let faults: [(&str, Edit); 4] = [
        ("a stray origin", |i| without(i, "/model_plan", "ship")),
        ("no origin to drop", |i| {
            without(i, "/model_origins/entries", "ship")
        }),
        ("a misfit to drop", |i| {
            i["model_origins"]["entries"]["ship"] = [0; 0].into()
        }),
        ("a misfit elsewhere", |i| {
            i["model_origins"]["entries"]["plan"] = [0; 0].into()
        }),
    ];
    let item = QueueItemId::new();
    for (why, edit) in faults {
        let mut stored = serde_json::to_value(enqueued(&item)).unwrap();
        edit(&mut stored);
        let mut planned: PlannedPlan = serde_json::from_value(stored).unwrap();
        assert!(planned.fault(&item).is_some(), "{why}");
        let had = planned.0.contains_key("ship");
        assert_eq!(planned.remove_key("ship"), had, "{why}");
        assert!(!planned.0.contains_key("ship"), "{why}");
        assert_all(&planned, &item, PlanSource::Unattributed, why);
        assert!(planned.fault(&item).is_some(), "{why}: still a fault");
    }
}

/// Every field of `planned` reads as `source`, by nobody known.
fn assert_all(planned: &PlannedPlan, item: &QueueItemId, source: PlanSource, why: &str) {
    let origins = planned.origins(item);
    assert!(origins.keys().eq(planned.0.keys()), "{why}");
    for (key, slots) in &origins {
        assert_eq!(slots.len(), planned.0[key].len(), "{why}: {key}");
        for (origin, choice) in slots.iter().zip(&planned.0[key]) {
            assert_eq!(origin.tier.is_some(), choice.tier.is_some(), "{why}");
            for field in origin.fields() {
                assert_eq!(
                    (field.source, field.by),
                    (source, Authority::Unknown),
                    "{why}"
                );
                assert!(field.is_pin() && field.at.is_none(), "{why}");
            }
        }
    }
}

type Edit = fn(&mut serde_json::Value);

/// Removes `key` from the object at `pointer` of the stored origins.
fn without(origins: &mut serde_json::Value, pointer: &str, key: &str) {
    let object = origins.pointer_mut(pointer).and_then(|v| v.as_object_mut());
    let removed = object.expect("an object there").remove(key);
    assert!(removed.is_some(), "{pointer} had no {key}");
}

/// Every way stored origins fail to decode or to describe their plan.
fn misfits() -> Vec<(&'static str, Edit)> {
    let other_item: Edit = |o| o["item"] = QueueItemId::new().to_string().into();
    let extra_key: Edit = |o| o["entries"]["verify"] = o["entries"]["plan"].clone();
    let unknown_source: Edit = |o| o["entries"]["plan"][0]["model"]["source"] = "comparison".into();
    let unknown_authority: Edit = |o| o["entries"]["plan"][0]["model"]["by"] = "someone".into();
    let by_nobody: Edit = |o| o["entries"]["plan"][0]["model"]["by"] = "unknown".into();
    let extra_tier: Edit =
        |o| o["entries"]["build"][0]["tier"] = o["entries"]["review"][0]["tier"].clone();
    vec![
        ("no item", |o| without(o, "", "item")),
        ("no entries", |o| without(o, "", "entries")),
        ("entries of another type", |o| o["entries"] = "x".into()),
        ("malformed", |o| *o = serde_json::json!([1, 2])),
        ("null", |o| *o = serde_json::Value::Null),
        ("another item", other_item),
        ("a key without origins", |o| without(o, "/entries", "plan")),
        ("a key the plan lacks", extra_key),
        ("another entry count", |o| {
            o["entries"]["plan"] = [0; 0].into()
        }),
        ("no effort origin", |o| {
            without(o, "/entries/build/0", "effort")
        }),
        ("an unknown source", unknown_source),
        ("an unknown authority", unknown_authority),
        ("no authority", |o| {
            without(o, "/entries/plan/0/model", "by")
        }),
        ("an attributed source by nobody", by_nobody),
        ("no time", |o| without(o, "/entries/plan/0/effort", "at")),
        ("no tier origin for a tier", |o| {
            without(o, "/entries/review/0", "tier")
        }),
        ("a tier origin for no tier", extra_tier),
    ]
}

/// S1: stored origins that fail to decode in any way, do not fit the plan or
/// are bound to another item read every field of the item as unattributed.
#[test]
fn stored_origins_that_do_not_decode_or_fit_read_every_field_as_unattributed() {
    let item = QueueItemId::new();
    let sound = enqueued(&item);
    assert_eq!(sound.unattributed_fields(&item), 0);
    for (why, edit) in misfits() {
        let mut stored = serde_json::to_value(&sound).unwrap();
        edit(&mut stored["model_origins"]);
        let planned: PlannedPlan = serde_json::from_value(stored).expect("the item stays readable");
        assert_eq!(planned.plan(), sound.plan(), "{why}");
        assert_all(&planned, &item, PlanSource::Unattributed, why);
        let unattributed = planned.unattributed_fields(&item);
        assert_eq!(unattributed, 9, "{why}: no field is kept");
        assert_eq!(planned.legacy_fields(&item), 0, "{why}");
        assert!(planned.fault(&item).is_some(), "{why}");
    }
}

/// S1: a row with no `model_origins` reads every field as `legacy_unknown`,
/// and is written back as it was.
#[test]
fn a_plan_stored_without_origins_reads_every_field_as_legacy() {
    let stored = serde_json::json!({
        "model_plan": {"build": [{"model": {"provider": "a", "model": "opus"}, "effort": "high"}],
                       "review": [{"model": {"provider": "b", "model": "sol"}, "effort": "low",
                                   "tier": {"requested": "strong", "resolved": "strong"}}]}
    });
    let planned: PlannedPlan = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(serde_json::to_value(&planned).unwrap(), stored);
    let item = QueueItemId::new();
    assert_all(&planned, &item, PlanSource::LegacyUnknown, "no origins");
    assert_eq!(planned.legacy_fields(&item), 5, "two entries and one tier");
    assert_eq!(planned.unattributed_fields(&item), 0);
    assert_eq!(planned.fault(&item), None);
}

/// S2: the daemon may change a field only when it filled it itself.
#[test]
fn only_a_field_the_daemon_filled_itself_is_movable() {
    use Authority::{Daemon, Operator, Orchestrator, Unknown};
    use PlanSource::*;
    let fills = [Learned, Tier, Fallback, Sizer, Escalation, Migrated];
    let pins = [Explicit, LegacyUnknown, Unattributed];
    for source in fills.into_iter().chain(pins) {
        for by in [Daemon, Operator, Orchestrator, Unknown] {
            let field = FieldOrigin {
                source,
                by,
                at: Some(crate::now()),
                note: None,
            };
            let movable = fills.contains(&source) && by == Daemon;
            assert_eq!(field.is_movable(), movable, "{source} by {by}");
            assert_eq!(field.is_pin(), !movable, "{source} by {by}");
        }
    }
    let item = QueueItemId::new();
    let mut stored = serde_json::to_value(enqueued(&item)).unwrap();
    stored["model_origins"]["entries"]["review"][0]["model"]["by"] = "operator".into();
    let planned: PlannedPlan = serde_json::from_value(stored).unwrap();
    let review = planned.origin(&item, "review", 0);
    assert_eq!(review.model.source, PlanSource::Tier, "it decodes whole");
    assert!(review.model.is_pin() && !review.may_move(TierMove::InRun));
    assert!(!review.may_move(TierMove::Dispatch), "nor at dispatch");
}

fn refuse(_: &str, _: &TierChoice) -> Result<Option<(ModelRef, TierChoice)>, ArchonError> {
    panic!("a pin is not resolved again")
}

/// A tiered review under each kind of pinned tier, and whether its model is
/// already the daemon's to move in a run.
fn pinned_tiers(item: &QueueItemId, tiers: &ModelPlan) -> [(&'static str, PlannedPlan, bool); 3] {
    let mut asked = PlanWriter::new(item, crate::now());
    let named = draft(serde_json::json!({"review": [{"tier": "strong", "effort": "low"}]}));
    let resolved = plan(&[("review", tiered("b/sol", Effort::Low))]);
    asked.request(&named, resolved, Authority::Operator);
    asked.filled(tiers.clone(), (PlanSource::Fallback, PlanSource::Fallback));
    let unattributed = PlannedPlan::unattributed(item, tiers.clone());
    [
        ("legacy", PlannedPlan::legacy(tiers.clone()), false),
        ("unattributed", unattributed, false),
        (
            "explicit",
            asked.finish(Sizing::Off { policy_cap: None }),
            true,
        ),
    ]
}

/// Dispatches `planned` onto `c/sol`: only its tiered review is resolved,
/// and only the review's model changes hands.
fn dispatch_resolves_the_review(planned: &mut PlannedPlan, item: &QueueItemId, why: &str) {
    let before = planned.clone();
    let at = crate::now();
    let mut seen = Vec::new();
    let dispatch = |key: &str, asked: &TierChoice| {
        seen.push(key.to_owned());
        Ok(Some(("c/sol".parse().unwrap(), asked.clone())))
    };
    let when = (at, None, TierMove::Dispatch);
    planned.retier(item, when, dispatch).unwrap();
    assert_eq!(seen, ["review"], "{why}: only a tier is resolved");
    let (was, now) = (
        before.origin(item, "review", 0),
        planned.origin(item, "review", 0),
    );
    let model = (now.model.source, now.model.by, now.model.at);
    assert_eq!(
        model,
        (PlanSource::Tier, Authority::Daemon, Some(at)),
        "{why}"
    );
    assert_eq!(now.tier, was.tier, "{why}: the tier is what was pinned");
    assert_eq!(now.effort, was.effort, "{why}");
    assert_eq!(
        planned.0["review"][0].tier, before.0["review"][0].tier,
        "{why}"
    );
    let build = planned.origin(item, "build", 0);
    assert_eq!(build, before.origin(item, "build", 0), "{why}: untiered");
}

/// S3: a tier that is pinned is resolved at dispatch, where the model
/// becomes the daemon's; before that no move in a run touches it, and after
/// it moves within the same tier only.
#[test]
fn a_pinned_tier_is_resolved_at_dispatch_and_then_moves_only_in_its_tier() {
    let item = QueueItemId::new();
    let tiers = plan(&[
        ("build", choice("a/opus", Effort::Medium)),
        ("review", tiered("b/sol", Effort::Low)),
    ]);
    for (why, mut planned, moves_before) in pinned_tiers(&item, &tiers) {
        let pinned = planned.origin(&item, "review", 0).tier.expect("a tier");
        assert!(pinned.is_pin(), "{why}");
        let in_run = planned.dispatched(&item, TierMove::InRun);
        assert_eq!(in_run.0["review"][0].tier.is_some(), moves_before, "{why}");
        if !moves_before {
            let follow = (crate::now(), Some("follow"), TierMove::InRun);
            planned.retier(&item, follow, refuse).unwrap();
        }
        let held = planned.dispatched(&item, TierMove::Dispatch);
        assert!(held.0["review"][0].tier.is_some(), "{why}: held as a tier");

        dispatch_resolves_the_review(&mut planned, &item, why);

        let moved =
            |_: &str, asked: &TierChoice| Ok(Some(("d/sol".parse().unwrap(), asked.clone())));
        let follow = (crate::now(), Some("follow"), TierMove::InRun);
        planned.retier(&item, follow, moved).unwrap();
        let review = &planned.0["review"][0];
        assert_eq!(review.model.to_string(), "d/sol", "{why}");
        assert_eq!(review.tier, tiers.0["review"][0].tier, "{why}");
        assert_eq!(planned.fault(&item), None, "{why}");
    }
}

/// S3 has no other exception: an explicit model under a tier the daemon
/// chose is a pin at dispatch too, and nothing unchanged is rewritten.
#[test]
fn a_pinned_model_under_a_daemon_tier_is_never_resolved() {
    let item = QueueItemId::new();
    let mut stored = serde_json::to_value(enqueued(&item)).unwrap();
    let model = &mut stored["model_origins"]["entries"]["review"][0]["model"];
    model["source"] = "explicit".into();
    model["by"] = "operator".into();
    let mut planned: PlannedPlan = serde_json::from_value(stored).unwrap();
    let before = planned.clone();
    for when in [TierMove::Dispatch, TierMove::InRun] {
        assert!(!planned.origin(&item, "review", 0).may_move(when));
        assert_eq!(planned.dispatched(&item, when).0["review"][0].tier, None);
        planned
            .retier(&item, (crate::now(), None, when), refuse)
            .unwrap();
    }
    assert_eq!(planned, before);
    let mut legacy = PlannedPlan::legacy(plan(&[("build", choice("a/opus", Effort::Low))]));
    legacy
        .retier(&item, (crate::now(), None, TierMove::Dispatch), refuse)
        .unwrap();
    let stored = serde_json::to_value(&legacy).unwrap();
    assert!(
        stored.get("model_origins").is_none(),
        "left as 1.6.3 stored it"
    );
}

/// The store's guard: origins a clone kept under another id describe no
/// field of it, and a save replaces them whole.
#[test]
fn origins_kept_by_a_clone_under_another_id_read_as_unattributed_pins() {
    let item = QueueItemId::new();
    let planned = enqueued(&item);
    let clone = QueueItemId::new();
    let fault = planned.fault(&clone).expect("written for another item");
    assert!(fault.starts_with("origins written for"), "{fault}");
    assert_all(&planned, &clone, PlanSource::Unattributed, "a clone");
    let reconciled = planned.reconciled(&clone).expect("a fault to reconcile");
    assert_eq!(reconciled.fault(&clone), None);
    assert_eq!(
        reconciled.unattributed_fields(&clone),
        9,
        "two fields of four entries and one tier"
    );
    assert!(
        planned.reconciled(&item).is_none(),
        "nothing to fix on its own item"
    );
}

#[test]
fn every_source_is_stored_under_the_name_it_is_shown_with() {
    use PlanSource::*;
    let sources = [
        Explicit,
        Learned,
        Tier,
        Fallback,
        Sizer,
        Escalation,
        Migrated,
        LegacyUnknown,
        Unattributed,
    ];
    for source in sources {
        let stored = serde_json::to_value(source).unwrap();
        assert_eq!(stored, serde_json::json!(source.to_string()));
        let read: PlanSource = serde_json::from_value(stored).unwrap();
        assert_eq!(read, source);
    }
    assert!(serde_json::from_str::<PlanSource>("\"comparison\"").is_err());
}

#[test]
fn catalog_repair_moves_daemon_entries_and_keeps_pins_atomic() {
    use crate::model_tier::{CatalogModel, ModelCatalog};
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    let row = |model: &str| CatalogModel {
        model: model.parse().unwrap(),
        tier: Some(Tier::Strong),
        is_default: false,
    };
    let mut catalog = ModelCatalog::new(vec![row("a/opus"), row("b/new"), row("b/sol")]);
    catalog.add_retired("b/terra".parse().unwrap(), Some("b/new".parse().unwrap()));
    planned
        .repair_catalog(&item, &catalog, crate::now())
        .unwrap();
    assert_eq!(planned.0["plan"][0].model, "b/new".parse().unwrap());
    assert_eq!(
        planned.origin(&item, "plan", 0).model.source,
        PlanSource::Migrated
    );
    let before = planned.clone();
    catalog = {
        let mut only_new = ModelCatalog::new(vec![row("b/new"), row("b/sol")]);
        only_new.add_retired("a/opus".parse().unwrap(), Some("b/new".parse().unwrap()));
        only_new
    };
    assert!(
        planned
            .repair_catalog(&item, &catalog, crate::now())
            .is_err()
    );
    assert_eq!(
        planned, before,
        "a pinned missing model leaves every entry unchanged"
    );
    assert!(
        planned
            .repair_catalog(&item, &ModelCatalog::unavailable("offline"), crate::now())
            .is_err()
    );
    assert_eq!(planned, before);
}

#[test]
fn an_empty_or_unavailable_catalog_never_admits_even_an_empty_plan() {
    use crate::model_tier::ModelCatalog;
    let item = QueueItemId::new();
    let mut planned = PlannedPlan::unattributed(&item, ModelPlan::default());
    let before = planned.clone();
    for catalog in [
        ModelCatalog::new(Vec::new()),
        ModelCatalog::unavailable("offline"),
    ] {
        assert!(
            planned
                .repair_catalog(&item, &catalog, crate::now())
                .is_err()
        );
        assert_eq!(planned, before);
    }
}

#[test]
fn migrated_entries_remain_daemon_owned_and_can_move_again() {
    use crate::model_tier::{CatalogModel, ModelCatalog};
    let id = QueueItemId::new();
    let mut writer = PlanWriter::new(&id, crate::now());
    writer.filled(
        plan(&[("build", choice("p/old", Effort::High))]),
        (PlanSource::Fallback, PlanSource::Fallback),
    );
    let mut planned = writer.finish(Sizing::Off { policy_cap: None });
    let model = |id: &str| CatalogModel {
        model: id.parse().unwrap(),
        tier: Some(Tier::Strong),
        is_default: false,
    };
    let mut first = ModelCatalog::new(vec![model("p/next")]);
    first.add_retired("p/old".parse().unwrap(), Some("p/next".parse().unwrap()));
    planned.repair_catalog(&id, &first, crate::now()).unwrap();
    assert_eq!(
        planned.origin(&id, "build", 0).model.source,
        PlanSource::Migrated
    );
    assert!(planned.origin(&id, "build", 0).model.is_movable());
    let mut second = ModelCatalog::new(vec![model("p/final")]);
    second.add_retired("p/next".parse().unwrap(), Some("p/final".parse().unwrap()));
    planned.repair_catalog(&id, &second, crate::now()).unwrap();
    assert_eq!(planned.0["build"][0].model.to_string(), "p/final");
    assert_eq!(planned.0["build"][0].effort, Effort::High);
}

/// A daemon choice the catalog supersedes yet still lists keeps running when
/// no eligible model replaces it; a retired one with none still refuses.
#[test]
fn a_superseded_daemon_choice_with_no_replacement_keeps_its_model() {
    use crate::model_tier::{CatalogModel, ModelCatalog};
    let id = QueueItemId::new();
    let mut writer = PlanWriter::new(&id, crate::now());
    writer.filled(
        plan(&[("report", choice("p/old", Effort::High))]),
        (PlanSource::Fallback, PlanSource::Fallback),
    );
    let mut planned = writer.finish(Sizing::Off { policy_cap: None });
    let model = |id: &str| CatalogModel {
        model: id.parse().unwrap(),
        tier: Some(Tier::Strong),
        is_default: false,
    };
    let mut listed = ModelCatalog::new(vec![model("p/old"), model("p/next")]);
    listed.add_superseded("p/old".parse().unwrap(), "p/next".parse().unwrap());
    assert!(planned.needs_catalog_repair(&id, &listed));
    let before = planned.clone();
    planned
        .repair_catalog_with(&id, &listed, crate::now(), |_, _, _, _| Ok(None))
        .expect("a listed model with nothing to move to still runs");
    assert_eq!(planned.0["report"][0].model.to_string(), "p/old");
    assert_eq!(planned.0, before.0, "nothing moved");

    let mut retired = ModelCatalog::new(vec![model("p/next")]);
    retired.add_retired("p/old".parse().unwrap(), None);
    let refused = planned.repair_catalog_with(&id, &retired, crate::now(), |_, _, _, _| Ok(None));
    assert!(
        refused.is_err(),
        "a retired model with no replacement waits"
    );
    assert_eq!(planned.0, before.0, "a refused repair changes nothing");
}
