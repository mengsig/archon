//! Steering an item without cancelling it: what `steer_item` asks for, and
//! the changes to a run's shape that it records.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::autonomy::AutonomyPatch;
use crate::error::ArchonError;
use crate::model::{Effort, ModelRef};
use crate::queue::check_item_budget;

pub const MAX_EXTEND_MINUTES: u32 = 24 * 60;
/// How many steers an item's history keeps; the oldest go first.
pub const MAX_STEER_RECORDS: usize = 100;

/// Who may steer an item (ARCHITECTURE §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeredBy {
    Operator,
    Orchestrator,
}

impl std::fmt::Display for SteeredBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Operator => "operator",
            Self::Orchestrator => "orchestrator",
        })
    }
}

/// One steer in an item's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerRecord {
    pub at: Timestamp,
    pub by: SteeredBy,
    /// One line naming everything the steer changed.
    pub what: String,
    /// The note it amended the brief with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What the run does once a stage was aborted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "then", rename_all = "snake_case")]
pub enum Then {
    Retry,
    Skip,
    Goto { to: String },
}

/// `stage` of catalog pipeline `from` (the item's own when left out),
/// placed after stage `after` (before the first unstarted one when left out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertStage {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Steer {
    /// Amends the brief and is sent to the item's live agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The stage `model`, `effort` and `abort` are about; the running one
    /// when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// The item's new dollar cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extend_minutes: Option<u32>,
    /// Timeout overrides for stages spawned after this steer.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub stage_timeouts: std::collections::BTreeMap<String, u32>,
    /// Abort the stage, then do this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<Then>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_stage: Option<InsertStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_pipeline: Option<String>,
    /// Knobs to change, within the item's ceiling; for a running item the
    /// stages already started keep theirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<AutonomyPatch>,
}

impl Steer {
    /// Everything that can be refused without the item or the catalog.
    pub fn check(&self) -> Result<(), ArchonError> {
        if self == &Self::default() || (self.stage.is_some() && !self.addresses_a_stage()) {
            return Err(ArchonError::Invalid(
                "steer_item needs something to change".into(),
            ));
        }
        if self.note.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err(ArchonError::Invalid("note is empty".into()));
        }
        check_item_budget(self.budget_usd)?;
        if self.autonomy.is_some_and(|a| a.is_empty()) {
            return Err(ArchonError::Invalid("autonomy names no knob".into()));
        }
        if self
            .extend_minutes
            .is_some_and(|m| m == 0 || m > MAX_EXTEND_MINUTES)
        {
            return Err(ArchonError::Invalid(format!(
                "an extension is 1 to {MAX_EXTEND_MINUTES} minutes"
            )));
        }
        if self.insert_stage.is_some() && self.switch_pipeline.is_some() {
            return Err(ArchonError::Invalid(
                "give insert_stage or switch_pipeline, not both".into(),
            ));
        }
        let names = [
            self.stage.as_deref(),
            self.switch_pipeline.as_deref(),
            self.insert_stage.as_ref().map(|i| i.stage.as_str()),
            match &self.abort {
                Some(Then::Goto { to }) => Some(to.as_str()),
                _ => None,
            },
        ];
        if names.into_iter().flatten().any(|n| n.trim().is_empty()) {
            return Err(ArchonError::Invalid(
                "stage and pipeline names must not be empty".into(),
            ));
        }
        Ok(())
    }

    fn addresses_a_stage(&self) -> bool {
        self.model.is_some() || self.effort.is_some() || self.abort.is_some()
    }

    /// Whether it records a [`RunEdit`], which only an unfinished run takes.
    pub fn changes_shape(&self) -> bool {
        self.abort.is_some() || self.insert_stage.is_some() || self.switch_pipeline.is_some()
    }
}

/// One recorded change to a run's shape. A run's edits apply in order before
/// it resumes, and each holds what it needs, so replay never reads the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "edit", rename_all = "snake_case")]
pub enum RunEdit {
    /// `spec` is the stage as the catalog had it when the edit was recorded.
    InsertStage {
        after: Option<String>,
        spec: serde_json::Value,
    },
    /// `stages` replace every stage that had not started.
    SwitchPipeline {
        pipeline: String,
        stages: Vec<serde_json::Value>,
    },
    AbortStage {
        stage: String,
        attempt: u32,
        #[serde(flatten)]
        then: Then,
    },
    /// The daemon, not a steer: a planner's validated swarm plan gives team
    /// stage `stage` its `roles`, one per task, each with its task prompt.
    AssignRoles {
        stage: String,
        roles: Vec<String>,
        role_prompts: std::collections::BTreeMap<String, String>,
    },
}

impl RunEdit {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::InsertStage { .. } => "insert_stage",
            Self::SwitchPipeline { .. } => "switch_pipeline",
            Self::AbortStage { .. } => "abort_stage",
            Self::AssignRoles { .. } => "assign_roles",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_steer_needs_something_valid_to_change() {
        assert!(Steer::default().check().is_err());
        let only_stage = Steer {
            stage: Some("review".into()),
            ..Steer::default()
        };
        assert!(only_stage.check().is_err());
        let note = |text: &str| Steer {
            note: Some(text.into()),
            ..Steer::default()
        };
        assert!(note("Use the v2 client.").check().is_ok());
        assert!(note("  ").check().is_err());
        let knobs = |list: &str| Steer {
            autonomy: Some(AutonomyPatch::parse_list(list).unwrap()),
            ..Steer::default()
        };
        assert!(knobs("merge=auto").check().is_ok());
        assert!(knobs("").check().is_err(), "autonomy names a knob");
        let extend = |minutes| Steer {
            extend_minutes: Some(minutes),
            ..Steer::default()
        };
        assert!(extend(0).check().is_err());
        assert!(extend(MAX_EXTEND_MINUTES).check().is_ok());
        assert!(extend(MAX_EXTEND_MINUTES + 1).check().is_err());
        let budget = Steer {
            budget_usd: Some(-1.0),
            ..Steer::default()
        };
        assert!(budget.check().is_err());
    }

    #[test]
    fn shape_changes_are_named_and_exclusive() {
        let insert = InsertStage {
            stage: "review".into(),
            from: Some("standard-pr".into()),
            after: None,
        };
        let both = Steer {
            insert_stage: Some(insert.clone()),
            switch_pipeline: Some("standard-pr".into()),
            ..Steer::default()
        };
        assert!(both.check().is_err());
        let one = Steer {
            insert_stage: Some(insert),
            ..Steer::default()
        };
        assert!(one.check().is_ok() && one.changes_shape());
        let nowhere = Steer {
            abort: Some(Then::Goto { to: " ".into() }),
            ..Steer::default()
        };
        assert!(nowhere.check().is_err());
        let effort = Steer {
            effort: Some(Effort::High),
            ..Steer::default()
        };
        assert!(effort.check().is_ok() && !effort.changes_shape());
    }

    #[test]
    fn steers_and_edits_have_a_flat_wire_form() {
        let steer = Steer {
            abort: Some(Then::Goto {
                to: "implement".into(),
            }),
            ..Steer::default()
        };
        let wire = json!({"abort": {"then": "goto", "to": "implement"}});
        assert_eq!(serde_json::to_value(&steer).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Steer>(wire).unwrap(), steer);

        let edit = RunEdit::AbortStage {
            stage: "review".into(),
            attempt: 2,
            then: Then::Retry,
        };
        let stored =
            json!({"edit": "abort_stage", "stage": "review", "attempt": 2, "then": "retry"});
        assert_eq!(serde_json::to_value(&edit).unwrap(), stored);
        assert_eq!(serde_json::from_value::<RunEdit>(stored).unwrap(), edit);
        assert_eq!(edit.kind(), "abort_stage");
    }
}
