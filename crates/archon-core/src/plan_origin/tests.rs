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
    writer.finish()
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
    assert!(planned.faults(&item).is_empty());
    assert_eq!(planned.unattributed_fields(&item), 0);
}

#[test]
fn a_draft_naming_a_tier_leaves_the_model_to_the_daemon_and_names_who_asked() {
    let item = QueueItemId::new();
    let mut writer = PlanWriter::new(&item, crate::now());
    let asked = draft(serde_json::json!({"review": [{"tier": "strong", "effort": "high"}]}));
    let resolved = plan(&[("review", tiered("b/sol", Effort::High))]);
    writer.request(&asked, resolved, Authority::Orchestrator);
    let origin = writer.finish().origin(&item, "review", 0);
    assert_eq!(
        (origin.model.source, origin.model.by),
        (PlanSource::Tier, Authority::Daemon)
    );
    assert!(
        origin
            .model
            .note
            .as_deref()
            .unwrap()
            .contains("orchestrator"),
        "{origin:?}"
    );
    assert_eq!(
        (origin.effort.source, origin.effort.by),
        (PlanSource::Explicit, Authority::Orchestrator)
    );
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
    retry.carry(&planned, &item, |key| (key != "ship").then_some(usize::MAX));
    let retried = retry.finish();
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
    assert!(copied.faults(&fix).is_empty());
}

#[test]
fn origins_kept_by_a_clone_under_another_id_read_as_unattributed_pins() {
    let item = QueueItemId::new();
    let planned = enqueued(&item);
    let clone = QueueItemId::new();
    assert_eq!(planned.faults(&clone).len(), 1, "written for another item");
    let origin = planned.origin(&clone, "plan", 0);
    assert_eq!(origin.model.source, PlanSource::Unattributed);
    assert!(origin.model.is_pin() && origin.effort.is_pin());
    let reconciled = planned.reconciled(&clone).expect("a fault to reconcile");
    assert!(reconciled.faults(&clone).is_empty());
    assert_eq!(
        reconciled.unattributed_fields(&clone),
        8,
        "two fields of four entries"
    );
    assert!(
        planned.reconciled(&item).is_none(),
        "nothing to fix on its own item"
    );
}

#[test]
fn origins_of_another_shape_read_as_unattributed_for_that_key_only() {
    let item = QueueItemId::new();
    let mut stored = serde_json::to_value(enqueued(&item)).unwrap();
    stored["model_origins"]["entries"]["plan"] = serde_json::json!([]);
    let planned: PlannedPlan = serde_json::from_value(stored).unwrap();
    assert_eq!(
        planned.faults(&item).len(),
        1,
        "{:?}",
        planned.faults(&item)
    );
    assert_eq!(
        planned.origin(&item, "plan", 0).model.source,
        PlanSource::Unattributed
    );
    assert_eq!(
        planned.origin(&item, "build", 0).model.source,
        PlanSource::Explicit
    );
    assert_eq!(planned.unattributed_fields(&item), 2);
}

#[test]
fn a_plan_stored_without_origins_reads_as_legacy_pins_until_a_writer_changes_it() {
    let stored = serde_json::json!({
        "model_plan": {"build": [{"model": {"provider": "a", "model": "opus"}, "effort": "high"}],
                       "review": [{"model": {"provider": "b", "model": "sol"}, "effort": "low",
                                   "tier": {"requested": "strong", "resolved": "strong"}}]}
    });
    let mut planned: PlannedPlan = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&planned).unwrap(),
        stored,
        "written back unchanged"
    );
    let item = QueueItemId::new();
    let origin = planned.origin(&item, "build", 0);
    assert_eq!(
        (origin.model.source, origin.model.by),
        (PlanSource::LegacyUnknown, Authority::Unknown)
    );
    assert!(origin.model.is_pin() && origin.effort.is_pin());
    assert!(planned.faults(&item).is_empty() && planned.unattributed_fields(&item) == 0);
    let resolve =
        |_: &str, asked: &TierChoice| Ok(Some(("b/sol2".parse().unwrap(), asked.clone())));
    planned
        .retier(&item, (crate::now(), None), resolve)
        .unwrap();
    let review = planned.origin(&item, "review", 0);
    assert_eq!(
        (review.model.source, review.model.by),
        (PlanSource::Tier, Authority::Daemon)
    );
    assert_eq!(
        review.effort.source,
        PlanSource::LegacyUnknown,
        "the effort is not touched"
    );
    assert_eq!(
        planned.origin(&item, "build", 0).model.source,
        PlanSource::LegacyUnknown
    );
}

#[test]
fn an_unattributed_tiered_model_is_a_pin_no_dispatch_resolves_again() {
    let item = QueueItemId::new();
    let mut planned =
        PlannedPlan::unattributed(&item, plan(&[("review", tiered("b/sol", Effort::Low))]));
    assert_eq!(planned.unattributed_fields(&item), 2);
    let resolve = |_: &str, _: &TierChoice| -> Result<_, ArchonError> {
        unreachable!("an unattributed model is not resolved again")
    };
    planned
        .retier(&item, (crate::now(), None), resolve)
        .unwrap();
    assert_eq!(
        planned.choices_for("review").unwrap()[0].model.to_string(),
        "b/sol"
    );
}

#[test]
fn a_retier_that_moves_a_model_records_when_and_why() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    let first = planned.origin(&item, "review", 0);
    let same = |_: &str, asked: &TierChoice| Ok(Some(("b/sol".parse().unwrap(), asked.clone())));
    planned
        .retier(&item, (crate::now(), Some("unused")), same)
        .unwrap();
    assert_eq!(
        planned.origin(&item, "review", 0),
        first,
        "an unchanged model keeps its origin"
    );
    let moved = |_: &str, asked: &TierChoice| Ok(Some(("c/sol".parse().unwrap(), asked.clone())));
    planned
        .retier(&item, (crate::now(), Some("moved off b")), moved)
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
fn removing_a_key_drops_its_origins() {
    let item = QueueItemId::new();
    let mut planned = enqueued(&item);
    assert!(planned.remove_key("ship"));
    assert!(!planned.remove_key("ship"));
    assert!(
        planned.faults(&item).is_empty(),
        "origins still fit the plan"
    );
    assert!(
        planned
            .stored_origins()
            .unwrap()
            .entries
            .get("ship")
            .is_none()
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
