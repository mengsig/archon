//! A stand-in for the daemon around one `RunDriver`: it persists every
//! action the way the executor does and, at every step, checks the resume
//! property: a driver rebuilt from the persisted stage runs alone must owe
//! exactly the actions the uninterrupted driver returned.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use archon_core::Timestamp;
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::model::ModelPlan;
use archon_core::run::{GateDecision, PipelineRun, RunState, StageRun, StageState, Verdict};
use archon_core::steer::Then;
use archon_pipeline::autonomy::OperatorGates;
use archon_pipeline::spec::PipelineSpec;
use archon_pipeline::team::{MergeOutcome, MergePlan};
use archon_pipeline::{EngineAction, OperatorWait, RunDriver, ShipOutcome, ShipPlan, SpawnPlan};

use crate::support;

/// How a stage agent ends.
#[derive(Debug, Clone)]
pub enum Agent {
    /// Completed with this summary and no verdict (work, ship, report).
    Done(&'static str),
    /// Completed with a summary and structured data, without a verdict.
    DoneStructured(&'static str, serde_json::Value),
    /// Completed with a structured `pass`.
    Pass,
    /// Completed with a structured `fail` carrying one finding titled so.
    Fail(&'static str),
    /// Completed with only a summary `VERDICT:` line, verdict not persisted.
    SummaryVerdict(&'static str),
    /// Completed without any verdict.
    NoVerdict,
    /// `fail_task` with this error.
    Error(&'static str),
    /// `fail_task` with `BLOCKED: <text>`.
    Blocked(&'static str),
    TimedOut,
    Cancelled,
}

#[derive(Debug)]
pub struct Sim {
    pub spec: Arc<PipelineSpec>,
    pub plan: ModelPlan,
    pub base: BTreeMap<String, String>,
    pub driver: RunDriver,
    pub run: PipelineRun,
    pub stages: Vec<StageRun>,
    /// Spawns emitted but not yet admitted (the daemon's pending list).
    pub pending: Vec<SpawnPlan>,
    /// The plan each persisted stage run was spawned from.
    pub plans: BTreeMap<StageRunId, SpawnPlan>,
    pub gates: OperatorGates,
    /// Persisted operator decisions, oldest first.
    pub decisions: Vec<(String, u32, GateDecision)>,
    /// The gate the run waits on, as the daemon's gate row records it.
    pub open_gate: Option<OperatorWait>,
    /// Persisted team merge outcomes, oldest first.
    pub merges: Vec<(String, u32, MergeOutcome)>,
    /// The team merge the run waits on, as the daemon's table holds it.
    pub open_merge: Option<MergePlan>,
    /// How archond's delivery ended, as the daemon persists it.
    pub shipped: Option<ShipOutcome>,
    /// The delivery the run waits on, as the daemon's table holds it.
    pub open_ship: Option<ShipPlan>,
    /// Whether the record stage's agent writes the ship record its contract
    /// asks for; a test of a missing record turns it off.
    pub writes_record: bool,
    /// Persisted steered aborts, oldest first.
    pub aborts: Vec<(String, u32, Then)>,
    clock: Timestamp,
}

impl Sim {
    pub fn new(spec: Arc<PipelineSpec>, plan: ModelPlan) -> Self {
        Self::with_gates(spec, plan, OperatorGates::default())
    }

    pub fn with_gates(spec: Arc<PipelineSpec>, plan: ModelPlan, gates: OperatorGates) -> Self {
        let run_id = PipelineRunId::new();
        let item = QueueItemId::new();
        let base = support::base_vars_for(&spec, &run_id, &item);
        let driver = RunDriver::new(spec.clone(), plan.clone(), base.clone())
            .and_then(|d| d.with_gates(gates.clone()))
            .expect("valid driver");
        let clock = archon_core::now();
        let run = PipelineRun {
            id: run_id,
            item,
            pipeline: spec.name().to_owned(),
            shipped: false,
            aether_run: None,
            workspace: base["workspace"].clone().into(),
            team_root: None,
            branch: "archon/w1-fix-the-widget".into(),
            state: RunState::Running {
                stage_id: spec.file.stages[0].id.clone(),
                attempt: 1,
            },
            cost_usd: 0.0,
            started_at: clock,
            finished_at: None,
        };
        Self {
            spec,
            plan,
            base,
            driver,
            run,
            stages: Vec::new(),
            pending: Vec::new(),
            plans: BTreeMap::new(),
            gates,
            decisions: Vec::new(),
            open_gate: None,
            merges: Vec::new(),
            open_merge: None,
            shipped: None,
            open_ship: None,
            writes_record: true,
            aborts: Vec::new(),
            clock,
        }
    }

    /// Start the run and admit every spawn.
    pub fn start(&mut self) -> Vec<EngineAction> {
        let owed = self.resume_fresh();
        let actions = self.driver.start();
        assert_eq!(owed, actions, "resume of an empty run must equal start()");
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Start the run but admit only `admit` of its spawns.
    pub fn start_partial(&mut self, admit: usize) -> Vec<EngineAction> {
        let actions = self.driver.start();
        self.apply(&actions, admit);
        actions
    }

    /// Settle the live agent of `stage`/`fan`, feed the driver, check the
    /// resume property and idempotency, then persist the actions.
    pub fn settle(&mut self, stage: &str, fan: u32, agent: Agent) -> Vec<EngineAction> {
        self.settle_admitting(stage, fan, agent, usize::MAX)
    }

    pub fn settle_admitting(
        &mut self,
        stage: &str,
        fan: u32,
        agent: Agent,
        admit: usize,
    ) -> Vec<EngineAction> {
        let index = self.live_index(stage, fan);
        let at = self.tick();
        apply_agent(&mut self.stages[index], &agent, at);
        let records = self.spec.ship_record_stage().is_some_and(|s| s.id == stage);
        if records && self.writes_record {
            write_ship_record(&mut self.stages[index]);
        }
        let settled = self.stages[index].clone();
        let owed = self.resume_fresh();
        let actions = self.driver.on_stage_settled(&settled, &self.stages);
        assert_eq!(
            owed,
            self.expected_owed(&actions),
            "resumed driver disagrees after settling {stage}#{fan} with {agent:?}"
        );
        let again = self.driver.on_stage_settled(&settled, &self.stages);
        assert!(
            again.is_empty(),
            "a duplicate settle must be a no-op, got {again:?}"
        );
        self.apply(&actions, admit);
        actions
    }

    /// Decide the open gate the way the daemon does: persist the decision,
    /// feed the driver, and check the resume property and idempotency.
    pub fn decide(&mut self, decision: GateDecision) -> Vec<EngineAction> {
        let wait = self.open_gate.take().expect("a gate is open");
        self.decisions
            .push((wait.stage_id.clone(), wait.attempt, decision.clone()));
        let owed = self.resume_fresh();
        let actions = self
            .driver
            .on_operator_decision(&wait.stage_id, wait.attempt, decision.clone(), &self.stages)
            .expect("an open gate accepts a decision");
        assert_eq!(
            owed,
            self.expected_owed(&actions),
            "resumed driver disagrees after deciding {wait:?} with {decision:?}"
        );
        let again = self
            .driver
            .on_operator_decision(&wait.stage_id, wait.attempt, decision, &self.stages)
            .expect("the same decision twice is accepted");
        assert!(again.is_empty(), "a duplicate decision must be a no-op");
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Finish the open team merge the way the daemon does: persist the
    /// outcome, feed the driver, and check the resume property and
    /// idempotency.
    pub fn merge(&mut self, outcome: MergeOutcome) -> Vec<EngineAction> {
        let plan = self.open_merge.take().expect("a merge is open");
        self.merges
            .push((plan.stage_id.clone(), plan.attempt, outcome.clone()));
        let owed = self.resume_fresh();
        let actions = self
            .driver
            .on_team_merged(&plan.stage_id, plan.attempt, outcome.clone(), &self.stages)
            .expect("an open merge accepts its outcome");
        assert_eq!(
            owed,
            self.expected_owed(&actions),
            "resumed driver disagrees after merging {plan:?} with {outcome:?}"
        );
        let again = self
            .driver
            .on_team_merged(&plan.stage_id, plan.attempt, outcome, &self.stages)
            .expect("the same outcome twice is accepted");
        assert!(
            again.is_empty(),
            "a duplicate merge outcome must be a no-op"
        );
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Finish the open delivery as the daemon does: persist the outcome, feed
    /// the driver, and check resume and idempotency.
    pub fn ship(&mut self, outcome: ShipOutcome) -> Vec<EngineAction> {
        let plan = self.open_ship.take().expect("a delivery is open");
        assert!(self.shipped.is_none(), "a run is delivered once");
        self.shipped = Some(outcome.clone());
        let owed = self.resume_fresh();
        let actions = self
            .driver
            .on_shipped(outcome.clone(), &self.stages)
            .expect("an open delivery accepts its outcome");
        assert_eq!(
            owed,
            self.expected_owed(&actions),
            "resumed driver disagrees after shipping {plan:?} with {outcome:?}"
        );
        let again = self
            .driver
            .on_shipped(outcome, &self.stages)
            .expect("the same outcome twice is accepted");
        assert!(again.is_empty(), "a duplicate delivery must be a no-op");
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Deliver the open plan as a pull request for the commit it records.
    pub fn ship_pr(&mut self, url: &str) -> Vec<EngineAction> {
        let plan = self.open_ship.as_ref().expect("a delivery is open");
        let outcome = ShipOutcome::Shipped {
            commit: plan.record.commit.clone(),
            pr_url: Some(url.to_owned()),
            updated: false,
            risk: None,
        };
        self.ship(outcome)
    }

    /// Cancel the run the way the daemon does for an operator request.
    pub fn cancel(&mut self, reason: &str) -> Vec<EngineAction> {
        let live: Vec<StageRun> = self
            .stages
            .iter()
            .filter(|s| !s.state.is_terminal())
            .cloned()
            .collect();
        let actions = self.driver.cancel(&live, reason);
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Abort `attempt` of `stage` as the daemon does for a steer: persist it,
    /// stop the group's agents, act on what follows. Attempt 0 removes it.
    pub fn abort(&mut self, stage: &str, attempt: u32, then: Then) -> Vec<EngineAction> {
        self.driver
            .record_abort(stage, attempt, then.clone())
            .expect("the pipeline allows the abort");
        self.aborts.push((stage.to_owned(), attempt, then));
        self.pending
            .retain(|p| p.stage_id != stage || p.attempt != attempt);
        let at = self.tick();
        let mut actions = Vec::new();
        for index in 0..self.stages.len() {
            let member = &mut self.stages[index];
            let aborted = member.stage_id == stage && member.attempt == attempt;
            if !aborted || member.state.is_terminal() {
                continue;
            }
            apply_agent(member, &Agent::Cancelled, at);
            let settled = member.clone();
            actions.extend(self.driver.on_stage_settled(&settled, &self.stages));
        }
        actions.extend(self.driver.reconsider(&self.stages));
        assert_eq!(
            self.resume_fresh(),
            self.expected_owed(&actions),
            "resumed driver disagrees after aborting {stage} attempt {attempt}"
        );
        assert!(self.driver.reconsider(&self.stages).is_empty());
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Continue on a reshaped pipeline the way the daemon does: a driver
    /// rebuilt from persisted state takes over.
    pub fn reshape(&mut self, spec: PipelineSpec, plan: ModelPlan) -> Vec<EngineAction> {
        assert_eq!(spec.name(), self.spec.name(), "a run keeps its snapshot");
        self.spec = Arc::new(spec);
        self.plan = plan;
        self.pending.clear();
        self.driver = self.new_driver();
        let actions = self.driver.resume(&self.run, &self.stages);
        self.apply(&actions, usize::MAX);
        actions
    }

    /// Admit up to `count` pending spawns (the dispatcher found capacity);
    /// a team group's spawns all at once or none, like the dispatcher.
    pub fn admit(&mut self, count: usize) {
        let take = gang_admit(&self.pending, count).min(self.pending.len());
        let admitted: Vec<SpawnPlan> = self.pending.drain(..take).collect();
        for plan in admitted {
            self.persist_spawn(&plan);
        }
    }

    /// A driver rebuilt from persisted state only, and what it owes.
    pub fn resume_fresh(&self) -> Vec<EngineAction> {
        self.new_driver().resume(&self.run, &self.stages)
    }

    /// A driver rebuilt from persisted state only, as after a restart.
    pub fn resumed_driver(&self) -> RunDriver {
        let mut fresh = self.new_driver();
        fresh.resume(&self.run, &self.stages);
        fresh
    }

    fn new_driver(&self) -> RunDriver {
        let mut driver = RunDriver::new(self.spec.clone(), self.plan.clone(), self.base.clone())
            .and_then(|d| d.with_gates(self.gates.clone()))
            .expect("valid driver");
        for (stage, attempt, decision) in &self.decisions {
            driver
                .record_decision(stage, *attempt, decision.clone())
                .expect("persisted decisions replay");
        }
        for (stage, attempt, outcome) in &self.merges {
            driver
                .record_merge(stage, *attempt, outcome.clone())
                .expect("persisted merge outcomes replay");
        }
        for (stage, attempt, then) in &self.aborts {
            driver
                .record_abort(stage, *attempt, then.clone())
                .expect("persisted aborts replay");
        }
        if let Some(outcome) = &self.shipped {
            driver
                .record_ship(outcome.clone())
                .expect("a persisted delivery replays");
        }
        driver
    }

    pub fn live(&self) -> Vec<&StageRun> {
        self.stages
            .iter()
            .filter(|s| !s.state.is_terminal())
            .collect()
    }

    pub fn finished(&self) -> Option<&RunState> {
        self.run.state.is_terminal().then_some(&self.run.state)
    }

    /// The daemon's infra retry: fail the live slot and persist the retry
    /// `RunDriver::respawn` returns, without settling the driver. The retry
    /// must repeat the original spawn exactly, live and after a resume.
    pub fn infra_respawn(&mut self, stage: &str, fan: u32) -> SpawnPlan {
        let index = self.live_index(stage, fan);
        // Restarted while the agent was live, before the failure arrived.
        let resumed = self.resumed_driver();
        let at = self.tick();
        let old = &mut self.stages[index];
        old.state = StageState::Failed {
            error: "engine died".into(),
        };
        old.finished_at = Some(at);
        let failed = old.clone();
        let plan = self
            .driver
            .respawn(&failed, &self.stages)
            .expect("the failed slot belongs to the current group");
        assert_eq!(plan, self.plans[&failed.id], "a retry repeats its spawn");
        assert_eq!(
            resumed.respawn(&failed, &self.stages),
            Some(plan.clone()),
            "a resumed driver re-spawns the same slot"
        );
        self.push_stage_run(&plan);
        assert_eq!(
            self.driver.respawn(&failed, &self.stages),
            None,
            "a superseded stage run is not re-spawned twice"
        );
        assert_eq!(self.resume_fresh(), self.pending_actions());
        plan
    }

    fn live_index(&self, stage: &str, fan: u32) -> usize {
        self.stages
            .iter()
            .rposition(|s| s.stage_id == stage && s.fan_index == fan && !s.state.is_terminal())
            .unwrap_or_else(|| panic!("no live stage run for {stage}#{fan}: {:#?}", self.stages))
    }

    /// Whether the pending spawns are a team group, admitted whole.
    pub fn pending_team(&self) -> bool {
        self.pending.iter().any(|p| p.team.is_some())
    }

    /// A gate, a merge or a delivery opens only once nothing runs or waits.
    fn assert_settled(&self, what: &str) {
        assert!(
            self.live().is_empty(),
            "{what} opens once its stage settled"
        );
        assert!(self.pending.is_empty(), "nothing is pending at {what}");
    }

    fn apply(&mut self, actions: &[EngineAction], admit: usize) {
        let spawns: Vec<SpawnPlan> = spawn_plans(actions).into_iter().cloned().collect();
        let admit = gang_admit(&spawns, admit);
        let mut admitted = 0;
        for action in actions {
            match action {
                EngineAction::Spawn(plan) if admitted < admit => {
                    admitted += 1;
                    self.persist_spawn(plan);
                }
                EngineAction::Spawn(plan) => self.pending.push(plan.clone()),
                EngineAction::CancelStage { stage_run, reason } => {
                    self.persist_cancel(stage_run, reason)
                }
                EngineAction::Finish { state } => {
                    self.pending.clear();
                    self.open_gate = None;
                    self.open_merge = None;
                    self.open_ship = None;
                    let at = self.tick();
                    self.run.finish(state.clone(), at).expect("finish once");
                }
                EngineAction::AwaitOperator(wait) => {
                    self.assert_settled("a gate");
                    self.run.state = RunState::AwaitingOperator {
                        stage_id: wait.stage_id.clone(),
                        attempt: wait.attempt,
                        subject: wait.subject,
                    };
                    self.open_gate = Some(wait.clone());
                }
                EngineAction::MergeTeam(plan) => {
                    self.assert_settled("a merge");
                    assert!(self.open_merge.is_none(), "one merge at a time");
                    self.open_merge = Some(plan.clone());
                }
                EngineAction::Ship(plan) => {
                    self.assert_settled("a delivery");
                    assert!(self.open_ship.is_none(), "one delivery per run");
                    self.open_ship = Some(plan.clone());
                }
            }
        }
        let owed_now = if self.run.state.is_terminal() {
            Vec::new()
        } else {
            self.pending_actions()
        };
        assert_eq!(
            self.resume_fresh(),
            owed_now,
            "after persisting, a resumed driver owes exactly the pending spawns"
        );
    }

    /// What a resumed driver must owe given the live driver's `actions`: a
    /// decision replaces everything; otherwise unadmitted spawns come first.
    fn expected_owed(&self, actions: &[EngineAction]) -> Vec<EngineAction> {
        if finish_state(actions).is_some() {
            return actions.to_vec();
        }
        let mut owed = self.pending_actions();
        owed.extend(actions.iter().cloned());
        owed
    }

    /// What a restarted daemon still owes: pending spawns, or the open gate.
    fn pending_actions(&self) -> Vec<EngineAction> {
        let mut owed: Vec<EngineAction> = self
            .pending
            .iter()
            .cloned()
            .map(EngineAction::Spawn)
            .collect();
        owed.extend(self.open_gate.clone().map(EngineAction::AwaitOperator));
        owed.extend(self.open_merge.clone().map(EngineAction::MergeTeam));
        owed.extend(self.open_ship.clone().map(EngineAction::Ship));
        owed
    }

    fn persist_spawn(&mut self, plan: &SpawnPlan) {
        assert!(
            !self.run.state.is_terminal(),
            "spawn after the run finished"
        );
        let duplicate = self.stages.iter().any(|s| {
            s.stage_id == plan.stage_id
                && s.attempt == plan.attempt
                && s.fan_index == plan.fan_index
        });
        assert!(!duplicate, "slot spawned twice: {plan:?}");
        self.push_stage_run(plan);
    }

    fn push_stage_run(&mut self, plan: &SpawnPlan) {
        self.run.state = RunState::Running {
            stage_id: plan.stage_id.clone(),
            attempt: plan.attempt,
        };
        let started_at = self.tick();
        let id = StageRunId::new();
        self.plans.insert(id.clone(), plan.clone());
        self.stages.push(StageRun {
            id,
            run: self.run.id.clone(),
            stage_id: plan.stage_id.clone(),
            kind: Some(plan.kind),
            attempt: plan.attempt,
            fan_index: plan.fan_index,
            model_segments: Vec::new(),
            infrastructure_failure: false,
            host_context: Default::default(),
            choice: plan.choice.clone(),
            aether_agent: None,
            aether_task: None,
            state: StageState::Running,
            verdict: None,
            summary: None,
            structured: None,
            cost_usd: 0.0,
            started_at,
            finished_at: None,
        });
    }

    /// Cancel as the daemon does: the reason becomes the summary.
    fn persist_cancel(&mut self, id: &StageRunId, reason: &str) {
        let at = self.tick();
        let stage = self
            .stages
            .iter_mut()
            .find(|s| &s.id == id)
            .expect("cancelled stage run exists");
        assert!(!stage.state.is_terminal(), "cancel of a finished stage run");
        stage.state = StageState::Cancelled;
        stage.summary = Some(reason.to_owned());
        stage.finished_at = Some(at);
    }

    fn tick(&mut self) -> Timestamp {
        self.clock += std::time::Duration::from_millis(1);
        self.clock
    }
}

/// The commit and tree every simulated record stage reports.
pub const COMMIT: &str = "1111111111111111111111111111111111111111";
pub const TREE: &str = "2222222222222222222222222222222222222222";

/// The ship record a record stage's agent writes.
pub fn ship_record() -> serde_json::Value {
    serde_json::json!({"commit": COMMIT, "tree": TREE, "checks": ["cargo test"],
        "pr": {"title": "fix(widget): handle the empty case", "body": "What changed and why."}})
}

/// Put the ship record on a completed result, beside what the agent wrote.
fn write_ship_record(stage: &mut StageRun) {
    if stage.state != StageState::Completed {
        return;
    }
    let data = stage
        .structured
        .get_or_insert_with(|| serde_json::json!({}));
    let fields = data.as_object_mut().expect("structured data is an object");
    for (key, value) in ship_record().as_object().expect("an object") {
        fields.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

/// How many of `plans` a dispatcher with `free` slots admits: a team
/// group is admitted whole or not at all.
fn gang_admit(plans: &[SpawnPlan], free: usize) -> usize {
    let team = plans.iter().any(|p| p.team.is_some());
    assert!(
        !team || plans.iter().all(|p| p.team.is_some()),
        "a team group is pending alone"
    );
    if team && free < plans.len() { 0 } else { free }
}

fn apply_agent(stage: &mut StageRun, agent: &Agent, at: Timestamp) {
    let completed = |summary: String, structured: Option<serde_json::Value>, persist: bool| {
        let verdict = persist.then(|| Verdict::extract(&summary, structured.as_ref()));
        (StageState::Completed, Some(summary), structured, verdict)
    };
    let (state, summary, structured, verdict) = match agent {
        Agent::Done(text) => completed((*text).to_owned(), None, false),
        Agent::DoneStructured(text, data) => {
            completed((*text).to_owned(), Some(data.clone()), false)
        }
        Agent::Pass => completed(
            "Looks correct and well tested.\nVERDICT: PASS".into(),
            Some(serde_json::json!({"verdict": "pass", "findings": []})),
            true,
        ),
        Agent::Fail(title) => completed(
            format!("Found a problem: {title}.\nVERDICT: FAIL"),
            Some(serde_json::json!({"verdict": "fail", "findings": [
                {"severity": "high", "title": title, "detail": "fix it", "path": "src/lib.rs:1"}
            ]})),
            true,
        ),
        Agent::SummaryVerdict(text) => completed((*text).to_owned(), None, false),
        Agent::NoVerdict => completed("I looked around.".into(), None, true),
        Agent::Error(error) => (
            StageState::Failed {
                error: (*error).to_owned(),
            },
            None,
            None,
            None,
        ),
        Agent::Blocked(text) => (
            StageState::Failed {
                error: format!("BLOCKED: {text}"),
            },
            None,
            None,
            None,
        ),
        Agent::TimedOut => (StageState::TimedOut, None, None, None),
        Agent::Cancelled => (StageState::Cancelled, None, None, None),
    };
    stage.state = state;
    stage.summary = summary;
    stage.structured = structured;
    stage.verdict = verdict;
    stage.finished_at = Some(at);
}

/// `(stage, attempt, fan)` of every spawn in `actions`.
pub fn spawned(actions: &[EngineAction]) -> Vec<(String, u32, u32)> {
    actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::Spawn(p) => Some((p.stage_id.clone(), p.attempt, p.fan_index)),
            _ => None,
        })
        .collect()
}

pub fn spawn_plans(actions: &[EngineAction]) -> Vec<&SpawnPlan> {
    actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::Spawn(p) => Some(p),
            _ => None,
        })
        .collect()
}

pub fn finish_state(actions: &[EngineAction]) -> Option<&RunState> {
    let finishes: Vec<&RunState> = actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::Finish { state } => Some(state),
            _ => None,
        })
        .collect();
    assert!(finishes.len() <= 1, "at most one Finish per step");
    if let Some(last) = actions.last() {
        let finish_is_last = matches!(last, EngineAction::Finish { .. });
        assert!(
            finishes.is_empty() || finish_is_last,
            "Finish must be the last action"
        );
    }
    finishes.first().copied()
}

pub fn awaited(actions: &[EngineAction]) -> Vec<&OperatorWait> {
    actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::AwaitOperator(w) => Some(w),
            _ => None,
        })
        .collect()
}

/// The deliveries `actions` ask of archond.
pub fn ship_plans(actions: &[EngineAction]) -> Vec<&ShipPlan> {
    actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::Ship(plan) => Some(plan),
            _ => None,
        })
        .collect()
}

pub fn merge_plans(actions: &[EngineAction]) -> Vec<&MergePlan> {
    actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::MergeTeam(p) => Some(p),
            _ => None,
        })
        .collect()
}

pub fn cancelled(actions: &[EngineAction]) -> Vec<StageRunId> {
    actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::CancelStage { stage_run, .. } => Some(stage_run.clone()),
            _ => None,
        })
        .collect()
}

pub fn slots(list: &[(&str, u32, u32)]) -> Vec<(String, u32, u32)> {
    list.iter()
        .map(|(s, a, f)| ((*s).to_owned(), *a, *f))
        .collect()
}
