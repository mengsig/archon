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
use crate::protocol::ClientKind;
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
    /// No writer accounted for it, or a source this archond does not know.
    #[serde(other)]
    Unattributed,
}

impl PlanSource {
    /// Every source a writer can account for a field with.
    pub const ATTRIBUTED: [Self; 8] = [
        Self::Explicit,
        Self::Learned,
        Self::Tier,
        Self::Fallback,
        Self::Sizer,
        Self::Escalation,
        Self::Migrated,
        Self::LegacyUnknown,
    ];

    fn unattributed() -> Self {
        Self::Unattributed
    }

    /// A pinned field is never sized, escalated or rewritten by the daemon.
    pub fn is_pin(self) -> bool {
        matches!(
            self,
            Self::Explicit | Self::LegacyUnknown | Self::Unattributed
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
    #[serde(other)]
    Unknown,
}

impl Authority {
    fn unknown() -> Self {
        Self::Unknown
    }

    /// The orchestrator calls through the MCP bridge, the operator otherwise.
    pub fn of_client(client: ClientKind) -> Self {
        match client {
            ClientKind::ArchonMcp => Self::Orchestrator,
            _ => Self::Operator,
        }
    }
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

/// The origin of one field (the model or the effort) of one plan entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FieldOrigin {
    /// A stored origin that names no source was accounted for by nobody.
    #[serde(default = "PlanSource::unattributed")]
    pub source: PlanSource,
    #[serde(default = "Authority::unknown")]
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
        debug_assert!(!matches!(
            source,
            PlanSource::LegacyUnknown | PlanSource::Unattributed
        ));
        Self {
            source,
            by,
            at: Some(at),
            note,
        }
    }

    fn unknown(source: PlanSource) -> Self {
        assert!(matches!(
            source,
            PlanSource::LegacyUnknown | PlanSource::Unattributed
        ));
        Self {
            source,
            by: Authority::Unknown,
            at: None,
            note: None,
        }
    }

    fn unattributed() -> Self {
        Self::unknown(PlanSource::Unattributed)
    }

    pub fn is_pin(&self) -> bool {
        self.source.is_pin()
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

/// The origins of one plan entry's model and effort, each on its own, and of
/// its tier when the entry has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryOrigin {
    /// A stored entry missing a field's origin keeps the other one.
    #[serde(default = "FieldOrigin::unattributed")]
    pub model: FieldOrigin,
    #[serde(default = "FieldOrigin::unattributed")]
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

    /// This stored origin with a tier origin exactly when `choice` has a tier:
    /// a tier no writer accounted for reads as unattributed.
    fn fitted(mut self, choice: &ModelChoice) -> Self {
        self.tier = match (&choice.tier, self.tier) {
            (None, _) => None,
            (Some(_), Some(tier)) => Some(tier),
            (Some(_), None) => Some(FieldOrigin::unattributed()),
        };
        debug_assert_eq!(self.tier.is_some(), choice.tier.is_some());
        self
    }

    fn fields(&self) -> impl Iterator<Item = &FieldOrigin> {
        [&self.model, &self.effort].into_iter().chain(&self.tier)
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
    /// The item these origins were written for: a clone under another id
    /// that kept them by accident reads as unattributed.
    pub item: QueueItemId,
    pub entries: BTreeMap<String, Vec<EntryOrigin>>,
}

/// An item's model plan with where each field came from. Serialized as the
/// item's `model_plan` and `model_origins` keys, so stored items and older
/// readers see the plan as before.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlannedPlan {
    model_plan: ModelPlan,
    // Boxed so a stored item stays about the size it was before origins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_origins: Option<Box<PlanOrigins>>,
}

impl Deref for PlannedPlan {
    type Target = ModelPlan;

    fn deref(&self) -> &ModelPlan {
        &self.model_plan
    }
}

impl PlannedPlan {
    /// `plan` with no writer to vouch for it: every field reads as a pin and
    /// is counted as unattributed.
    pub fn unattributed(item: &QueueItemId, plan: ModelPlan) -> Self {
        let entries = shape_of(&plan, |c| EntryOrigin::unknown(PlanSource::Unattributed, c));
        Self {
            model_plan: plan,
            model_origins: Some(Box::new(PlanOrigins {
                item: item.clone(),
                entries,
            })),
        }
    }

    /// `plan` as an item stored before 1.6.4 reads: no origins, every field a
    /// `legacy_unknown` pin. Only tests make one; the daemon reads them.
    #[cfg(any(test, feature = "testing"))]
    pub fn legacy(plan: ModelPlan) -> Self {
        Self {
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

    /// The plan a run is driven by: a model no writer accounted for drops its
    /// tier there, so no quota move of an unstarted stage changes it.
    pub fn dispatched(&self, item: &QueueItemId) -> ModelPlan {
        let mut plan = self.model_plan.clone();
        for (key, choices) in plan.0.iter_mut() {
            for (slot, choice) in choices.iter_mut().enumerate() {
                if self.origin(item, key, slot).model.source == PlanSource::Unattributed {
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

    pub fn into_plan(self) -> ModelPlan {
        self.model_plan
    }

    /// The origins as stored; `None` for an item stored before 1.6.4.
    pub fn stored_origins(&self) -> Option<&PlanOrigins> {
        self.model_origins.as_deref()
    }

    /// Where slot `slot` of `key` came from, for the item `item`: legacy for
    /// a record without origins, unattributed where its origins do not fit.
    pub fn origin(&self, item: &QueueItemId, key: &str, slot: usize) -> EntryOrigin {
        let choice = self.model_plan.0.get(key).and_then(|c| c.get(slot));
        let choice = choice.expect("an origin is read for a choice of the plan");
        let Some(origins) = &self.model_origins else {
            return EntryOrigin::unknown(PlanSource::LegacyUnknown, choice);
        };
        let fits = origins.item == *item && self.fits(key);
        let found = origins.entries.get(key).and_then(|e| e.get(slot));
        match found.filter(|_| fits) {
            Some(origin) => origin.clone().fitted(choice),
            None => EntryOrigin::unknown(PlanSource::Unattributed, choice),
        }
    }

    /// Every entry's origin, keyed like the plan.
    pub fn origins(&self, item: &QueueItemId) -> BTreeMap<String, Vec<EntryOrigin>> {
        let origins = self.model_plan.0.iter().map(|(key, choices)| {
            let slots = (0..choices.len()).map(|slot| self.origin(item, key, slot));
            (key.clone(), slots.collect())
        });
        origins.collect()
    }

    /// How many fields (a model, an effort or a tier each) read as unattributed.
    pub fn unattributed_fields(&self, item: &QueueItemId) -> usize {
        let entries = self.origins(item).into_values().flatten();
        let unattributed = |e: &EntryOrigin| {
            let fields = e.fields().filter(|f| f.source == PlanSource::Unattributed);
            fields.count()
        };
        entries.map(|e| unattributed(&e)).sum()
    }

    /// Why the stored origins do not describe this plan of `item`; empty
    /// when they do or when there are none.
    pub fn faults(&self, item: &QueueItemId) -> Vec<String> {
        let Some(origins) = &self.model_origins else {
            return Vec::new();
        };
        let mut faults = Vec::new();
        if origins.item != *item {
            faults.push(format!("origins written for {}", origins.item));
        }
        let keys = self.model_plan.0.keys().chain(origins.entries.keys());
        for key in keys.collect::<std::collections::BTreeSet<_>>() {
            if !self.fits(key) {
                faults.push(format!("plan key {key:?} has origins of another shape"));
            }
        }
        faults
    }

    /// This plan with faulty origins replaced by unattributed ones; `None`
    /// when nothing needs replacing.
    pub fn reconciled(&self, item: &QueueItemId) -> Option<Self> {
        if self.faults(item).is_empty() {
            return None;
        }
        let entries = self.origins(item);
        debug_assert!(entries.keys().eq(self.model_plan.0.keys()));
        Some(Self {
            model_plan: self.model_plan.clone(),
            model_origins: Some(Box::new(PlanOrigins {
                item: item.clone(),
                entries,
            })),
        })
    }

    fn fits(&self, key: &str) -> bool {
        let origins = self.model_origins.as_ref().map(|o| o.entries.get(key));
        let slots = origins.flatten().map(Vec::len);
        slots == self.model_plan.0.get(key).map(Vec::len)
    }

    /// Make the origins whole for `item` before a change: a legacy record
    /// gets `legacy_unknown` for every field it had.
    fn make_whole(&mut self, item: &QueueItemId) {
        let entries = self.origins(item);
        debug_assert!(entries.keys().eq(self.model_plan.0.keys()));
        self.model_origins = Some(Box::new(PlanOrigins {
            item: item.clone(),
            entries,
        }));
        debug_assert!(self.faults(item).is_empty());
    }

    /// The same plan for the new item `to`, cloned from `from`: each field
    /// keeps its source, authority and time, with `note` added when given.
    pub fn copy_for(&self, from: &QueueItemId, to: &QueueItemId, note: Option<&str>) -> Self {
        assert_ne!(from, to, "a copy is for another item");
        let Some(_) = &self.model_origins else {
            return self.clone();
        };
        let mut entries = self.origins(from);
        if let Some(note) = note {
            for entry in entries.values_mut().flatten() {
                *entry = entry.clone().noted(note);
            }
        }
        let copy = Self {
            model_plan: self.model_plan.clone(),
            model_origins: Some(Box::new(PlanOrigins {
                item: to.clone(),
                entries,
            })),
        };
        debug_assert!(copy.faults(to).is_empty());
        copy
    }

    /// Drop `key` and its origins; `true` when it was there.
    pub fn remove_key(&mut self, key: &str) -> bool {
        let removed = self.model_plan.0.remove(key).is_some();
        if let Some(origins) = &mut self.model_origins {
            origins.entries.remove(key);
        }
        debug_assert!(!self.model_plan.0.contains_key(key));
        removed
    }

    /// Steer every choice of `key` to the `model` and `effort` named: each
    /// named field becomes explicit by `by`, even when it keeps its value.
    /// `None` when the plan has no such key.
    pub fn steer(
        &mut self,
        item: &QueueItemId,
        key: &str,
        (model, effort): (Option<&ModelRef>, Option<Effort>),
        (by, at): (Authority, Timestamp),
    ) -> Option<&[ModelChoice]> {
        assert!(model.is_some() || effort.is_some(), "a steer names a field");
        self.model_plan.0.get(key)?;
        self.make_whole(item);
        let Self {
            model_plan,
            model_origins,
        } = self;
        let origins = model_origins.as_mut().expect("made whole above");
        let slots = origins.entries.get_mut(key).expect("made whole above");
        let choices = model_plan.0.get_mut(key).expect("checked above");
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
        self.model_plan.0.get(key).map(Vec::as_slice)
    }

    /// Resolve each tiered choice the daemon may move again: `resolve`
    /// gives its new model and tier, or `None` to keep it. A model whose
    /// origin is unattributed is a pin and is passed over. A moved model is
    /// the daemon's, noted with `note` alone; the tier keeps who asked for it.
    pub fn retier(
        &mut self,
        item: &QueueItemId,
        (at, note): (Timestamp, Option<&str>),
        mut resolve: impl FnMut(
            &str,
            &TierChoice,
        ) -> Result<Option<(ModelRef, TierChoice)>, ArchonError>,
    ) -> Result<(), ArchonError> {
        let shape: Vec<usize> = self.model_plan.0.values().map(Vec::len).collect();
        // Resolved into copies, so a failure leaves both halves as they were.
        let mut plan = self.model_plan.clone();
        let mut origins = self.origins(item);
        for (key, choices) in plan.0.iter_mut() {
            let slots = origins.get_mut(key).expect("an origin per entry");
            for (choice, origin) in choices.iter_mut().zip(slots.iter_mut()) {
                let Some(asked) = &choice.tier else { continue };
                if origin.model.source == PlanSource::Unattributed {
                    continue;
                }
                let Some((model, tier)) = resolve(key, asked)? else {
                    continue;
                };
                debug_assert_eq!(tier.requested, asked.requested, "the tier asked for");
                let moved = model != choice.model || origin.model.source != PlanSource::Tier;
                if moved {
                    let note = note.map(str::to_owned);
                    origin.model =
                        FieldOrigin::chosen(PlanSource::Tier, Authority::Daemon, at, note);
                }
                debug_assert!(origin.tier.is_some(), "a tiered choice has a tier origin");
                choice.model = model;
                choice.tier = Some(tier);
            }
        }
        let after: Vec<usize> = plan.0.values().map(Vec::len).collect();
        assert_eq!(shape, after, "only models change");
        self.model_plan = plan;
        if self.model_origins.is_some() || origins.values().flatten().any(|o| !o.model.is_pin()) {
            self.model_origins = Some(Box::new(PlanOrigins {
                item: item.clone(),
                entries: origins,
            }));
        }
        Ok(())
    }
}

fn shape_of(
    plan: &ModelPlan,
    origin: impl Fn(&ModelChoice) -> EntryOrigin,
) -> BTreeMap<String, Vec<EntryOrigin>> {
    let entries = plan
        .0
        .iter()
        .map(|(key, choices)| (key.clone(), choices.iter().map(&origin).collect()));
    entries.collect()
}

/// Builds a [`PlannedPlan`] step by step: what a request names, what an
/// earlier plan of the same item keeps, then each fill step's additions.
#[derive(Debug)]
pub struct PlanWriter {
    item: QueueItemId,
    at: Timestamp,
    plan: ModelPlan,
    entries: BTreeMap<String, Vec<EntryOrigin>>,
}

impl PlanWriter {
    pub fn new(item: &QueueItemId, at: Timestamp) -> Self {
        Self {
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
            !model.is_pin() && !effort.is_pin(),
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

    pub fn finish(self) -> PlannedPlan {
        let planned = PlannedPlan {
            model_plan: self.plan,
            model_origins: Some(Box::new(PlanOrigins {
                item: self.item.clone(),
                entries: self.entries,
            })),
        };
        assert!(
            planned.faults(&self.item).is_empty(),
            "one origin per field"
        );
        planned
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
