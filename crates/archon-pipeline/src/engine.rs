//! The pipeline run state machine (ARCHITECTURE §6.4). Pure: it never
//! spawns or persists. The daemon feeds it settled stage runs and executes
//! the actions it returns. All state is derivable from persisted stage runs,
//! so [`RunDriver::resume`] rebuilds it exactly after a restart.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use archon_core::catalog::{StageKind, WorkspaceMode};
use archon_core::error::ArchonError;
use archon_core::id::{PipelineRunId, StageRunId};
use archon_core::model::{ModelChoice, ModelPlan};
use archon_core::repo::Attribution;
use archon_core::run::{
    GateDecision, GateSubject, PipelineRun, RunState, StageOutcome, StageRun, StageState,
};
use archon_core::steer::Then;

use crate::autonomy::{self, OperatorGates};
use crate::gate::{self, GroupResult};
use crate::ship::{self, BodyAdditions, SHIP_STEP, ShipOutcome, ShipPlan, ShipRecord};
use crate::spec::{PipelineSpec, ReviewDuty, StageSpec};
use crate::team::{self, MergeOutcome, MergePlan, RoleBranch, TeamSlot};
use crate::{contract, review, sections, template};

#[derive(Debug, Clone, PartialEq)]
pub enum EngineAction {
    /// Start one stage agent once capacity allows. Slots of one stage are
    /// emitted together; the daemon may admit them partially.
    Spawn(SpawnPlan),
    CancelStage {
        stage_run: StageRunId,
        reason: String,
    },
    Finish {
        state: RunState,
    },
    /// An operator gate's stage passed: the run waits for a decision,
    /// delivered through [`RunDriver::on_operator_decision`].
    AwaitOperator(OperatorWait),
    /// Every role of a team stage passed: merge their branches onto the run
    /// branch, then report through [`RunDriver::on_team_merged`].
    MergeTeam(MergePlan),
    /// Every stage of a pipeline archond ships passed: deliver its commits,
    /// then report through [`RunDriver::on_shipped`].
    Ship(ShipPlan),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OperatorWait {
    pub stage_id: String,
    pub attempt: u32,
    pub subject: GateSubject,
    /// The passed stage's summary, shown to the operator.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPlan {
    pub stage_id: String,
    /// The stage's kind, recorded on its stage run for later attribution.
    pub kind: StageKind,
    pub attempt: u32,
    /// 0-based slot within the stage's `fan_out`.
    pub fan_index: u32,
    /// The stage's `fan_out`, so an agent can be named "2/3".
    pub fan_count: u32,
    pub choice: ModelChoice,
    /// Rendered stage prompt (task objective).
    pub prompt: String,
    /// Stage contract (ARCHITECTURE §8.6) for `config.instructions`.
    pub instructions: String,
    pub timeout_minutes: u32,
    pub allow_delegation: bool,
    /// The pipeline's workspace mode: what a delegating agent's children may do.
    pub workspace: WorkspaceMode,
    /// The slot's role, branch and worktree on a team stage; `None` for
    /// every other kind, whose agents work in the run's workspace.
    pub team: Option<TeamSlot>,
    /// A later stage may loop back here: the agent is kept after its task
    /// settles so that the next attempt can resume it.
    pub keep: bool,
    /// What the kept agent of the slot's previous attempt is told instead of
    /// spawning anew; `None` when only a fresh agent will do.
    pub resume: Option<String>,
    /// Set when the agent owes reviewers: the models the plan names for them,
    /// empty when the item's current model policy alone bounds them.
    pub reviewers: Option<Vec<ModelChoice>>,
}

/// Errors carried into `RunState::Failed` are capped; the full feedback
/// stays in the stage runs.
pub const MAX_ERROR_CHARS: usize = 8_000;

/// Operator revisions per gate stage and run, like `on_fail.max_loops`.
pub const MAX_REVISIONS: u32 = 5;

#[derive(Debug)]
pub struct RunDriver {
    spec: Arc<PipelineSpec>,
    plan: ModelPlan,
    /// Run-wide template variables (task, repo, workspace, branch, ship).
    base_vars: BTreeMap<String, String>,
    /// `(stage index, attempt)` groups whose spawns this instance already
    /// emitted; `resume` clears it. Only a duplicate-settle guard.
    emitted: BTreeSet<(usize, u32)>,
    /// Set once a `Finish` was emitted or the resumed run was terminal.
    finished: bool,
    gates: OperatorGates,
    /// Operator decisions by `(stage id, attempt)`; persisted by the daemon.
    decisions: BTreeMap<(String, u32), GateDecision>,
    /// `(stage index, attempt)` gates this instance already announced.
    awaited: BTreeSet<(usize, u32)>,
    /// Team merge outcomes by `(stage id, attempt)`; persisted by the daemon.
    merges: BTreeMap<(String, u32), MergeOutcome>,
    /// `(stage index, attempt)` merges this instance already asked for.
    merging: BTreeSet<(usize, u32)>,
    /// How archond's delivery of the run ended; persisted by the daemon.
    shipped: Option<ShipOutcome>,
    /// Whether this instance already asked for that delivery.
    shipping: bool,
    /// Team siblings this instance already asked to cancel.
    cancelling: BTreeSet<StageRunId>,
    /// Steered aborts by `(stage id, attempt)`; persisted by the daemon.
    aborts: BTreeMap<(String, u32), Then>,
    /// The item's directory for reviewers' experiment copies, which the
    /// contract names; `None` leaves them to `mktemp -d`.
    experiments: Option<PathBuf>,
    /// The repository's attribution policy, which decides the body's last line.
    attribution: Attribution,
}

/// A live role of a team group another role already failed, and why it is
/// cancelled.
type SiblingCancel = (StageRunId, String);

/// Where the replay of persisted stage runs stopped.
#[derive(Debug)]
enum Replay {
    /// The group at `at` is unfinished; `missing` slots have no stage run.
    Await { at: Position, missing: Vec<u32> },
    /// The run is decided; `cancel` lists live stage runs to stop first.
    Stop {
        cancel: Vec<StageRunId>,
        state: RunState,
    },
    /// The gate stage at index `stage` passed and has no decision yet.
    AwaitOperator { stage: usize, wait: OperatorWait },
    /// The team stage at index `stage` passed and its merge has no outcome.
    AwaitMerge { stage: usize, plan: MergePlan },
    /// Every stage passed and archond's delivery has no outcome.
    AwaitShip { plan: ShipPlan },
}

/// Per-stage counters of one replay.
#[derive(Debug)]
struct Counters {
    /// How often each stage was entered: its latest attempt.
    entries: Vec<u32>,
    /// `on_fail` loops taken from each stage.
    loops: Vec<u32>,
    /// Operator revisions requested at each gate stage.
    revisions: Vec<u32>,
}

/// A stage group in the replay plus the template context it runs with.
#[derive(Debug)]
struct Position {
    stage: usize,
    attempt: u32,
    feedback: String,
    /// `previous.<id>.summary` values of the stages passed so far.
    previous: BTreeMap<String, String>,
    /// Every `(stage, attempt)` group entered so far, this one included.
    entered: BTreeSet<(usize, u32)>,
    /// Gate stages the operator approved, with the operator's note.
    approved: BTreeMap<String, Option<String>>,
    /// The ship record of the record stage's latest pass, and the review
    /// line Archon wrote for it.
    record: Option<(ShipRecord, Option<String>)>,
}

impl Position {
    /// The first stage's first attempt, recorded in `entries`.
    fn first(entries: &mut [u32]) -> Self {
        let mut at = Self {
            stage: 0,
            attempt: 0,
            feedback: String::new(),
            previous: BTreeMap::new(),
            entered: BTreeSet::new(),
            approved: BTreeMap::new(),
            record: None,
        };
        at.enter(0, entries, String::new());
        at
    }

    /// Enter `stage` once more: its attempt is how often it was entered.
    fn enter(&mut self, stage: usize, entries: &mut [u32], feedback: String) {
        entries[stage] += 1;
        self.stage = stage;
        self.attempt = entries[stage];
        self.feedback = feedback;
        let fresh = self.entered.insert((stage, self.attempt));
        debug_assert!(fresh && self.attempt >= 1, "a group is entered once");
    }
}

type SlotKey<'a> = (&'a str, u32, u32);

impl RunDriver {
    /// Refuses a plan that does not name every stage, and base variables
    /// that would leave a prompt unrenderable.
    pub fn new(
        spec: Arc<PipelineSpec>,
        plan: ModelPlan,
        base_vars: BTreeMap<String, String>,
    ) -> Result<Self, ArchonError> {
        check_plan(&spec, &plan)?;
        check_base_vars(&base_vars)?;
        check_team_layout(&spec, &base_vars)?;
        let driver = Self {
            spec,
            plan,
            base_vars,
            emitted: BTreeSet::new(),
            finished: false,
            gates: OperatorGates::default(),
            decisions: BTreeMap::new(),
            awaited: BTreeSet::new(),
            merges: BTreeMap::new(),
            merging: BTreeSet::new(),
            shipped: None,
            shipping: false,
            cancelling: BTreeSet::new(),
            aborts: BTreeMap::new(),
            experiments: None,
            attribution: Attribution::default(),
        };
        driver.check_prompts_render()?;
        Ok(driver)
    }

    /// Make the stages `gates` names wait for the operator once they pass.
    pub fn with_gates(mut self, gates: OperatorGates) -> Result<Self, ArchonError> {
        assert!(
            self.emitted.is_empty() && !self.finished,
            "gates are set before the driver acts"
        );
        let named = [
            (&gates.proposal, StageKind::Propose),
            (&gates.report, StageKind::Report),
        ];
        for (id, kind) in named {
            let Some(id) = id else { continue };
            if self.spec.stage(id).is_none_or(|s| s.kind != kind) {
                return Err(ArchonError::Pipeline(format!(
                    "operator gate {id:?} names no {kind:?} stage of pipeline {:?}",
                    self.spec.name()
                )));
            }
        }
        self.gates = gates;
        Ok(self)
    }

    /// Name `dir` in review contracts as where experiment copies go. The
    /// daemon creates it before such a stage starts.
    pub fn with_experiments(mut self, dir: PathBuf) -> Self {
        assert!(
            self.emitted.is_empty() && !self.finished,
            "the directory is set before the driver acts"
        );
        assert!(dir.is_absolute(), "the experiments directory is absolute");
        self.experiments = Some(dir);
        self
    }

    /// The repository's attribution policy for the pull request this run
    /// may open; the default keeps the `Archon item:` line.
    pub fn with_attribution(mut self, attribution: Attribution) -> Self {
        assert!(
            self.emitted.is_empty() && !self.finished && !self.shipping,
            "the policy is set before the driver acts"
        );
        assert!(self.shipped.is_none(), "set before a delivery is replayed");
        self.attribution = attribution;
        self
    }

    /// The stages that wait for the operator once they pass.
    pub fn gates(&self) -> &OperatorGates {
        &self.gates
    }

    /// Record an operator decision without acting on it: resume needs the
    /// decisions persisted before a restart. Deciding a gate twice alike
    /// is a no-op; deciding it differently is refused.
    pub fn record_decision(
        &mut self,
        stage_id: &str,
        attempt: u32,
        decision: GateDecision,
    ) -> Result<(), ArchonError> {
        assert!(attempt >= 1, "attempts are 1-based");
        if self.gates.subject(stage_id).is_none() {
            return Err(ArchonError::InvalidTransition(format!(
                "stage {stage_id:?} of pipeline {:?} is not an operator gate",
                self.spec.name()
            )));
        }
        let key = (stage_id.to_owned(), attempt);
        match self.decisions.get(&key) {
            Some(known) if *known == decision => Ok(()),
            Some(known) => Err(ArchonError::InvalidTransition(format!(
                "gate {stage_id}#{attempt} was already {}",
                known.word()
            ))),
            None => {
                self.decisions.insert(key, decision);
                Ok(())
            }
        }
    }

    /// The operator decided a gate: go on, loop back with the feedback, or
    /// finish cancelled. `all` is every stage run of the run.
    pub fn on_operator_decision(
        &mut self,
        stage_id: &str,
        attempt: u32,
        decision: GateDecision,
        all: &[StageRun],
    ) -> Result<Vec<EngineAction>, ArchonError> {
        self.record_decision(stage_id, attempt, decision)?;
        debug_assert!(self.decisions.contains_key(&(stage_id.to_owned(), attempt)));
        if self.finished {
            return Ok(Vec::new());
        }
        Ok(self.act_on_runs(all))
    }

    /// Record a team merge outcome without acting on it: resume needs the
    /// outcomes persisted before a restart. The same outcome twice is a
    /// no-op; a different one is refused.
    pub fn record_merge(
        &mut self,
        stage_id: &str,
        attempt: u32,
        outcome: MergeOutcome,
    ) -> Result<(), ArchonError> {
        assert!(attempt >= 1, "attempts are 1-based");
        let Some(stage) = self
            .spec
            .stage(stage_id)
            .filter(|s| s.kind == StageKind::Team)
        else {
            return Err(ArchonError::InvalidTransition(format!(
                "stage {stage_id:?} of pipeline {:?} is not a team stage",
                self.spec.name()
            )));
        };
        if let Some(role) = outcome.role()
            && !stage.roles.iter().any(|r| r == role)
        {
            return Err(ArchonError::InvalidTransition(format!(
                "team stage {stage_id:?} has no role {role:?}"
            )));
        }
        let key = (stage_id.to_owned(), attempt);
        match self.merges.get(&key) {
            Some(known) if *known == outcome => Ok(()),
            Some(known) => Err(ArchonError::InvalidTransition(format!(
                "team merge {stage_id}#{attempt} was already recorded as {known:?}"
            ))),
            None => {
                self.merges.insert(key, outcome);
                Ok(())
            }
        }
    }

    /// The daemon merged a passed team attempt: go on, or treat a conflict
    /// as the stage failing. `all` is every stage run of the run.
    pub fn on_team_merged(
        &mut self,
        stage_id: &str,
        attempt: u32,
        outcome: MergeOutcome,
        all: &[StageRun],
    ) -> Result<Vec<EngineAction>, ArchonError> {
        self.record_merge(stage_id, attempt, outcome)?;
        debug_assert!(self.merges.contains_key(&(stage_id.to_owned(), attempt)));
        if self.finished {
            return Ok(Vec::new());
        }
        Ok(self.act_on_runs(all))
    }

    /// Record how archond's delivery ended, without acting on it, for resume;
    /// the same outcome again is a no-op, a different one refused.
    pub fn record_ship(&mut self, outcome: ShipOutcome) -> Result<(), ArchonError> {
        if self.spec.ship_record_stage().is_none() {
            return Err(ArchonError::InvalidTransition(format!(
                "pipeline {:?} is not shipped by archond",
                self.spec.name()
            )));
        }
        match &self.shipped {
            Some(known) if *known == outcome => Ok(()),
            Some(known) => Err(ArchonError::InvalidTransition(format!(
                "the run's delivery was already recorded as {known:?}"
            ))),
            None => {
                self.shipped = Some(outcome);
                Ok(())
            }
        }
    }

    /// archond delivered the run, or could not: finish it either way. `all`
    /// is every stage run of the run.
    pub fn on_shipped(
        &mut self,
        outcome: ShipOutcome,
        all: &[StageRun],
    ) -> Result<Vec<EngineAction>, ArchonError> {
        self.record_ship(outcome)?;
        debug_assert!(self.shipped.is_some());
        if self.finished {
            return Ok(Vec::new());
        }
        Ok(self.act_on_runs(all))
    }

    /// Whether archond ships this run itself once its last stage passed.
    pub fn ships_by_daemon(&self) -> bool {
        self.spec.ship_record_stage().is_some()
    }

    /// The roles of team stage `stage_id` in slot order; `None` for any
    /// other stage.
    pub fn team_roles(&self, stage_id: &str) -> Option<&[String]> {
        let stage = self.spec.stage(stage_id)?;
        if stage.kind != StageKind::Team {
            return None;
        }
        debug_assert_eq!(stage.roles.len(), stage.fan_out as usize);
        Some(&stage.roles)
    }

    /// Whether stage `stage_id` hands in the ship record archond delivers.
    pub fn records_ship(&self, stage_id: &str) -> bool {
        let records = self.spec.ship_record_stage();
        records.is_some_and(|stage| stage.id == stage_id)
    }

    /// How many independent reviewers stage `stage_id` owes this run as the
    /// review its pipeline declares; `0` when it holds none.
    pub fn reviewers_owed(&self, stage_id: &str) -> u32 {
        let stage = self.spec.stage(stage_id);
        let duty = stage.and_then(|stage| self.spec.review_duty(stage));
        duty.map_or(0, |duty| duty.reviewers)
    }

    /// Whether this instance cancelled `stage_run` as the live sibling of
    /// a team role that failed, rather than because the run finished.
    pub fn cancels_sibling(&self, stage_run: &StageRunId) -> bool {
        self.cancelling.contains(stage_run)
    }

    /// Record a steer's abort of group `(stage_id, attempt)`; attempt 0 with
    /// a skip removes an unstarted stage. Then `resume` or [`Self::reconsider`].
    pub fn record_abort(
        &mut self,
        stage_id: &str,
        attempt: u32,
        then: Then,
    ) -> Result<(), ArchonError> {
        let stages = &self.spec.file.stages;
        let Some(index) = self.spec.stage_index(stage_id) else {
            return Err(ArchonError::Invalid(format!(
                "pipeline {:?} has no stage {stage_id:?} to abort",
                self.spec.name()
            )));
        };
        if attempt == 0 && then != Then::Skip {
            return Err(ArchonError::Invalid(format!(
                "stage {stage_id:?} has not started, so there is nothing to abort; it can only be skipped"
            )));
        }
        let records = |id: &str| self.spec.ship_record_stage().is_some_and(|s| s.id == id);
        match &then {
            Then::Retry => {}
            Then::Skip if index + 1 == stages.len() => {
                return Err(ArchonError::Invalid(format!(
                    "stage {stage_id:?} is the last one, so nothing follows a skip: cancel the item instead"
                )));
            }
            Then::Skip if records(stage_id) => {
                return Err(ArchonError::Invalid(format!(
                    "stage {stage_id:?} writes the ship record Archon delivers, so it cannot be skipped: retry it, or cancel the item"
                )));
            }
            Then::Skip => {}
            Then::Goto { to } if self.spec.stage_index(to).is_none() => {
                return Err(ArchonError::Invalid(format!(
                    "pipeline {:?} has no stage {to:?} to go to",
                    self.spec.name()
                )));
            }
            Then::Goto { .. } => {}
        }
        self.aborts.insert((stage_id.to_owned(), attempt), then);
        Ok(())
    }

    /// The decision the persisted stage runs imply now, after an abort was
    /// recorded on a live driver. Spawns come once per group, as ever.
    pub fn reconsider(&mut self, all: &[StageRun]) -> Vec<EngineAction> {
        assert!(
            all.windows(2).all(|pair| pair[0].run == pair[1].run),
            "stage runs of several runs passed together"
        );
        if self.finished {
            return Vec::new();
        }
        self.act_on_runs(all)
    }

    /// Whether `stage`'s agent outlives its task: a single work agent a later
    /// stage can send back, unless it holds the declared review.
    pub fn keeps_agent(&self, stage_id: &str) -> bool {
        let stages = &self.spec.file.stages;
        let Some(stage) = self.spec.stage(stage_id) else {
            return false;
        };
        let looped_to = stages
            .iter()
            .filter_map(|s| s.on_fail.as_ref())
            .any(|on_fail| on_fail.goto == stage.id);
        let single_worker = stage.kind == StageKind::Work && stage.fan_out == 1;
        single_worker && looped_to && !self.spec.holds_review(stage)
    }

    /// Actions for a brand-new run: the first stage's spawns.
    pub fn start(&mut self) -> Vec<EngineAction> {
        assert!(
            !self.finished && self.emitted.is_empty(),
            "start() is only for a fresh driver"
        );
        self.act_on_runs(&[])
    }

    /// Rebuild after a restart and return the spawns still owed (slots of
    /// the current stage with no stage run yet); they replace any spawns
    /// still pending from before. If the persisted runs already decide the
    /// stage, returns that decision instead.
    pub fn resume(&mut self, run: &PipelineRun, stages: &[StageRun]) -> Vec<EngineAction> {
        assert_eq!(
            run.pipeline,
            self.spec.name(),
            "driver resumed for another pipeline"
        );
        assert!(
            stages.iter().all(|s| s.run == run.id),
            "stage runs of another run passed to resume"
        );
        self.emitted.clear();
        self.awaited.clear();
        self.merging.clear();
        self.shipping = false;
        self.cancelling.clear();
        self.finished = run.state.is_terminal();
        if self.finished {
            return Vec::new();
        }
        self.act_on_runs(stages)
    }

    /// Feed one settled stage run (terminal state, verdict, summary set).
    /// Returns nothing until every slot of the stage is terminal; then the
    /// gate decides: next stage, loop back with feedback, or finish. A
    /// `BLOCKED:` failure finishes at once, cancelling live siblings. A role
    /// of a team stage that fails in any way cancels its live siblings, and
    /// the group is decided as if they had settled cancelled.
    pub fn on_stage_settled(&mut self, settled: &StageRun, all: &[StageRun]) -> Vec<EngineAction> {
        assert!(
            settled.state.is_terminal(),
            "settled stage run {} is still live",
            settled.id
        );
        assert!(
            all.iter().all(|s| s.run == settled.run),
            "stage runs of several runs passed together"
        );
        if self.finished {
            return Vec::new();
        }
        let mut merged: Vec<StageRun> =
            all.iter().filter(|s| s.id != settled.id).cloned().collect();
        merged.push(settled.clone());
        self.act_on_runs(&merged)
    }

    /// The spawn that re-runs `failed`'s slot after an infrastructure
    /// failure (`limits.infra_retries`), identical to the original spawn.
    /// `None` when the slot was superseded or the run is already decided.
    /// Persist the failure and the new stage run together, and do not feed
    /// `failed` to [`Self::on_stage_settled`]: a failure persisted alone is
    /// final on resume.
    pub fn respawn(&self, failed: &StageRun, all: &[StageRun]) -> Option<SpawnPlan> {
        assert!(
            matches!(failed.state, StageState::Failed { .. }),
            "only a failed stage run is re-spawned, not {:?}",
            failed.state
        );
        assert!(
            all.iter().all(|s| s.run == failed.run),
            "stage runs of several runs passed together"
        );
        let final_failure =
            matches!(&failed.state, StageState::Failed { error } if StageOutcome::ends_run(error));
        // An operator blocker or an escalation is never retried automatically.
        if self.finished || final_failure {
            return None;
        }
        // Replay as if the slot were still live: the group must be the
        // current one, undecided, with no later group started, and `failed`
        // the newest run of its slot.
        let mut runs: Vec<StageRun> = all.iter().filter(|s| s.id != failed.id).cloned().collect();
        runs.push(StageRun {
            state: StageState::Running,
            ..failed.clone()
        });
        let key = (failed.stage_id.as_str(), failed.attempt, failed.fan_index);
        if latest_per_slot(&runs).get(&key).map(|s| &s.id) != Some(&failed.id) {
            return None;
        }
        // A sibling already failed the team attempt: this slot is cancelled.
        if !self.doomed_siblings(&runs).is_empty() {
            return None;
        }
        let Replay::Await { at, .. } = self.replay(&runs) else {
            return None;
        };
        let stage = &self.spec.file.stages[at.stage];
        let current = stage.id == failed.stage_id && at.attempt == failed.attempt;
        let later_started = all.iter().any(|s| {
            self.spec
                .stage_index(&s.stage_id)
                .is_some_and(|index| !at.entered.contains(&(index, s.attempt)))
        });
        if !current || later_started || failed.fan_index >= stage.fan_out {
            return None;
        }
        match self.spawns(&at, &[failed.fan_index]).pop() {
            Some(EngineAction::Spawn(plan)) => Some(plan),
            other => unreachable!("spawns() returns one Spawn per slot, got {other:?}"),
        }
    }

    /// Operator or orchestrator cancel: cancel live stages, finish cancelled.
    /// Terminal entries in `live` are ignored.
    pub fn cancel(&mut self, live: &[StageRun], reason: &str) -> Vec<EngineAction> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut actions: Vec<EngineAction> = live
            .iter()
            .filter(|s| !s.state.is_terminal())
            .map(|s| EngineAction::CancelStage {
                stage_run: s.id.clone(),
                reason: reason.to_owned(),
            })
            .collect();
        actions.push(EngineAction::Finish {
            state: RunState::Cancelled {
                reason: Some(reason.to_owned()),
            },
        });
        actions
    }

    /// Replay `runs` and turn the result into actions.
    fn act_on_runs(&mut self, runs: &[StageRun]) -> Vec<EngineAction> {
        let (siblings, replay) = self.replay_team_aware(runs);
        self.act_on(siblings, replay)
    }

    /// Turn a replay result into actions, after cancelling the team
    /// `siblings` not cancelled yet by this instance.
    fn act_on(&mut self, siblings: Vec<SiblingCancel>, replay: Replay) -> Vec<EngineAction> {
        let mut actions: Vec<EngineAction> = siblings
            .into_iter()
            .filter(|(stage_run, _)| self.cancelling.insert(stage_run.clone()))
            .map(|(stage_run, reason)| EngineAction::CancelStage { stage_run, reason })
            .collect();
        actions.extend(self.act_on_replay(replay));
        actions
    }

    fn act_on_replay(&mut self, replay: Replay) -> Vec<EngineAction> {
        match replay {
            Replay::Await { at, missing } => self.owed_spawns(&at, &missing),
            Replay::Stop { cancel, state } => {
                self.finished = true;
                let reason = format!("pipeline run finished: {}", state_word(&state));
                let mut actions: Vec<EngineAction> = cancel
                    .into_iter()
                    .map(|stage_run| EngineAction::CancelStage {
                        stage_run,
                        reason: reason.clone(),
                    })
                    .collect();
                actions.push(EngineAction::Finish { state });
                actions
            }
            Replay::AwaitOperator { stage, wait } => {
                if !self.awaited.insert((stage, wait.attempt)) {
                    return Vec::new();
                }
                vec![EngineAction::AwaitOperator(wait)]
            }
            Replay::AwaitMerge { stage, plan } => {
                if !self.merging.insert((stage, plan.attempt)) {
                    return Vec::new();
                }
                vec![EngineAction::MergeTeam(plan)]
            }
            Replay::AwaitShip { plan } => {
                if std::mem::replace(&mut self.shipping, true) {
                    return Vec::new();
                }
                vec![EngineAction::Ship(plan)]
            }
        }
    }

    /// Spawns for the `missing` slots, once per group per driver instance:
    /// a duplicate settle must not spawn a slot that is already pending.
    fn owed_spawns(&mut self, at: &Position, missing: &[u32]) -> Vec<EngineAction> {
        if missing.is_empty() || !self.emitted.insert((at.stage, at.attempt)) {
            return Vec::new();
        }
        self.spawns(at, missing)
    }

    /// [`Self::replay`] with every doomed team sibling seen as cancelled,
    /// its reason as summary: how the daemon persists it once cancelled.
    fn replay_team_aware(&self, runs: &[StageRun]) -> (Vec<SiblingCancel>, Replay) {
        let siblings = self.doomed_siblings(runs);
        if siblings.is_empty() {
            return (siblings, self.replay(runs));
        }
        let seen: Vec<StageRun> = runs
            .iter()
            .map(|run| match siblings.iter().find(|(id, _)| *id == run.id) {
                Some((_, reason)) => cancelled_copy(run, reason),
                None => run.clone(),
            })
            .collect();
        debug_assert!(
            siblings
                .iter()
                .all(|(id, _)| seen.iter().any(|s| s.id == *id && s.state.is_terminal())),
            "every doomed sibling is seen cancelled"
        );
        (siblings, self.replay(&seen))
    }

    /// The live latest runs of team groups where another role's latest run
    /// ended without completing, in slot order, each with its reason.
    fn doomed_siblings(&self, runs: &[StageRun]) -> Vec<SiblingCancel> {
        let mut groups: BTreeMap<(&str, u32), Vec<&StageRun>> = BTreeMap::new();
        for ((stage, attempt, _), run) in latest_per_slot(runs) {
            groups.entry((stage, attempt)).or_default().push(run);
        }
        let mut doomed = Vec::new();
        for ((stage_id, _), members) in groups {
            let team = self
                .spec
                .stage(stage_id)
                .filter(|s| s.kind == StageKind::Team);
            let Some(stage) = team else {
                continue;
            };
            let failed: Vec<&StageRun> = members
                .iter()
                .copied()
                .filter(|m| m.state.is_terminal() && m.state != StageState::Completed)
                .collect();
            if failed.is_empty() {
                continue;
            }
            let reason = sibling_reason(stage, &failed);
            doomed.extend(
                members
                    .iter()
                    .filter(|m| !m.state.is_terminal())
                    .map(|m| (m.id.clone(), reason.clone())),
            );
        }
        doomed
    }

    /// Re-derive the run's position from persisted stage runs alone: walk
    /// the pipeline from the first stage, deciding each finished group the
    /// same way the live driver did.
    fn replay(&self, runs: &[StageRun]) -> Replay {
        let slots = latest_per_slot(runs);
        let stages = &self.spec.file.stages;
        let mut counts = Counters {
            entries: vec![0_u32; stages.len()],
            loops: vec![0_u32; stages.len()],
            revisions: vec![0_u32; stages.len()],
        };
        let mut at = Position::first(&mut counts.entries);
        // Each pass that does not return consumes a distinct, fully persisted
        // group or an abort, or steps over one removed stage towards one.
        let passes = (slots.len() + self.aborts.len() + 1) * (stages.len() + 1);
        for _ in 0..passes {
            let stage = &stages[at.stage];
            let group = group_at(&slots, stage, at.attempt);
            if let Some(then) = self.abort_at(&stage.id, at.attempt) {
                if group.iter().flatten().any(|m| !m.state.is_terminal()) {
                    // Its agents are being stopped; no slot is spawned anew.
                    let missing = Vec::new();
                    return Replay::Await { at, missing };
                }
                if let Some(stop) = self.after_abort(&mut at, &mut counts, then) {
                    return stop;
                }
                continue;
            }
            if let Some(stop) = blocked_stop(stage, &group) {
                return stop;
            }
            let missing: Vec<u32> = (0..stage.fan_out)
                .filter(|f| group[*f as usize].is_none())
                .collect();
            let members: Vec<&StageRun> = group.into_iter().flatten().collect();
            if !missing.is_empty() || members.iter().any(|m| !m.state.is_terminal()) {
                return Replay::Await { at, missing };
            }
            if let Some(replay) = self.decide_group(&mut at, &mut counts, &members) {
                return replay;
            }
        }
        unreachable!("replay passed more groups than there are persisted slots")
    }

    /// A finished group: a pass goes on (a team's once its merge is in), a
    /// failure loops back or fails the run. `Some` ends the replay.
    fn decide_group(
        &self,
        at: &mut Position,
        counts: &mut Counters,
        members: &[&StageRun],
    ) -> Option<Replay> {
        let stage = &self.spec.file.stages[at.stage];
        let owed = self.spec.owed(stage);
        let (headline, report) = match gate::evaluate(stage, at.attempt, members, owed) {
            GroupResult::Fail { headline, report } => (headline, report),
            GroupResult::Pass { summary } if stage.kind != StageKind::Team => {
                if owed.ship_record {
                    at.record = Some(passed_record(members, owed.review));
                }
                return self.after_pass(at, counts, summary);
            }
            GroupResult::Pass { summary } => {
                let plan = self.merge_plan(stage, at.attempt);
                match self.merges.get(&(stage.id.clone(), at.attempt)) {
                    None => {
                        let stage = at.stage;
                        return Some(Replay::AwaitMerge { stage, plan });
                    }
                    Some(MergeOutcome::Merged) => return self.after_pass(at, counts, summary),
                    Some(MergeOutcome::Conflict { role, files }) => (
                        format!(
                            "merging role `{role}` conflicted on {} file(s)",
                            files.len()
                        ),
                        team::conflict_report(&plan, role, files),
                    ),
                    Some(MergeOutcome::Refused { role, reason }) => (
                        format!("role `{role}`'s work could not be merged: {reason}"),
                        team::refusal_report(&plan, role, reason),
                    ),
                }
            }
        };
        match self.loop_target(at.stage, &mut counts.loops) {
            Some(target) => {
                at.enter(target, &mut counts.entries, report);
                None
            }
            None => Some(self.failed_stop(at.stage, &counts.loops, &headline, &report)),
        }
    }

    /// The abort recorded for this attempt of the stage, or the standing
    /// skip of a stage that was removed before it started.
    fn abort_at(&self, stage_id: &str, attempt: u32) -> Option<&Then> {
        let exact = self.aborts.get(&(stage_id.to_owned(), attempt));
        exact.or_else(|| self.aborts.get(&(stage_id.to_owned(), 0)))
    }

    /// An aborted group is never judged: the run does what the steer said,
    /// a step back with a fresh fix-loop budget. `Some` ends the replay.
    fn after_abort(&self, at: &mut Position, counts: &mut Counters, then: &Then) -> Option<Replay> {
        let stages = &self.spec.file.stages;
        let from = at.stage;
        let target = match then {
            Then::Retry => Some(from),
            Then::Skip => Some(from + 1).filter(|next| *next < stages.len()),
            Then::Goto { to } => self.spec.stage_index(to),
        };
        let Some(target) = target else {
            // The stage it named was edited away after the abort was recorded.
            let error = format!(
                "stage `{}` was aborted by a steer, and the stage to continue with no longer exists",
                stages[from].id
            );
            return Some(Replay::Stop {
                cancel: Vec::new(),
                state: RunState::Failed {
                    stage_id: stages[from].id.clone(),
                    error,
                },
            });
        };
        assert!(
            target < stages.len(),
            "the run goes on at one of its stages"
        );
        for passed_over in &stages[from..target.max(from)] {
            at.previous
                .entry(template::previous_variable(&passed_over.id))
                .or_insert_with(|| sections::SKIPPED_SUMMARY.to_owned());
        }
        if target < from {
            counts.loops[target..].fill(0);
        }
        let feedback = match then {
            Then::Retry => at.feedback.clone(),
            Then::Skip | Then::Goto { .. } => String::new(),
        };
        at.enter(target, &mut counts.entries, feedback);
        None
    }

    /// A passed group: wait for or apply an operator decision on a gate,
    /// then enter the next stage. `Some` ends the replay.
    fn after_pass(
        &self,
        at: &mut Position,
        counts: &mut Counters,
        summary: String,
    ) -> Option<Replay> {
        let stage = &self.spec.file.stages[at.stage];
        assert!(at.attempt >= 1, "attempts are 1-based");
        if let Some(subject) = self.gates.subject(&stage.id) {
            match self.decisions.get(&(stage.id.clone(), at.attempt)) {
                None => {
                    let wait = OperatorWait {
                        stage_id: stage.id.clone(),
                        attempt: at.attempt,
                        subject,
                        summary,
                    };
                    return Some(Replay::AwaitOperator {
                        stage: at.stage,
                        wait,
                    });
                }
                Some(GateDecision::Reject { reason }) => {
                    return Some(rejected_stop(stage, subject, reason));
                }
                Some(GateDecision::Revise { feedback }) => {
                    return self.revise(at, counts, subject, feedback);
                }
                Some(GateDecision::Approve { note }) => {
                    at.approved.insert(stage.id.clone(), note.clone());
                }
            }
        }
        if at.stage + 1 == self.spec.file.stages.len() {
            return Some(self.after_last_pass(at, summary));
        }
        at.previous
            .insert(template::previous_variable(&stage.id), summary);
        at.enter(at.stage + 1, &mut counts.entries, String::new());
        None
    }

    /// Every stage passed: the run succeeded, once archond delivered it where
    /// the pipeline leaves shipping to archond.
    fn after_last_pass(&self, at: &Position, summary: String) -> Replay {
        let stop = |state| Replay::Stop {
            cancel: Vec::new(),
            state,
        };
        let Some(record_stage) = self.spec.ship_record_stage() else {
            return stop(RunState::Succeeded { summary });
        };
        // A steer's goto can pass the record stage by: then nothing is proved.
        if at.record.is_none() {
            return stop(RunState::Failed {
                stage_id: SHIP_STEP.to_owned(),
                error: format!(
                    "stage `{}`, which writes the ship record, never passed in this run, so \
                     there is nothing Archon can ship",
                    record_stage.id
                ),
            });
        }
        match &self.shipped {
            None => Replay::AwaitShip {
                plan: self.ship_plan(at, record_stage, summary),
            },
            Some(ShipOutcome::Failed { error }) => stop(RunState::Failed {
                stage_id: SHIP_STEP.to_owned(),
                error: gate::clip(error, MAX_ERROR_CHARS),
            }),
            Some(shipped) => {
                let line = shipped.line().expect("a delivery that shipped says where");
                stop(RunState::Succeeded {
                    summary: format!("{summary}\n\n{line}"),
                })
            }
        }
    }

    /// The delivery of the run at `at`, its last stage, which just passed
    /// with `summary`.
    fn ship_plan(&self, at: &Position, record_stage: &StageSpec, summary: String) -> ShipPlan {
        let (record, review) = at
            .record
            .clone()
            .expect("the caller checked that the record stage passed");
        let stages = &self.spec.file.stages;
        let passed = |stage: &StageSpec| match stage.id == stages[at.stage].id {
            true => Some(summary.as_str()),
            false => at
                .previous
                .get(&template::previous_variable(&stage.id))
                .map(String::as_str),
        };
        let report_stage = autonomy::change_report_stage(&self.spec);
        let is_report = |stage: &StageSpec| report_stage.is_some_and(|r| r.id == stage.id);
        let recorded_at = self.spec.stage_index(&record_stage.id).expect("a stage");
        let later: Vec<(&str, &str)> = stages[recorded_at + 1..]
            .iter()
            .filter(|stage| !is_report(stage))
            .filter_map(|stage| Some((stage.id.as_str(), passed(stage)?)))
            .collect();
        let added = BodyAdditions {
            review: review.as_deref(),
            later: &later,
            report: report_stage.and_then(passed),
        };
        let body = ship::pr_body(&record, added, &self.base_vars["item.id"], self.attribution);
        ShipPlan {
            record_stage: record_stage.id.clone(),
            record,
            body,
            summary,
            attribution: self.attribution,
        }
    }

    /// Loop back from a gate with the operator's feedback: a proposal is
    /// rewritten, a change report sends the work back to the first work
    /// stage. Every stage the revision re-runs starts a fresh fix-loop
    /// budget. Fails the run past [`MAX_REVISIONS`].
    fn revise(
        &self,
        at: &mut Position,
        counts: &mut Counters,
        subject: GateSubject,
        feedback: &str,
    ) -> Option<Replay> {
        let stages = &self.spec.file.stages;
        let stage = &stages[at.stage];
        if counts.revisions[at.stage] >= MAX_REVISIONS {
            let error = format!(
                "stage `{}`: the operator asked for another revision of the {}, but {MAX_REVISIONS} is the limit; retry the item to start over",
                stage.id,
                subject.noun()
            );
            return Some(Replay::Stop {
                cancel: Vec::new(),
                state: RunState::Failed {
                    stage_id: stage.id.clone(),
                    error,
                },
            });
        }
        counts.revisions[at.stage] += 1;
        let target = match subject {
            GateSubject::Proposal => at.stage,
            GateSubject::Report => stages[..at.stage]
                .iter()
                .position(|s| matches!(s.kind, StageKind::Work | StageKind::Team))
                .unwrap_or(at.stage),
        };
        assert!(target <= at.stage, "a revision never skips ahead");
        counts.loops[target..].fill(0);
        let feedback = sections::operator_feedback(subject, feedback);
        at.enter(target, &mut counts.entries, feedback);
        None
    }

    /// The `on_fail.goto` stage when a loop is still allowed; counts it.
    fn loop_target(&self, stage: usize, loops: &mut [u32]) -> Option<usize> {
        let on_fail = self.spec.file.stages[stage].on_fail.as_ref()?;
        if loops[stage] >= on_fail.max_loops {
            return None;
        }
        loops[stage] += 1;
        let target = self
            .spec
            .stage_index(&on_fail.goto)
            .expect("validated: on_fail.goto names a stage");
        assert!(
            target < stage,
            "validated: on_fail.goto names an earlier stage"
        );
        Some(target)
    }

    fn failed_stop(&self, stage: usize, loops: &[u32], headline: &str, report: &str) -> Replay {
        let spec = &self.spec.file.stages[stage];
        let exhausted = match &spec.on_fail {
            Some(on_fail) => format!(
                " after {} of {} fix loop(s)",
                loops[stage], on_fail.max_loops
            ),
            None => String::new(),
        };
        let error = format!(
            "stage `{}` failed{exhausted}: {headline}\n\n{report}",
            spec.id
        );
        Replay::Stop {
            cancel: Vec::new(),
            state: RunState::Failed {
                stage_id: spec.id.clone(),
                error: gate::clip(&error, MAX_ERROR_CHARS),
            },
        }
    }

    fn spawns(&self, at: &Position, fans: &[u32]) -> Vec<EngineAction> {
        let stage = &self.spec.file.stages[at.stage];
        let choices = self
            .plan
            .choices_for(&stage.id)
            .filter(|c| !c.is_empty())
            .expect("RunDriver::new checked the plan names every stage");
        let template = &self.spec.prompts[&stage.prompt];
        let mode = self.spec.file.pipeline.workspace;
        let context =
            sections::context(&self.spec, stage, &at.feedback, &at.previous, &at.approved);
        let keep = self.keeps_agent(&stage.id);
        let owed = self.spec.owed(stage);
        let resume = (keep && at.attempt > 1 && !at.feedback.trim().is_empty())
            .then(|| sections::resume_message(stage, at.attempt, &at.feedback, owed));
        // The daemon advises within the policy current when it spawns.
        let reviewers = owed
            .review
            .is_some_and(|duty| duty.reviewers > 0)
            .then(|| self.plan.reviewer_choices().unwrap_or_default().to_vec());
        fans.iter()
            .map(|&fan| {
                assert!(fan < stage.fan_out, "slot {fan} outside fan_out");
                let team = self.team_slot(stage, at.attempt, fan);
                let vars = self.spawn_vars(stage, at, fan, team.as_ref());
                let mut prompt = template::render(template, &vars)
                    .expect("RunDriver::new test-rendered every prompt");
                prompt.push_str(&context);
                if let Some(extra) = team.as_ref().and_then(|t| stage.role_prompts.get(&t.role)) {
                    prompt.push_str(&format!("\n\n## Your role\n\n{}\n", extra.trim()));
                }
                let part = self.team_part(team.as_ref());
                let workspace = &vars["workspace"];
                let experiments = self.experiments.as_deref();
                let instructions =
                    contract::instructions(stage, mode, workspace, part, owed, experiments);
                EngineAction::Spawn(SpawnPlan {
                    stage_id: stage.id.clone(),
                    kind: stage.kind,
                    attempt: at.attempt,
                    fan_index: fan,
                    fan_count: stage.fan_out,
                    choice: choices[fan as usize % choices.len()].clone(),
                    prompt,
                    instructions,
                    timeout_minutes: stage.timeout_minutes,
                    // The run's reviewers are agents the stage delegates to.
                    allow_delegation: stage.allow_delegation || reviewers.is_some(),
                    workspace: mode,
                    team,
                    keep,
                    resume: resume.clone(),
                    reviewers: reviewers.clone(),
                })
            })
            .collect()
    }

    /// Base variables plus the per-spawn ones. `fan.index` is 1-based for
    /// prompts ("reviewer 2 of 3"); `SpawnPlan::fan_index` stays 0-based.
    fn spawn_vars(
        &self,
        stage: &StageSpec,
        at: &Position,
        fan: u32,
        team: Option<&TeamSlot>,
    ) -> BTreeMap<String, String> {
        let mut vars = self.base_vars.clone();
        vars.extend(at.previous.iter().map(|(k, v)| (k.clone(), v.clone())));
        let per_spawn = [
            ("stage.id", stage.id.clone()),
            ("attempt", at.attempt.to_string()),
            ("fan.index", (fan + 1).to_string()),
            ("fan.count", stage.fan_out.to_string()),
            ("feedback", at.feedback.clone()),
        ];
        vars.extend(per_spawn.into_iter().map(|(k, v)| (k.to_owned(), v)));
        if let Some(slot) = team {
            let plan = self.merge_plan(stage, at.attempt);
            let branches: Vec<&str> = plan.roles.iter().map(|r| r.branch.as_str()).collect();
            let per_slot = [
                ("workspace", path_text(&slot.workspace)),
                ("team.role", slot.role.clone()),
                ("team.roles", stage.roles.join(", ")),
                ("team.branches", branches.join(", ")),
            ];
            vars.extend(per_slot.into_iter().map(|(k, v)| (k.to_owned(), v)));
        }
        vars
    }

    /// Slot `fan` of a team stage: its role, its branch for `attempt`, and
    /// its worktree beside the run's. `None` for every other kind.
    fn team_slot(&self, stage: &StageSpec, attempt: u32, fan: u32) -> Option<TeamSlot> {
        if stage.kind != StageKind::Team {
            return None;
        }
        let role = stage
            .roles
            .get(fan as usize)
            .expect("validated: one role per slot");
        let root = team::team_root(Path::new(&self.base_vars["workspace"]))
            .expect("RunDriver::new checked the team layout");
        let slot = TeamSlot {
            role: role.clone(),
            branch: team::worker_branch(&self.base_vars["branch"], &self.run_id(), role, attempt),
            workspace: root.join(team::worker_dir(&stage.id, role)),
        };
        debug_assert_ne!(PathBuf::from(&self.base_vars["workspace"]), slot.workspace);
        Some(slot)
    }

    /// How a spawn relates to the pipeline's team stage, for its contract.
    fn team_part<'a>(&'a self, slot: Option<&'a TeamSlot>) -> contract::TeamPart<'a> {
        let has_team = self
            .spec
            .file
            .stages
            .iter()
            .any(|s| s.kind == StageKind::Team);
        match slot {
            Some(slot) => contract::TeamPart::Role(slot),
            None if has_team => contract::TeamPart::RunTree {
                branch: &self.base_vars["branch"],
            },
            None => contract::TeamPart::Solo,
        }
    }

    fn run_id(&self) -> PipelineRunId {
        PipelineRunId::parse(&self.base_vars["run.id"]).expect("RunDriver::new checked the run id")
    }

    /// The merge of every role's branch of `attempt`, in roles order.
    fn merge_plan(&self, stage: &StageSpec, attempt: u32) -> MergePlan {
        assert_eq!(stage.kind, StageKind::Team, "only team stages merge");
        let into = self.base_vars["branch"].clone();
        let run = self.run_id();
        let roles = stage
            .roles
            .iter()
            .map(|role| RoleBranch {
                role: role.clone(),
                branch: team::worker_branch(&into, &run, role, attempt),
            })
            .collect();
        MergePlan {
            stage_id: stage.id.clone(),
            attempt,
            into,
            roles,
        }
    }

    /// Render every stage prompt once with placeholder values, so a spawn
    /// can never fail to render later.
    fn check_prompts_render(&self) -> Result<(), ArchonError> {
        let mut at = Position {
            stage: 0,
            attempt: 1,
            feedback: String::new(),
            previous: BTreeMap::new(),
            entered: BTreeSet::new(),
            approved: BTreeMap::new(),
            record: None,
        };
        for (index, stage) in self.spec.file.stages.iter().enumerate() {
            at.stage = index;
            let template = self.spec.prompts.get(&stage.prompt).ok_or_else(|| {
                ArchonError::Pipeline(format!(
                    "stage {:?}: prompt {:?} not loaded",
                    stage.id, stage.prompt
                ))
            })?;
            let team = self.team_slot(stage, 1, 0);
            template::render(template, &self.spawn_vars(stage, &at, 0, team.as_ref()))
                .map_err(|e| ArchonError::Pipeline(format!("stage {:?}: {e}", stage.id)))?;
            at.previous
                .insert(template::previous_variable(&stage.id), String::new());
        }
        Ok(())
    }
}

fn check_plan(spec: &PipelineSpec, plan: &ModelPlan) -> Result<(), ArchonError> {
    for stage in &spec.file.stages {
        let choices = plan.choices_for(&stage.id).unwrap_or_default();
        if choices.is_empty() {
            return Err(ArchonError::Invalid(format!(
                "model_plan names no model for stage {:?} of pipeline {:?}; every stage needs at least one",
                stage.id,
                spec.name()
            )));
        }
        if choices.len() > stage.fan_out as usize {
            return Err(ArchonError::Invalid(format!(
                "model_plan lists {} choices for stage {:?}, which runs {} agent(s)",
                choices.len(),
                stage.id,
                stage.fan_out
            )));
        }
    }
    spec.check_plan_keys(plan)
}

fn check_base_vars(base_vars: &BTreeMap<String, String>) -> Result<(), ArchonError> {
    if let Some(missing) = template::RUN_VARIABLES
        .iter()
        .find(|k| !base_vars.contains_key(**k))
    {
        return Err(ArchonError::Invalid(format!(
            "run variable {missing:?} is missing"
        )));
    }
    if let Some(extra) = base_vars
        .keys()
        .find(|k| !template::RUN_VARIABLES.contains(&k.as_str()))
    {
        return Err(ArchonError::Invalid(format!(
            "{extra:?} is not a run variable; the driver sets per-spawn values itself"
        )));
    }
    if base_vars["workspace"].is_empty() {
        return Err(ArchonError::Invalid(
            "run variable \"workspace\" is empty".into(),
        ));
    }
    Ok(())
}

/// A pipeline with a team stage runs in `<team root>/main`, on a branch the
/// worker branches can extend.
fn check_team_layout(
    spec: &PipelineSpec,
    base_vars: &BTreeMap<String, String>,
) -> Result<(), ArchonError> {
    if !spec.file.stages.iter().any(|s| s.kind == StageKind::Team) {
        return Ok(());
    }
    let workspace = Path::new(&base_vars["workspace"]);
    if team::team_root(workspace).is_none() {
        return Err(ArchonError::Invalid(format!(
            "pipeline {:?} has a team stage, so its run workspace must be <item>/{}, not {}",
            spec.name(),
            team::RUN_TREE_DIR,
            workspace.display()
        )));
    }
    if base_vars["branch"].is_empty() {
        return Err(ArchonError::Invalid(
            "a team pipeline needs a run branch for its workers to fork".into(),
        ));
    }
    PipelineRunId::parse(&base_vars["run.id"])?;
    Ok(())
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

/// The newest stage run per `(stage, attempt, slot)`: an infra retry
/// re-spawns a slot, superseding the failed run. Newest is by start time,
/// then id.
fn latest_per_slot(runs: &[StageRun]) -> BTreeMap<SlotKey<'_>, &StageRun> {
    let mut slots: BTreeMap<SlotKey<'_>, &StageRun> = BTreeMap::new();
    for run in runs {
        let key = (run.stage_id.as_str(), run.attempt, run.fan_index);
        let newer = slots
            .get(&key)
            .is_none_or(|seen| (run.started_at, &run.id) > (seen.started_at, &seen.id));
        if newer {
            slots.insert(key, run);
        }
    }
    slots
}

/// The latest stage run of each slot of `stage` at `attempt`.
fn group_at<'a>(
    slots: &BTreeMap<SlotKey<'_>, &'a StageRun>,
    stage: &StageSpec,
    attempt: u32,
) -> Vec<Option<&'a StageRun>> {
    (0..stage.fan_out)
        .map(|fan| slots.get(&(stage.id.as_str(), attempt, fan)).copied())
        .collect()
}

/// The record stage's passed member's ship record, with the line Archon
/// wrote for the review it holds as `duty`.
fn passed_record(members: &[&StageRun], duty: Option<ReviewDuty>) -> (ShipRecord, Option<String>) {
    let [member] = members else {
        panic!("validated: one agent writes the ship record")
    };
    let structured = member.structured.as_ref();
    let record = ShipRecord::read(structured).expect("the gate passed only with the record");
    let review = duty.map(|duty| {
        let held = review::Evidence::read(structured, duty);
        held.expect("the gate passed only with the review").line()
    });
    (record, review)
}

/// A `BLOCKED:` or `ESCALATE:` failure anywhere in the group ends the run at
/// once: no loop, and live siblings are cancelled.
fn blocked_stop(stage: &StageSpec, group: &[Option<&StageRun>]) -> Option<Replay> {
    let error = group.iter().flatten().find_map(|m| match &m.state {
        StageState::Failed { error } if StageOutcome::ends_run(error) => {
            Some(error.trim().to_owned())
        }
        _ => None,
    })?;
    let cancel = group
        .iter()
        .flatten()
        .filter(|m| !m.state.is_terminal())
        .map(|m| m.id.clone())
        .collect();
    Some(Replay::Stop {
        cancel,
        state: RunState::Failed {
            stage_id: stage.id.clone(),
            error: gate::clip(&error, MAX_ERROR_CHARS),
        },
    })
}

/// Why a live team role is cancelled: the roles that already failed.
fn sibling_reason(stage: &StageSpec, failed: &[&StageRun]) -> String {
    assert_eq!(stage.kind, StageKind::Team, "only team roles are siblings");
    assert!(
        !failed.is_empty(),
        "a sibling is cancelled for a failed role"
    );
    let roles: Vec<String> = failed
        .iter()
        .map(|m| gate::role_outcome(stage, m))
        .collect();
    format!(
        "cancelled because {} in team stage `{}` attempt {}",
        roles.join(" and "),
        stage.id,
        failed[0].attempt
    )
}

/// `run` as the daemon persists it once cancelled for `reason`.
fn cancelled_copy(run: &StageRun, reason: &str) -> StageRun {
    assert!(!run.state.is_terminal(), "only a live run is cancelled");
    StageRun {
        state: StageState::Cancelled,
        summary: Some(reason.to_owned()),
        ..run.clone()
    }
}

/// The operator rejected a gate: the item is cancelled, not failed.
fn rejected_stop(stage: &StageSpec, subject: GateSubject, reason: &str) -> Replay {
    let reason = format!(
        "the operator rejected the {} at stage `{}`: {}",
        subject.noun(),
        stage.id,
        reason.trim()
    );
    Replay::Stop {
        cancel: Vec::new(),
        state: RunState::Cancelled {
            reason: Some(reason),
        },
    }
}

fn state_word(state: &RunState) -> &'static str {
    match state {
        RunState::Running { .. } => "running",
        RunState::AwaitingOperator { .. } => "awaiting the operator",
        RunState::Succeeded { .. } => "succeeded",
        RunState::Failed { .. } => "failed",
        RunState::Cancelled { .. } => "cancelled",
    }
}
