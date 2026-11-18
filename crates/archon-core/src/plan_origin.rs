//! Where every field of an item's model plan came from (design 7.9). The
//! plan and its origins form one [`PlannedPlan`], whose fields are private:
//! it is built by a [`PlanWriter`] and changed only by methods that name who
//! changes it, so a writer that forgets to say why does not compile.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::error::ArchonError;
use crate::id::QueueItemId;
use crate::model::{Effort, ModelChoice, ModelChoiceDraft, ModelPlan, ModelPlanDraft, ModelRef};
use crate::model_tier::TierChoice;
use crate::steer::SteeredBy;

/// How a plan field got its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanSource {
    /// Named by a request or a steer.
    Explicit,
    /// The learned route of the repository.
    Learned,
    /// Resolved inside a tier the request or the stage asked for.
    Tier,
    /// The stage's or the pipeline's default.
    Fallback,
    /// Sized by the daemon for an effort nobody pinned. No writer sets it yet.
    Sizer,
    /// Raised by the daemon after a failure. No writer sets it yet.
    Escalation,
    /// Moved by the daemon off a model that can no longer run it.
    Migrated,
    /// Stored before origins were recorded.
    LegacyUnknown,
    /// No writer accounted for it.
    Unattributed,
}

impl PlanSource {
    /// Whether a writer accounted for the field, so it names who and when.
    fn is_attributed(self) -> bool {
        !matches!(self, Self::LegacyUnknown | Self::Unattributed)
    }

    /// Whether the daemon fills a field this way and may change it again.
    fn is_fill(self) -> bool {
        matches!(
            self,
            Self::Learned | Self::Tier | Self::Fallback | Self::Sizer | Self::Escalation
        )
    }
}

impl fmt::Display for PlanSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Explicit => "explicit",
            Self::Learned => "learned",
            Self::Tier => "tier",
            Self::Fallback => "fallback",
            Self::Sizer => "sizer",
            Self::Escalation => "escalation",
            Self::Migrated => "migrated",
            Self::LegacyUnknown => "legacy_unknown",
            Self::Unattributed => "unattributed",
        })
    }
}

/// Who chose a plan field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Operator,
    Orchestrator,
    Daemon,
    /// Nobody known: a legacy or an unattributed field.
    Unknown,
}

impl From<SteeredBy> for Authority {
    fn from(by: SteeredBy) -> Self {
        match by {
            SteeredBy::Operator => Self::Operator,
            SteeredBy::Orchestrator => Self::Orchestrator,
        }
    }
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Operator => "operator",
            Self::Orchestrator => "orchestrator",
            Self::Daemon => "daemon",
            Self::Unknown => "unknown",
        })
    }
}

/// The origin of one field (the model, the effort or the tier) of one plan entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FieldOrigin {
    pub source: PlanSource,
    pub by: Authority,
    /// When the field was last chosen; `None` when nobody knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl FieldOrigin {
    fn chosen(source: PlanSource, by: Authority, at: Timestamp, note: Option<String>) -> Self {
        debug_assert!(
            (source == PlanSource::Explicit)
                == matches!(by, Authority::Operator | Authority::Orchestrator),
            "a request or a steer names a field; the daemon fills the rest"
        );
        debug_assert!(source.is_attributed());
        Self {
            source,
            by,
            at: Some(at),
            note,
        }
    }

    fn unknown(source: PlanSource) -> Self {
        assert!(!source.is_attributed());
        Self {
            source,
            by: Authority::Unknown,
            at: None,
            note: None,
        }
    }

    /// The one rule for what the daemon may change by itself: a field it
    /// chose itself. Everything else is a pin.
    pub fn is_movable(&self) -> bool {
        (self.source.is_fill() || self.source == PlanSource::Migrated)
            && self.by == Authority::Daemon
    }

    /// A pinned field is never sized, escalated or rewritten by the daemon.
    pub fn is_pin(&self) -> bool {
        !self.is_movable()
    }

    /// Whether a field a writer accounted for names who chose it and when.
    fn is_whole(&self) -> bool {
        !self.source.is_attributed() || (self.by != Authority::Unknown && self.at.is_some())
    }
}

impl fmt::Display for FieldOrigin {
    /// `explicit by operator at 2026-11-18 12:00`, with its note.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} by {}", self.source, self.by)?;
        if let Some(at) = self.at {
            write!(f, " at {}", at.format("%Y-%m-%d %H:%M"))?;
        }
        match &self.note {
            Some(note) => write!(f, " ({note})"),
            None => Ok(()),
        }
    }
}

/// When the daemon considers resolving a tiered choice's model again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierMove {
    /// Dispatch starts a run, or admission holds a queued item for it.
    Dispatch,
    /// A run is under way: a quota reroute, a pending spawn or a recovery.
    InRun,
}

/// The origins of one plan entry's model and effort, each on its own, and of
/// its tier when the entry has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryOrigin {
    pub model: FieldOrigin,
    pub effort: FieldOrigin,
    /// Who asked for the tier the daemon resolves the model in; present
    /// exactly when the choice has a tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<FieldOrigin>,
}

impl EntryOrigin {
    fn unknown(source: PlanSource, choice: &ModelChoice) -> Self {
        Self {
            model: FieldOrigin::unknown(source),
            effort: FieldOrigin::unknown(source),
            tier: choice.tier.as_ref().map(|_| FieldOrigin::unknown(source)),
        }
    }

    /// Whether the daemon may resolve this entry's model in its tier at
    /// `when`: a model it chose itself, always; at dispatch also the model
    /// of a pinned tier, because the tier is what was pinned.
    pub fn may_move(&self, when: TierMove) -> bool {
        let Some(tier) = &self.tier else {
            return false;
        };
        match when {
            TierMove::Dispatch => self.model.is_movable() || tier.is_pin(),
            TierMove::InRun => self.model.is_movable(),
        }
    }

    fn fields(&self) -> impl Iterator<Item = &FieldOrigin> {
        [&self.model, &self.effort].into_iter().chain(&self.tier)
    }

    /// Whether this is a whole origin of `choice`: a tier origin exactly
    /// when the choice has a tier, and no field missing who or when.
    fn describes(&self, choice: &ModelChoice) -> bool {
        self.tier.is_some() == choice.tier.is_some() && self.fields().all(FieldOrigin::is_whole)
    }

    fn noted(mut self, note: &str) -> Self {
        let fields = [&mut self.model, &mut self.effort].into_iter();
        for field in fields.chain(self.tier.as_mut()) {
            field.note = Some(match field.note.take() {
                Some(was) => format!("{note}; {was}"),
                None => note.to_owned(),
            });
        }
        self
    }
}

/// The origins of a plan, written for one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanOrigins {
    /// The item these origins were written for; `None` describes no item.
    pub item: Option<QueueItemId>,
    pub entries: BTreeMap<String, Vec<EntryOrigin>>,
}

/// Whether `slots` are the whole origins of `choices`, one each.
fn describe(slots: &[EntryOrigin], choices: &[ModelChoice]) -> bool {
    slots.len() == choices.len() && slots.iter().zip(choices).all(|(o, c)| o.describes(c))
}

impl PlanOrigins {
    /// Origins that describe no field of any item, so every field of the
    /// item they are stored with reads as unattributed.
    fn of_no_item() -> Self {
        Self {
            item: None,
            entries: BTreeMap::new(),
        }
    }

    /// Why these origins do not describe `plan` of `item`; `None` when they
    /// do: written for it, with its keys and counts, every origin whole.
    fn fault(&self, item: &QueueItemId, plan: &ModelPlan) -> Option<String> {
        match &self.item {
            Some(written) if written == item => {}
            Some(written) => return Some(format!("origins written for {written}")),
            None => return Some("origins written for no item".to_owned()),
        }
        if !self.entries.keys().eq(plan.0.keys()) {
            return Some("origins of other plan keys".to_owned());
        }
        let misfit = plan.0.iter().zip(self.entries.values());
        let (key, _) = misfit
            .filter(|((_, choices), slots)| !describe(slots, choices))
            .map(|(entry, _)| entry)
            .next()?;
        Some(format!("plan key {key:?} has origins of another shape"))
    }
}

/// Stored origins as read: present ones that do not decode, a `null` among
/// them, vouch for no field, so every field of the item reads as unattributed.
fn lenient_origins<'de, D>(stored: D) -> Result<Option<Box<PlanOrigins>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(stored)?;
    let origins = serde_json::from_value(value).unwrap_or_else(|_| PlanOrigins::of_no_item());
    Ok(Some(Box::new(origins)))
}

/// A replacement model and its tier resolution, when the choice is tiered.
pub type CatalogReplacement = (ModelRef, Option<TierChoice>);

/// The result worth surfacing alongside a stored model plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CatalogCheck {
    Deferred { reason: String },
    Warnings { warnings: Vec<String> },
}

impl std::fmt::Display for CatalogCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deferred { reason } => write!(f, "model catalog not checked: {reason}"),
            Self::Warnings { warnings } => f.write_str(&warnings.join(", ")),
        }
    }
}

/// An item's model plan with where each field came from. Serialized as the
/// item's `model_plan` and `model_origins` keys, so stored items and older
/// readers see the plan as before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlannedPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_check: Option<Box<CatalogCheck>>,
    model_plan: ModelPlan,
    // Boxed so a stored item stays about the size it was before origins.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_origins"
    )]
    model_origins: Option<Box<PlanOrigins>>,
}

impl Deref for PlannedPlan {
    type Target = ModelPlan;

    fn deref(&self) -> &ModelPlan {
        &self.model_plan
    }
}

impl PlannedPlan {
    pub fn canonicalize_aliases(&mut self, catalog: &crate::model_tier::ModelCatalog) {
        for choice in self.model_plan.0.values_mut().flatten() {
            choice.model = catalog.canonical_alias(&choice.model).clone();
        }
    }

    pub fn check_request_policy(
        &self,
        policy: &crate::policy::ModelPolicy,
    ) -> Result<(), ArchonError> {
        if matches!(
            self.catalog_check.as_deref(),
            Some(CatalogCheck::Deferred { .. })
        ) {
            for choice in self.model_plan.0.values().flatten() {
                policy.check_pending_model(choice)?;
            }
            Ok(())
        } else {
            policy.check_plan(&self.model_plan)
        }
    }

    /// Apply a request check to carried entries too, retaining their provenance
    /// when the catalog supplies the canonical spelling.
    pub fn checked_request_catalog(
        mut self,
        catalog: &crate::model_tier::ModelCatalog,
        carried: Option<&ModelPlan>,
    ) -> Result<Self, ArchonError> {
        if catalog.not_ready_reason().is_none() {
            for (key, choices) in &mut self.model_plan.0 {
                let previous = carried.and_then(|plan| plan.choices_for(key));
                for (slot, choice) in choices.iter_mut().enumerate() {
                    let untouched = previous
                        .and_then(|slots| slots.get(slot))
                        .is_some_and(|old| old.model == choice.model);
                    if !untouched {
                        choice.model = catalog.check(&choice.model)?;
                    }
                }
            }
        }
        self.record_catalog(catalog);
        Ok(self)
    }

    pub fn record_catalog(&mut self, catalog: &crate::model_tier::ModelCatalog) {
        self.catalog_check = if let Some(reason) = catalog.not_ready_reason() {
            Some(Box::new(CatalogCheck::Deferred {
                reason: reason.to_owned(),
            }))
        } else {
            let warnings: std::collections::BTreeSet<_> = self
                .model_plan
                .0
                .values()
                .flatten()
                .filter_map(|choice| {
                    catalog
                        .superseded_by(&choice.model)
                        .map(|successor| format!("{} is superseded by {successor}", choice.model))
                })
                .collect();
            (!warnings.is_empty()).then(|| {
                Box::new(CatalogCheck::Warnings {
                    warnings: warnings.into_iter().collect(),
                })
            })
        };
    }

    /// `plan` with no writer to vouch for it: every field reads as a pin and
    /// is counted as unattributed.
    pub fn unattributed(item: &QueueItemId, plan: ModelPlan) -> Self {
        let entries = shape_of(&plan, PlanSource::Unattributed);
        Self {
            catalog_check: None,
            model_plan: plan,
            model_origins: Some(Box::new(PlanOrigins {
                item: Some(item.clone()),
                entries,
            })),
        }
    }

    /// `plan` as an item stored before 1.6.4 reads: no origins, every field a
    /// `legacy_unknown` pin. Only tests make one; the daemon reads them.
    #[cfg(any(test, feature = "testing"))]
    pub fn legacy(plan: ModelPlan) -> Self {
        Self {
            catalog_check: None,
            model_plan: plan,
            model_origins: None,
        }
    }

    /// The plan to edit by hand, its origins dropped as a 1.6.3 rewrite drops
    /// them. Only tests edit one.
    #[cfg(any(test, feature = "testing"))]
    pub fn legacy_mut(&mut self) -> &mut ModelPlan {
        self.model_origins = None;
        &mut self.model_plan
    }

    pub fn plan(&self) -> &ModelPlan {
        &self.model_plan
    }

    pub fn into_plan(self) -> ModelPlan {
        self.model_plan
    }

    /// The plan a run is driven by and quota holds read: a choice the daemon
    /// may not move at `when` drops its tier there, so it is an exact pin to
    /// every reroute, hold and recovery that reads it.
    pub fn dispatched(&self, item: &QueueItemId, when: TierMove) -> ModelPlan {
        let origins = self.origins(item);
        let mut plan = self.model_plan.clone();
        for (choices, slots) in plan.0.values_mut().zip(origins.values()) {
            assert_eq!(choices.len(), slots.len(), "an origin per choice");
            for (choice, origin) in choices.iter_mut().zip(slots) {
                if !origin.may_move(when) {
                    choice.tier = None;
                }
            }
        }
        debug_assert!(
            plan.0.keys().eq(self.model_plan.0.keys()),
            "only tiers change"
        );
        plan
    }

    /// Why the stored origins do not describe this plan of `item`; `None`
    /// when they do or when there are none.
    pub fn fault(&self, item: &QueueItemId) -> Option<String> {
        let origins = self.model_origins.as_ref()?;
        origins.fault(item, &self.model_plan)
    }

    /// Every entry's origin, keyed like the plan, by the one rule of reading:
    /// no stored origins read as legacy, stored origins that describe this
    /// plan of `item` as they are, and any others as unattributed, whole.
    pub fn origins(&self, item: &QueueItemId) -> BTreeMap<String, Vec<EntryOrigin>> {
        let origins = match &self.model_origins {
            None => shape_of(&self.model_plan, PlanSource::LegacyUnknown),
            Some(stored) => match stored.fault(item, &self.model_plan) {
                None => stored.entries.clone(),
                Some(_) => shape_of(&self.model_plan, PlanSource::Unattributed),
            },
        };
        debug_assert!(origins.keys().eq(self.model_plan.0.keys()));
        origins
    }

    /// Where slot `slot` of `key` came from, for the item `item`.
    pub fn origin(&self, item: &QueueItemId, key: &str, slot: usize) -> EntryOrigin {
        let mut origins = self.origins(item);
        let slots = origins.remove(key);
        let slots = slots.expect("an origin is read for a key of the plan");
        assert!(slot < slots.len(), "{key:?} has no choice {slot}");
        slots.into_iter().nth(slot).expect("checked above")
    }

    /// How many fields (a model, an effort or a tier each) read as unattributed.
    pub fn unattributed_fields(&self, item: &QueueItemId) -> usize {
        self.fields_from(item, PlanSource::Unattributed)
    }

    /// How many fields read as stored before origins were recorded.
    pub fn legacy_fields(&self, item: &QueueItemId) -> usize {
        self.fields_from(item, PlanSource::LegacyUnknown)
    }

    fn fields_from(&self, item: &QueueItemId, source: PlanSource) -> usize {
        let entries = self.origins(item).into_values().flatten();
        let from = |e: &EntryOrigin| {
            debug_assert!(e.fields().count() >= 2, "a model and an effort");
            e.fields().filter(|f| f.source == source).count()
        };
        entries.map(|e| from(&e)).sum()
    }

    /// This plan with origins that do not describe it replaced by
    /// unattributed ones; `None` when nothing needs replacing.
    pub fn reconciled(&self, item: &QueueItemId) -> Option<Self> {
        self.fault(item)?;
        let reconciled = Self::unattributed(item, self.model_plan.clone());
        debug_assert!(reconciled.fault(item).is_none());
        Some(reconciled)
    }

    /// The same plan for the new item `to`, cloned from `from`: each field
    /// keeps its source, authority and time, with `note` added when given.
    /// A plan without origins stays without them.
    pub fn copy_for(&self, from: &QueueItemId, to: &QueueItemId, note: Option<&str>) -> Self {
        assert_ne!(from, to, "a copy is for another item");
        if self.model_origins.is_none() {
            return self.clone();
        }
        let mut writer = PlanWriter::editing(self, from, crate::now());
        writer.item = to.clone();
        if let Some(note) = note {
            for entry in writer.entries.values_mut().flatten() {
                *entry = entry.clone().noted(note);
            }
        }
        writer.finish()
    }

    /// Drop `key` and its origins together; `true` when the plan had it.
    /// Origins that did not describe the dropped choices vouch for none of
    /// the rest either, so dropping a key never turns a fault into a fit.
    pub fn remove_key(&mut self, key: &str) -> bool {
        let Some(choices) = self.model_plan.0.remove(key) else {
            return false;
        };
        let Some(origins) = &mut self.model_origins else {
            return true;
        };
        let slots = origins.entries.remove(key);
        if !slots.is_some_and(|slots| describe(&slots, &choices)) {
            **origins = PlanOrigins::of_no_item();
        }
        debug_assert!(!origins.entries.contains_key(key), "dropped with its key");
        true
    }

    /// Steer every choice of `key` to the `model` and `effort` named: each
    /// named field becomes explicit by `by`, even when it keeps its value.
    /// `None` when the plan has no such key.
    pub fn steer(
        &mut self,
        item: &QueueItemId,
        key: &str,
        named: (Option<&ModelRef>, Option<Effort>),
        (by, at): (Authority, Timestamp),
    ) -> Option<&[ModelChoice]> {
        self.model_plan.0.get(key)?;
        let mut writer = PlanWriter::editing(self, item, at);
        writer.steer(key, named, by);
        *self = writer.finish();
        self.model_plan.0.get(key).map(Vec::as_slice)
    }

    pub fn needs_catalog_repair(
        &self,
        item: &QueueItemId,
        catalog: &crate::model_tier::ModelCatalog,
    ) -> bool {
        let origins = self.origins(item);
        self.model_plan.0.iter().any(|(key, choices)| {
            choices.iter().zip(&origins[key]).any(|(choice, origin)| {
                catalog.check(&choice.model).is_err()
                    || (origin.model.is_movable() && catalog.superseded_by(&choice.model).is_some())
            })
        })
    }

    /// Check every choice, allowing the caller to replace unavailable daemon choices.
    pub fn repair_catalog_with(
        &mut self,
        item: &QueueItemId,
        catalog: &crate::model_tier::ModelCatalog,
        at: Timestamp,
        mut replacement: impl FnMut(
            &str,
            usize,
            &ModelChoice,
            &EntryOrigin,
        ) -> Result<Option<CatalogReplacement>, ArchonError>,
    ) -> Result<(), ArchonError> {
        if let Some(reason) = catalog.not_ready_reason() {
            return Err(ArchonError::Unavailable(reason.to_owned()));
        }
        if !self.needs_catalog_repair(item, catalog) {
            self.canonicalize_aliases(catalog);
            self.record_catalog(catalog);
            return Ok(());
        }
        let mut writer = PlanWriter::editing(self, item, at);
        for (key, choices) in &mut writer.plan.0 {
            let slots = writer.entries.get_mut(key).expect("an origin per entry");
            assert_eq!(choices.len(), slots.len(), "origins fit the plan");
            for (slot, (choice, origin)) in choices.iter_mut().zip(slots).enumerate() {
                let listed = catalog.check(&choice.model).and_then(|model| {
                    if origin.model.is_movable()
                        && let Some(next) = catalog.superseded_by(&model)
                    {
                        return Err(ArchonError::Invalid(format!(
                            "{model} is superseded by {next}"
                        )));
                    }
                    Ok(model)
                });
                let model = match listed {
                    Ok(model) => model,
                    Err(error) if origin.model.is_movable() => {
                        let Some((model, tier)) = replacement(key, slot, choice, origin)? else {
                            return Err(error);
                        };
                        let model = catalog.check(&model)?;
                        choice.tier = tier;
                        origin.model = FieldOrigin::chosen(
                            PlanSource::Migrated,
                            Authority::Daemon,
                            at,
                            Some(format!("was {}", choice.model)),
                        );
                        model
                    }
                    Err(error) => return Err(error),
                };
                choice.model = model;
            }
        }
        let mut repaired = if writer.plan == self.model_plan {
            self.clone()
        } else {
            writer.finish()
        };
        repaired.record_catalog(catalog);
        debug_assert_eq!(repaired.origins(item).len(), self.model_plan.0.len());
        *self = repaired;
        Ok(())
    }

    /// Migrate unavailable daemon choices only when Aether names a successor.
    pub fn repair_catalog(
        &mut self,
        item: &QueueItemId,
        catalog: &crate::model_tier::ModelCatalog,
        at: Timestamp,
    ) -> Result<(), ArchonError> {
        self.repair_catalog_with(item, catalog, at, |_, _, choice, _| {
            Ok(catalog
                .successor(&choice.model)
                .cloned()
                .map(|model| (model, choice.tier.clone())))
        })
    }

    /// Resolve each tiered choice the daemon may move at `when`: `resolve`
    /// gives its new model and tier, or `None` to keep it. A moved model is
    /// the daemon's, noted with `note` alone; the tier keeps who asked for
    /// it. All or nothing: a refusal leaves the plan and its origins alone.
    pub fn retier(
        &mut self,
        item: &QueueItemId,
        (at, note, when): (Timestamp, Option<&str>, TierMove),
        resolve: impl FnMut(&str, &TierChoice) -> Result<Option<(ModelRef, TierChoice)>, ArchonError>,
    ) -> Result<(), ArchonError> {
        let mut writer = PlanWriter::editing(self, item, at);
        let before = writer.entries.clone();
        writer.retier((note, when), resolve)?;
        if writer.plan == self.model_plan && writer.entries == before {
            return Ok(());
        }
        *self = writer.finish();
        Ok(())
    }
}

/// `source` for every field of `plan`.
fn shape_of(plan: &ModelPlan, source: PlanSource) -> BTreeMap<String, Vec<EntryOrigin>> {
    let origin = |choice| EntryOrigin::unknown(source, choice);
    let entries = plan
        .0
        .iter()
        .map(|(key, choices)| (key.clone(), choices.iter().map(origin).collect()));
    entries.collect()
}

/// Builds a [`PlannedPlan`] step by step: what a request names, what an
/// earlier plan of the same item keeps, then each fill step's additions.
#[derive(Debug)]
pub struct PlanWriter {
    catalog_check: Option<Box<CatalogCheck>>,
    item: QueueItemId,
    at: Timestamp,
    plan: ModelPlan,
    entries: BTreeMap<String, Vec<EntryOrigin>>,
}

impl PlanWriter {
    pub fn new(item: &QueueItemId, at: Timestamp) -> Self {
        Self {
            catalog_check: None,
            item: item.clone(),
            at,
            plan: ModelPlan::default(),
            entries: BTreeMap::new(),
        }
    }

    /// The plan so far, to hand to the next fill step.
    pub fn plan(&self) -> &ModelPlan {
        &self.plan
    }

    /// The entries `resolved` from `draft` by a request of `by`: each field
    /// the draft names is explicit; a tier's model and a missing effort are
    /// the daemon's.
    pub fn request(&mut self, draft: &ModelPlanDraft, resolved: ModelPlan, by: Authority) {
        assert!(
            matches!(by, Authority::Operator | Authority::Orchestrator),
            "only a request names plan fields"
        );
        assert_eq!(
            draft.0.len(),
            resolved.0.len(),
            "every drafted key resolved"
        );
        for (key, choices) in resolved.0 {
            let drafts = draft.0.get(&key).expect("resolved from the draft");
            assert_eq!(drafts.len(), choices.len(), "one choice per draft");
            let slots = drafts
                .iter()
                .zip(&choices)
                .map(|(d, c)| self.requested(d, c, by))
                .collect();
            self.insert(key, choices, slots);
        }
    }

    fn requested(
        &self,
        draft: &ModelChoiceDraft,
        choice: &ModelChoice,
        by: Authority,
    ) -> EntryOrigin {
        let at = self.at;
        let model = match &draft.model {
            Some(_) => FieldOrigin::chosen(PlanSource::Explicit, by, at, None),
            None => FieldOrigin::chosen(PlanSource::Tier, Authority::Daemon, at, None),
        };
        let effort = match draft.effort {
            Some(_) => FieldOrigin::chosen(PlanSource::Explicit, by, at, None),
            None => default_effort(at),
        };
        let tier = choice.tier.as_ref().map(|_| match draft.tier {
            Some(_) => FieldOrigin::chosen(PlanSource::Explicit, by, at, None),
            None => default_tier(at),
        });
        debug_assert!(
            draft.tier.is_none() || tier.is_some(),
            "a drafted tier resolves to a tiered choice"
        );
        EntryOrigin {
            model,
            effort,
            tier,
        }
    }

    /// The entries of `kept` (a plan of `item`, which `limit` keeps a key of
    /// with at most that many choices) carried with the origins they have.
    pub fn carry(
        &mut self,
        kept: &PlannedPlan,
        item: &QueueItemId,
        limit: impl Fn(&str) -> Option<usize>,
    ) {
        assert_eq!(*item, self.item, "an earlier plan of the same item");
        for (key, choices) in &kept.model_plan.0 {
            let Some(max) = limit(key) else { continue };
            let mut choices = choices.clone();
            choices.truncate(max.max(1));
            let slots = (0..choices.len())
                .map(|s| kept.origin(item, key, s))
                .collect();
            self.insert(key.clone(), choices, slots);
        }
    }

    /// The plan a fill step returned, which keeps every entry so far: each
    /// entry it added is the daemon's, its model `model` (or `tier` when
    /// tiered) and its effort `effort`.
    pub fn filled(&mut self, filled: ModelPlan, (model, effort): (PlanSource, PlanSource)) {
        assert!(
            model.is_fill() && effort.is_fill(),
            "a fill step is the daemon's"
        );
        for (key, choices) in &self.plan.0 {
            assert_eq!(
                filled.0.get(key),
                Some(choices),
                "a fill step keeps {key:?}"
            );
        }
        let at = self.at;
        for (key, choices) in filled.0 {
            if self.plan.0.contains_key(&key) {
                continue;
            }
            let slots = choices
                .iter()
                .map(|choice| EntryOrigin {
                    model: match choice.tier {
                        Some(_) => {
                            FieldOrigin::chosen(PlanSource::Tier, Authority::Daemon, at, None)
                        }
                        None => FieldOrigin::chosen(model, Authority::Daemon, at, None),
                    },
                    effort: FieldOrigin::chosen(effort, Authority::Daemon, at, None),
                    tier: choice.tier.as_ref().map(|_| match model {
                        PlanSource::Tier => default_tier(at),
                        step => FieldOrigin::chosen(step, Authority::Daemon, at, None),
                    }),
                })
                .collect();
            self.insert(key, choices, slots);
        }
    }

    fn insert(&mut self, key: String, choices: Vec<ModelChoice>, slots: Vec<EntryOrigin>) {
        assert_eq!(
            choices.len(),
            slots.len(),
            "an origin per choice of {key:?}"
        );
        assert!(!self.plan.0.contains_key(&key), "{key:?} is written once");
        self.entries.insert(key.clone(), slots);
        self.plan.0.insert(key, choices);
    }

    /// The plan as stored, every entry carried with the origin it reads
    /// with for `item`, to change in place.
    fn editing(planned: &PlannedPlan, item: &QueueItemId, at: Timestamp) -> Self {
        let editing = Self {
            catalog_check: planned.catalog_check.clone(),
            item: item.clone(),
            at,
            plan: planned.model_plan.clone(),
            entries: planned.origins(item),
        };
        debug_assert!(editing.entries.keys().eq(editing.plan.0.keys()));
        editing
    }

    /// Steer every choice of `key`, which the plan has: see [`PlannedPlan::steer`].
    fn steer(
        &mut self,
        key: &str,
        (model, effort): (Option<&ModelRef>, Option<Effort>),
        by: Authority,
    ) {
        assert!(model.is_some() || effort.is_some(), "a steer names a field");
        let at = self.at;
        let choices = self
            .plan
            .0
            .get_mut(key)
            .expect("the caller checked the key");
        let slots = self.entries.get_mut(key).expect("an origin per entry");
        assert_eq!(slots.len(), choices.len(), "origins fit the plan");
        for (choice, origin) in choices.iter_mut().zip(slots.iter_mut()) {
            if model.is_some() {
                let was = format!("was {}: {}", origin.model.source, choice.model);
                origin.model = FieldOrigin::chosen(PlanSource::Explicit, by, at, Some(was));
            }
            if effort.is_some() {
                let was = format!("was {}: {}", origin.effort.source, choice.effort);
                origin.effort = FieldOrigin::chosen(PlanSource::Explicit, by, at, Some(was));
            }
            *choice = choice.steered(model, effort);
            if choice.tier.is_none() {
                origin.tier = None;
            }
        }
    }

    /// Resolve each tiered choice the daemon may move: see [`PlannedPlan::retier`].
    fn retier(
        &mut self,
        (note, when): (Option<&str>, TierMove),
        mut resolve: impl FnMut(
            &str,
            &TierChoice,
        ) -> Result<Option<(ModelRef, TierChoice)>, ArchonError>,
    ) -> Result<(), ArchonError> {
        let at = self.at;
        for (key, choices) in self.plan.0.iter_mut() {
            let slots = self.entries.get_mut(key).expect("an origin per entry");
            assert_eq!(slots.len(), choices.len(), "origins fit the plan");
            for (choice, origin) in choices.iter_mut().zip(slots.iter_mut()) {
                let Some(asked) = &choice.tier else { continue };
                if !origin.may_move(when) {
                    continue;
                }
                let Some((model, tier)) = resolve(key, asked)? else {
                    continue;
                };
                debug_assert_eq!(tier.requested, asked.requested, "the tier asked for");
                let daemons = (PlanSource::Tier, Authority::Daemon);
                let migrated = origin.model.source == PlanSource::Migrated
                    && origin.model.by == Authority::Daemon;
                if model != choice.model
                    || (!migrated && (origin.model.source, origin.model.by) != daemons)
                {
                    let note = note.map(str::to_owned);
                    origin.model = FieldOrigin::chosen(daemons.0, daemons.1, at, note);
                }
                choice.model = model;
                choice.tier = Some(tier);
            }
        }
        Ok(())
    }

    /// The plan and its origins as one value: the only place either is set.
    pub fn finish(self) -> PlannedPlan {
        let origins = PlanOrigins {
            item: Some(self.item.clone()),
            entries: self.entries,
        };
        let fault = origins.fault(&self.item, &self.plan);
        assert_eq!(fault, None, "one whole origin per field");
        PlannedPlan {
            catalog_check: self.catalog_check,
            model_plan: self.plan,
            model_origins: Some(Box::new(origins)),
        }
    }
}

/// A tier nobody asked for is the stage's or the pipeline's.
fn default_tier(at: Timestamp) -> FieldOrigin {
    let note = Some("the stage's tier".to_owned());
    FieldOrigin::chosen(PlanSource::Fallback, Authority::Daemon, at, note)
}

/// A missing effort takes the stage's default.
fn default_effort(at: Timestamp) -> FieldOrigin {
    let note = Some("the stage's default effort".to_owned());
    FieldOrigin::chosen(PlanSource::Fallback, Authority::Daemon, at, note)
}

#[cfg(test)]
#[path = "plan_origin/tests.rs"]
mod tests;
