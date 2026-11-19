//! The sizing decisions frozen into a plan (design 7.6, ruling E3'): which
//! keys the sizer decided, from which stage facts, under which cap and class.
//! Every later reader takes the stored decision; none decides again.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Authority, FieldOrigin, PlanSource, PlanWriter, PlannedPlan};
use crate::Timestamp;
use crate::autonomy::ReviewLevel;
use crate::effort_sizing::{self, ItemRisk, RiskClass, RiskInput, StageFacts};
use crate::id::QueueItemId;
use crate::model::{Effort, ModelChoice, ModelPlan};
use crate::outcome::DiffSize;

/// How one plan key was sized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KeySizing {
    /// The class the key was sized with; `None` until its item's own title,
    /// brief, tags and review level decide it, as the item is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_class: Option<RiskClass>,
    /// The signals of that class, so a key sized before they moved shows it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_signals: Vec<String>,
    /// The key's own stage facts, so no later reader borrows another's.
    pub facts: StageFacts,
    /// The cap in force when the key was sized.
    pub cap: Effort,
    /// What the sizer said of each choice, one per choice once sized.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

/// The sizing record of a plan: its item's class when a key was last sized,
/// and every key the sizer decided. A key without an entry was never sized.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanSizing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_class: Option<RiskClass>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_signals: Vec<String>,
    /// The diff size the item's last run measured when the plan was first
    /// sized. A size learned later never changes it; a `diff:` tag overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<DiffSize>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, KeySizing>,
}

/// A stored record as read: one that does not decode describes no key, so
/// its plan reads as never sized, which is how 1.6.3 reads every plan.
pub(super) fn lenient<'de, D>(stored: D) -> Result<Option<Box<PlanSizing>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(stored)?;
    Ok(serde_json::from_value(value).ok().map(Box::new))
}

impl PlanSizing {
    /// Whether every entry is of a key of `plan`, with a reason per choice
    /// once sized.
    pub(super) fn fits(&self, plan: &ModelPlan) -> bool {
        self.keys.iter().all(|(key, sized)| {
            let choices = plan.0.get(key).map(Vec::len);
            let reasons = sized.reasons.len();
            match sized.risk_class {
                Some(_) => choices == Some(reasons),
                None => choices.is_some() && reasons == 0,
            }
        })
    }

    /// The record to store: none when no key was ever sized.
    pub(super) fn stored(self) -> Option<Box<Self>> {
        (!self.keys.is_empty()).then(|| Box::new(self))
    }

    /// Take `key`'s entry of `kept`, for its first `choices` choices, and
    /// the class and size `kept` froze when this record has none yet.
    pub(super) fn carry(&mut self, kept: Option<&Self>, key: &str, choices: usize) {
        let Some(kept) = kept else { return };
        let Some(sized) = kept.keys.get(key).filter(|_| choices >= 1) else {
            return;
        };
        if self.keys.is_empty() {
            self.risk_class = kept.risk_class;
            self.risk_signals.clone_from(&kept.risk_signals);
            self.size = kept.size;
        }
        let mut sized = sized.clone();
        sized.reasons.truncate(choices);
        self.keys.insert(key.to_owned(), sized);
    }

    fn pending(&self) -> impl Iterator<Item = &String> {
        let unsized_yet = |(_, sized): &(&String, &KeySizing)| sized.risk_class.is_none();
        self.keys.iter().filter(unsized_yet).map(|(key, _)| key)
    }

    fn note(&mut self, key: &str, slot: usize, note: &str) {
        let reasons = self.keys.get_mut(key).map(|sized| &mut sized.reasons);
        if let Some(reason) = reasons.and_then(|reasons| reasons.get_mut(slot)) {
            reason.push_str("; ");
            reason.push_str(note);
        }
    }
}

/// How a writer's new keys are sized: `[models] effort_sizing` decides which.
#[derive(Debug, Clone, Copy)]
pub enum Sizing<'a> {
    /// Sizing is off: new keys keep what 1.6.3 gives them and get no record.
    /// Sized efforts carried from an earlier plan still stay within the policy.
    Off {
        policy_cap: Option<Effort>,
    },
    On(EffortSizer<'a>),
}

/// What the sizer needs that an item does not record itself.
#[derive(Clone, Copy)]
pub struct EffortSizer<'a> {
    /// The lower of the policy's cap and `[models] sizer_max_effort`.
    pub cap: Effort,
    /// Whether the sizer may set a stage above its default: `[models]
    /// sizer_max_effort` is set. Off, each stage's ceiling is its default.
    pub raises: bool,
    /// The diff size the item's last run measured; frozen with its first
    /// sizing, and read after the item's own `diff:` tag.
    pub size: Option<DiffSize>,
    /// A plan key's own stage facts; `None` for a key of no stage.
    pub facts: &'a dyn Fn(&str) -> Option<StageFacts>,
}

impl std::fmt::Debug for EffortSizer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut shown = f.debug_struct("EffortSizer");
        shown.field("cap", &self.cap).field("size", &self.size);
        shown.finish_non_exhaustive()
    }
}

impl Sizing<'_> {
    /// The cap a sized effort carried from an earlier plan is clipped to.
    fn carried_cap(&self) -> Option<Effort> {
        match self {
            Self::Off { policy_cap } => *policy_cap,
            Self::On(sizer) => Some(sizer.cap),
        }
    }
}

/// How one choice of a sized key got its effort: what an attempt records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotSizing {
    pub risk_class: RiskClass,
    pub effort_source: PlanSource,
    /// The sizer's reason; for a pinned effort, its advice and the pin.
    pub reason: String,
}

/// The one decision of a choice's effort: only an effort the stage default
/// gave (a fill, or one the sizer set before) is sized; a pin or an escalated
/// effort is left alone and a learned one is kept. `None` keeps the effort, and the reason says
/// which rule applied: unchanged or default-lowered (a raise shows in the target).
fn decide(
    (choice, origin): (&ModelChoice, &FieldOrigin),
    (facts, cap): (StageFacts, Effort),
    risk: (&ItemRisk, ReviewLevel),
) -> (Option<Effort>, String) {
    let sized = effort_sizing::size_effort(facts, risk, cap, &choice.model);
    assert!(sized.effort <= cap, "the sizer stays within its cap");
    if origin.is_pin() {
        return (None, sized.reason);
    }
    // A learned effort is kept as it is; the sizer origin lets dispatch clip it to the policy.
    if origin.source == PlanSource::Learned {
        let kept = format!("{}; unchanged: the learned route's effort", sized.reason);
        return (Some(choice.effort), kept);
    }
    if origin.source == PlanSource::Escalation {
        return (None, sized.reason);
    }
    let mut reason = sized.reason;
    if sized.effort == choice.effort && origin.source != PlanSource::Sizer {
        reason.push_str("; unchanged");
    } else if sized.effort < facts.base {
        reason.push_str("; default-lowered");
    }
    (Some(sized.effort), reason)
}

impl PlanWriter {
    /// The plan this writer built, its new keys sized as `sizing` says: with
    /// sizing on, each key resolved or filled here is recorded with its own
    /// stage facts, to be sized from its item's own fields as it is stored.
    pub fn finish(mut self, sizing: Sizing<'_>) -> PlannedPlan {
        self.clip(sizing.carried_cap());
        if let Sizing::On(sizer) = sizing {
            self.record_fresh(&sizer);
        }
        self.seal()
    }

    /// Whether sizing this writer's new keys is its plan's first sizing, the
    /// only one that reads and freezes the item's measured diff size.
    pub fn sizes_first(&self) -> bool {
        self.sizing.keys.is_empty() && !self.fresh.is_empty()
    }

    fn record_fresh(&mut self, sizer: &EffortSizer<'_>) {
        if self.sizing.keys.is_empty() {
            self.sizing.size = sizer.size;
        }
        for key in &self.fresh {
            let choices = self.plan.0.get(key).expect("a new key is of the plan");
            // An empty key (`workers: []`, which 1.6.3 accepts) has nothing to size.
            if choices.is_empty() {
                continue;
            }
            let Some(facts) = (sizer.facts)(key) else {
                continue;
            };
            let unsized_yet = KeySizing {
                risk_class: None,
                risk_signals: Vec::new(),
                facts,
                cap: effort_sizing::key_cap(sizer.cap, sizer.raises, facts.base),
                reasons: Vec::new(),
            };
            let was = self.sizing.keys.insert(key.clone(), unsized_yet);
            assert!(was.is_none(), "{key:?} is new, so nothing sized it before");
        }
    }

    /// Lower every effort the daemon sized or escalated to `cap` and to what
    /// its model's engine runs, with the reason. A pin is never lowered: the
    /// policy check refuses it.
    pub(super) fn clip(&mut self, cap: Option<Effort>) -> bool {
        let mut clipped = false;
        for (key, choices) in self.plan.0.iter_mut() {
            let slots = self.entries.get_mut(key).expect("an origin per entry");
            assert_eq!(slots.len(), choices.len(), "origins fit the plan");
            for (slot, (choice, origin)) in choices.iter_mut().zip(slots).enumerate() {
                let source = origin.effort.source;
                let sized = matches!(source, PlanSource::Sizer | PlanSource::Escalation);
                if !sized || origin.effort.is_pin() {
                    continue;
                }
                let Some((effort, note)) = lowered(choice, cap) else {
                    continue;
                };
                self.sizing.note(key, slot, &note);
                origin.effort = FieldOrigin::chosen(source, Authority::Daemon, self.at, Some(note));
                choice.effort = effort;
                clipped = true;
            }
        }
        clipped
    }

    /// Whether every effort the daemon sized is at or below `cap`.
    pub(super) fn sized_within(&self, cap: Effort) -> bool {
        let slots = self.plan.0.iter().flat_map(|(key, choices)| {
            let origins = self.entries.get(key).expect("an origin per entry");
            choices.iter().zip(origins)
        });
        slots.into_iter().all(|(choice, origin)| {
            let effort = &origin.effort;
            choice.effort <= cap || effort.is_pin() || effort.source != PlanSource::Sizer
        })
    }

    /// Decide every choice of the recorded `key` for an item of class `risk`.
    fn size_key(&mut self, key: &str, risk: (&ItemRisk, ReviewLevel)) {
        let sized = self.sizing.keys.get_mut(key).expect("a recorded key");
        let choices = self.plan.0.get_mut(key).expect("a record is of a plan key");
        let slots = self.entries.get_mut(key).expect("an origin per entry");
        assert_eq!(slots.len(), choices.len(), "origins fit the plan");
        sized.reasons.clear();
        for (choice, origin) in choices.iter_mut().zip(slots) {
            let within = (sized.facts, sized.cap);
            let (effort, reason) = decide((choice, &origin.effort), within, risk);
            sized.reasons.push(reason);
            let Some(effort) = effort else { continue };
            let was = format!("was {}: {}", origin.effort.source, choice.effort);
            let moved = effort != choice.effort || origin.effort.source != PlanSource::Sizer;
            if moved {
                let note = sized_note(origin.effort.note.as_deref(), &was);
                let (source, by) = (PlanSource::Sizer, Authority::Daemon);
                origin.effort = FieldOrigin::chosen(source, by, self.at, Some(note));
            }
            choice.effort = effort;
        }
        sized.risk_class = Some(risk.0.class);
        sized.risk_signals.clone_from(&risk.0.signals);
    }
}

/// What a sized effort of `choice` is lowered to, and why: the `cap`, then
/// `high` on an engine that runs no `x_high`. `None` when the effort stands.
fn lowered(choice: &ModelChoice, cap: Option<Effort>) -> Option<(Effort, String)> {
    let was = choice.effort;
    let mut effort = was;
    let mut notes = Vec::new();
    if let Some(cap) = cap.filter(|cap| was > *cap) {
        effort = cap;
        notes.push(format!("clipped from {was} to the cap {cap}"));
    }
    if effort == Effort::XHigh && !effort_sizing::supports_x_high(&choice.model) {
        effort = Effort::High;
        let engine = &choice.model.provider;
        notes.push(format!("{engine} runs no more than high, so high"));
    }
    assert!(effort <= was, "a sized effort is only ever lowered");
    assert_eq!(notes.is_empty(), effort == was, "a lowered effort says why");
    (effort < was).then(|| (effort, notes.join("; ")))
}

/// The note of an effort the sizer just set: what its writer said of it (a
/// refresh names its owner), then what it `was`. An earlier "was" is dropped.
fn sized_note(said: Option<&str>, was: &str) -> String {
    const WAS: &str = "was ";
    assert!(was.starts_with(WAS), "the sizer says what the effort was");
    let said = said.unwrap_or_default();
    let kept = match said.find("; was ") {
        Some(end) => &said[..end],
        None if said.starts_with(WAS) => "",
        None => said,
    };
    if kept.is_empty() {
        return was.to_owned();
    }
    format!("{kept}; {was}")
}

impl PlannedPlan {
    /// The sizing record, when it describes this plan's keys.
    pub fn sizing(&self) -> Option<&PlanSizing> {
        let sizing = self.effort_sizing.as_deref()?;
        sizing.fits(&self.model_plan).then_some(sizing)
    }

    pub(super) fn forget_sizing(&mut self, key: &str) {
        let Some(sizing) = &mut self.effort_sizing else {
            return;
        };
        sizing.keys.remove(key);
        if sizing.keys.is_empty() {
            self.effort_sizing = None;
        }
    }

    /// Whether a key is recorded for sizing and not yet sized.
    pub fn has_unsized(&self) -> bool {
        self.sizing().is_some_and(|s| s.pending().next().is_some())
    }

    /// The class of the item as its fields and the frozen diff size say.
    fn risk(&self, input: RiskInput<'_>) -> ItemRisk {
        let frozen = self.sizing().and_then(|sizing| sizing.size);
        let size = input.size.or(frozen);
        effort_sizing::classify(RiskInput { size, ..input })
    }

    /// Size the `keys` recorded here for an item of class `risk`, and record
    /// that class as the item's.
    fn size_keys(
        &mut self,
        item: &QueueItemId,
        keys: &[String],
        risk: (&ItemRisk, ReviewLevel),
        (cap, at): (Option<(Effort, bool)>, Timestamp),
    ) {
        let mut writer = PlanWriter::editing(self, item, at);
        for key in keys {
            if let Some((cap, raises)) = cap {
                let sized = writer.sizing.keys.get_mut(key).expect("a recorded key");
                sized.cap = effort_sizing::key_cap(cap, raises, sized.facts.base);
            }
            writer.size_key(key, risk);
        }
        writer.sizing.risk_class = Some(risk.0.class);
        writer.sizing.risk_signals.clone_from(&risk.0.signals);
        *self = writer.seal();
        debug_assert!(
            keys.iter()
                .all(|key| self.slot_sizing(item, key, 0).is_some())
        );
    }

    /// Size every key recorded and not yet sized, from `input`: the stored
    /// fields of the item this plan is of. `true` when one was.
    pub fn settle(&mut self, item: &QueueItemId, input: RiskInput<'_>, at: Timestamp) -> bool {
        let Some(sizing) = self.sizing() else {
            return false;
        };
        let pending: Vec<String> = sizing.pending().cloned().collect();
        if pending.is_empty() {
            return false;
        }
        let risk = self.risk(input);
        self.size_keys(item, &pending, (&risk, input.review), (None, at));
        assert!(!self.has_unsized(), "every recorded key is sized");
        true
    }

    /// Size again, within `cap`, each sized key `started` does not name whose
    /// class or signals `input` changed. A key never sized stays unsized, and
    /// a pinned effort stays what it is. `true` when a key was sized again.
    pub fn resize(
        &mut self,
        item: &QueueItemId,
        input: RiskInput<'_>,
        (cap, raises, at): (Effort, bool, Timestamp),
        started: &dyn Fn(&str) -> bool,
    ) -> bool {
        let Some(sizing) = self.sizing() else {
            return false;
        };
        let risk = self.risk(input);
        // Each key by what it was sized with: sizing one key vouches for no other.
        let stale = |sized: &KeySizing| match sized.risk_class {
            Some(class) => class != risk.class || sized.risk_signals != risk.signals,
            None => false,
        };
        let keys = sizing.keys.iter().filter(|(_, sized)| stale(sized));
        let keys: Vec<String> = keys
            .map(|(key, _)| key.clone())
            .filter(|key| !started(key))
            .collect();
        if keys.is_empty() {
            return false;
        }
        self.size_keys(
            item,
            &keys,
            (&risk, input.review),
            (Some((cap, raises)), at),
        );
        true
    }

    /// Lower every effort the daemon sized to `cap` and to what its model's
    /// engine runs, with the reason; pins are left for the policy check to
    /// refuse. `true` when one was lowered.
    pub fn clip_sized(&mut self, item: &QueueItemId, cap: Option<Effort>, at: Timestamp) -> bool {
        let stands = |choice: &ModelChoice| lowered(choice, cap).is_none();
        if self.model_plan.0.values().flatten().all(stands) {
            return false;
        }
        let mut writer = PlanWriter::editing(self, item, at);
        if !writer.clip(cap) {
            return false;
        }
        *self = writer.seal();
        assert!(cap.is_none_or(|cap| self.sized_within(item, cap)));
        true
    }

    /// Whether every effort the daemon sized is at or below `cap`.
    fn sized_within(&self, item: &QueueItemId, cap: Effort) -> bool {
        let within = |(key, choices): (&String, &Vec<ModelChoice>)| {
            choices.iter().enumerate().all(|(slot, choice)| {
                let origin = self.origin(item, key, slot).effort;
                choice.effort <= cap || origin.is_pin() || origin.source != PlanSource::Sizer
            })
        };
        self.model_plan.0.iter().all(within)
    }

    /// How choice `slot` of `key` got its effort, when the sizer decided the key.
    pub fn slot_sizing(&self, item: &QueueItemId, key: &str, slot: usize) -> Option<SlotSizing> {
        let sized = self.sizing()?.keys.get(key)?;
        let risk_class = sized.risk_class?;
        let choice = self.model_plan.0.get(key)?.get(slot)?;
        let origin = self.origin(item, key, slot).effort;
        let said = sized.reasons.get(slot)?;
        let reason = match origin.is_pin() {
            true => format!("{said}; the plan pins {}", choice.effort),
            false => said.clone(),
        };
        Some(SlotSizing {
            risk_class,
            effort_source: origin.source,
            reason,
        })
    }

    /// Why each sized key's choices have their efforts, for display.
    pub fn effort_reasons(&self, item: &QueueItemId) -> BTreeMap<String, Vec<String>> {
        let Some(sizing) = self.sizing() else {
            return BTreeMap::new();
        };
        let reasons = |key: &String| {
            let choices = self.model_plan.0.get(key).map_or(0, Vec::len);
            let slots = (0..choices).filter_map(|slot| self.slot_sizing(item, key, slot));
            slots.map(|sized| sized.reason).collect::<Vec<_>>()
        };
        let shown = sizing.keys.keys().map(|key| (key.clone(), reasons(key)));
        shown.filter(|(_, reasons)| !reasons.is_empty()).collect()
    }
}

#[cfg(test)]
#[path = "sizing_tests.rs"]
mod tests;
