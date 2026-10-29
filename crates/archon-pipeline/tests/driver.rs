//! RunDriver scenarios (ARCHITECTURE §6.4). Every step of every scenario
//! runs through `sim::Sim`, which also checks the resume property: a driver
//! rebuilt from persisted stage runs owes exactly what the live one said.

mod sim;
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use archon_core::autonomy::{Autonomy, ReviewLevel};
use archon_core::catalog::{Gate, StageKind, Tier, WorkspaceMode};
use archon_core::id::{PipelineRunId, QueueItemId, StageRunId};
use archon_core::model::{Effort, ModelChoice, ModelPlan, reviewers_key};
use archon_core::run::{RunState, StageRun, StageState};
use archon_pipeline::autonomy::augment;
use archon_pipeline::review::Reviewer;
use archon_pipeline::team::{MergeOutcome, worker_branch};
use archon_pipeline::{EngineAction, RunDriver, ShipOutcome, ShipPlan};
use sim::{Agent, Sim, cancelled, finish_state, ship_plans, slots, spawn_plans, spawned};

/// The pull request every simulated delivery opens.
const PR: &str = "https://github.com/o/r/pull/7";

/// What `direct` adds to a summary at review level none.
const NO_REVIEW: &str = "\n\nREVIEW: none, as this item's review level asks";

fn sim(name: &str) -> Sim {
    let spec = support::builtin(name);
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    Sim::new(spec, plan)
}

fn succeeded(actions: &[EngineAction]) -> String {
    match finish_state(actions) {
        Some(RunState::Succeeded { summary }) => summary.clone(),
        other => panic!("expected success, got {other:?} from {actions:#?}"),
    }
}

/// The delivery `actions` ask of archond; no agent ships a built-in.
fn shipping(actions: &[EngineAction]) -> &ShipPlan {
    assert!(spawned(actions).is_empty(), "no agent ships: {actions:#?}");
    let plans = ship_plans(actions);
    assert_eq!(plans.len(), 1, "one delivery: {actions:#?}");
    plans[0]
}

fn failed(actions: &[EngineAction]) -> (String, String) {
    match finish_state(actions) {
        Some(RunState::Failed { stage_id, error }) => (stage_id.clone(), error.clone()),
        other => panic!("expected failure, got {other:?} from {actions:#?}"),
    }
}

#[test]
fn linear_pipeline_runs_each_stage_once_and_succeeds() {
    let mut sim = sim("direct");
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("implement", 1, 0)]));
    let plan = spawn_plans(&first)[0];
    assert!(
        plan.prompt.contains("Fix the widget"),
        "task title rendered"
    );
    assert!(
        plan.prompt.contains("The widget breaks on empty input."),
        "brief rendered"
    );
    assert!(!plan.prompt.contains("{{"), "no raw placeholder left");
    assert!(
        plan.instructions.contains("/archon/worktrees/demo/w1"),
        "contract names the workspace"
    );
    assert_eq!(plan.timeout_minutes, 60);

    let next = sim.settle(
        "implement",
        0,
        Agent::Done("Changed parse() to accept empty input."),
    );
    let plan = shipping(&next);
    assert_eq!(plan.record_stage, "implement");
    assert_eq!(plan.record.commit, sim::COMMIT);
    assert_eq!(plan.record.tree, sim::TREE);
    let summary = format!("Changed parse() to accept empty input.{NO_REVIEW}");
    assert_eq!(plan.summary, summary);
    assert_eq!(plan.body, format!("What changed and why.{NO_REVIEW}"));
    let item = format!("Archon item: {}\n", sim.run.item);
    let delivered = archon_pipeline::pr_body(&plan.body, sim.run.item.as_str(), Default::default());
    assert_eq!(
        delivered,
        format!("{}\n\n{item}", plan.body),
        "delivery names the item"
    );
    assert!(sim.finished().is_none(), "the run waits for its delivery");

    let done = sim.ship_pr(PR);
    assert_eq!(
        succeeded(&done),
        format!("{summary}\n\nSHIPPED: {PR} (commit {})", sim::COMMIT),
        "the last stage's summary, then where it shipped"
    );
    assert!(sim.finished().is_some());
}

#[test]
fn failed_review_loops_to_implement_with_feedback_then_passes() {
    let mut sim = sim("reviewed");
    sim.start();
    sim.settle("implement", 0, Agent::Done("first try"));
    let looped = sim.settle("review", 0, Agent::Fail("off-by-one in window end"));
    assert_eq!(spawned(&looped), slots(&[("implement", 2, 0)]));
    let prompt = &spawn_plans(&looped)[0].prompt;
    assert!(
        prompt.contains("off-by-one in window end"),
        "finding title in feedback"
    );
    assert!(prompt.contains("src/lib.rs:1"), "finding path in feedback");
    assert!(prompt.contains("This is attempt 2"), "attempt rendered");

    let review2 = sim.settle("implement", 0, Agent::Done("fixed the window end"));
    assert_eq!(spawned(&review2), slots(&[("review", 2, 0)]));
    assert!(
        spawn_plans(&review2)[0]
            .prompt
            .contains("fixed the window end")
    );

    let ship = sim.settle("review", 0, Agent::Pass);
    let body = &shipping(&ship).body;
    assert!(
        body.contains("## Review and verification\n\n### `review`\n\nLooks correct"),
        "the review that passed is in the body: {body}"
    );
    assert!(!body.contains("first try"), "older attempts are superseded");
    succeeded(&sim.ship_pr(PR));
}

#[test]
fn exhausted_fix_loops_fail_the_run_at_the_gate() {
    let mut sim = sim("reviewed");
    sim.start();
    for attempt in 1..=3 {
        sim.settle("implement", 0, Agent::Done("attempt"));
        let actions = sim.settle("review", 0, Agent::Fail("still broken"));
        if attempt < 3 {
            assert_eq!(spawned(&actions), slots(&[("implement", attempt + 1, 0)]));
            continue;
        }
        assert!(spawned(&actions).is_empty(), "no third loop");
        let (stage, error) = failed(&actions);
        assert_eq!(stage, "review");
        assert!(error.contains("after 2 of 2 fix loop(s)"), "{error}");
        assert!(error.contains("still broken"), "{error}");
    }
}

/// build → review (3 agents, majority) → report, with the report prompt
/// showing the joined review summaries.
fn majority_sim() -> Sim {
    let toml = r#"
        [pipeline]
        name = "vote"
        description = "majority vote"
        version = 1
        [[stage]]
        id = "build"
        kind = "work"
        prompt = "build.md"
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        fan_out = 3
        gate = "majority"
        [[stage]]
        id = "report"
        kind = "report"
        prompt = "report.md"
    "#;
    let spec = Arc::new(support::spec_from(
        toml,
        &[("report.md", "Reviews:\n{{previous.review.summary}}")],
    ));
    spec.validate("vote").expect("valid");
    let plan = support::plan_for(
        &spec,
        &["claude-code/opus", "codex/astra", "claude-code/sonnet"],
    );
    Sim::new(spec, plan)
}

#[test]
fn majority_gate_passes_with_two_of_three_reviewers() {
    let mut sim = majority_sim();
    sim.start();
    let reviews = sim.settle("build", 0, Agent::Done("built"));
    assert_eq!(
        spawned(&reviews),
        slots(&[("review", 1, 0), ("review", 1, 1), ("review", 1, 2)])
    );
    assert!(
        sim.settle("review", 1, Agent::Fail("nit")).is_empty(),
        "waits for all slots"
    );
    assert!(sim.settle("review", 0, Agent::Pass).is_empty());
    let report = sim.settle("review", 2, Agent::Pass);
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    let prompt = &spawn_plans(&report)[0].prompt;
    assert!(
        prompt.contains("[#1] Looks correct") && prompt.contains("[#2] Found a problem"),
        "{prompt}"
    );
}

#[test]
fn majority_gate_fails_with_one_of_three_reviewers() {
    let mut sim = majority_sim();
    sim.start();
    sim.settle("build", 0, Agent::Done("built"));
    sim.settle("review", 0, Agent::Pass);
    sim.settle("review", 1, Agent::Fail("wrong"));
    let actions = sim.settle("review", 2, Agent::NoVerdict);
    let (stage, error) = failed(&actions);
    assert_eq!(stage, "review");
    assert!(
        error.contains("gate majority failed: 1 of 3 passed"),
        "{error}"
    );
    assert!(
        error.contains("verdict missing (counts as fail)"),
        "unknown verdict counts as fail: {error}"
    );
}

#[test]
fn an_unreadable_verdict_fails_the_gate_quoting_what_it_said() {
    let mut sim = majority_sim();
    sim.start();
    sim.settle("build", 0, Agent::Done("built"));
    sim.settle("review", 0, Agent::Pass);
    sim.settle("review", 1, Agent::Fail("wrong"));
    let unclear = serde_json::json!({"verdict": "lgtm mostly"});
    let actions = sim.settle("review", 2, Agent::DoneStructured("Read it.", unclear));
    let (_, error) = failed(&actions);
    assert!(
        error.contains(r#"verdict "lgtm mostly" unreadable as pass or fail (counts as fail)"#),
        "{error}"
    );
}

#[test]
fn any_gate_passes_with_a_single_pass() {
    let toml = r#"
        [pipeline]
        name = "any"
        description = "any"
        version = 1
        [[stage]]
        id = "check"
        kind = "test"
        prompt = "check.md"
        fan_out = 2
        gate = "any"
    "#;
    let spec = Arc::new(support::spec_from(toml, &[]));
    let plan = support::plan_for(&spec, &[]);
    let mut sim = Sim::new(spec, plan);
    sim.start();
    sim.settle("check", 0, Agent::Error("crashed"));
    let summary = succeeded(&sim.settle("check", 1, Agent::Pass));
    assert!(
        summary.starts_with("[#2] Looks correct"),
        "only completed slots summarised: {summary}"
    );
}

#[test]
fn cancel_mid_stage_cancels_live_agents_and_ignores_late_settles() {
    let mut sim = sim("strict-reviewed");
    sim.start();
    sim.settle("implement", 0, Agent::Done("done"));
    sim.settle("test", 0, Agent::Pass);
    sim.settle("review", 0, Agent::Pass);
    let live: Vec<_> = sim.live().iter().map(|s| s.id.clone()).collect();
    assert_eq!(live.len(), 2);

    let actions = sim.cancel("operator changed their mind");
    assert_eq!(cancelled(&actions), live);
    assert_eq!(
        finish_state(&actions),
        Some(&RunState::Cancelled {
            reason: Some("operator changed their mind".into())
        })
    );
    assert!(
        sim.driver.cancel(&[], "again").is_empty(),
        "cancel is idempotent"
    );
    assert!(
        sim.resume_fresh().is_empty(),
        "a cancelled run owes nothing"
    );
    let late = sim
        .stages
        .iter()
        .find(|s| s.id == live[0])
        .cloned()
        .expect("stage run");
    assert!(sim.driver.on_stage_settled(&late, &sim.stages).is_empty());
}

#[test]
fn blocked_stage_fails_without_looping() {
    let mut sim = sim("reviewed");
    sim.start();
    sim.settle("implement", 0, Agent::Done("done"));
    let actions = sim.settle("review", 0, Agent::Blocked("need the staging API key"));
    assert!(
        spawned(&actions).is_empty(),
        "BLOCKED never loops, even with on_fail"
    );
    let (stage, error) = failed(&actions);
    assert_eq!(stage, "review");
    assert!(
        error.starts_with("BLOCKED: need the staging API key"),
        "{error}"
    );
}

#[test]
fn blocked_slot_cancels_its_live_siblings() {
    let mut sim = sim("strict-reviewed");
    sim.start();
    sim.settle("implement", 0, Agent::Done("done"));
    sim.settle("test", 0, Agent::Pass);
    let siblings: Vec<_> = sim
        .live()
        .iter()
        .filter(|s| s.fan_index != 1)
        .map(|s| s.id.clone())
        .collect();
    let actions = sim.settle("review", 1, Agent::Blocked("repository is archived"));
    assert_eq!(cancelled(&actions), siblings);
    assert!(failed(&actions).1.starts_with("BLOCKED:"));
    assert!(sim.live().is_empty());
}

#[test]
fn strict_pipeline_counts_loops_per_gate_and_cycles_reviewer_choices() {
    let mut sim = sim("strict-reviewed");
    sim.start();
    sim.settle("implement", 0, Agent::Done("v1"));
    let back = sim.settle("test", 0, Agent::Fail("test_empty fails"));
    assert_eq!(spawned(&back), slots(&[("implement", 2, 0)]));
    sim.settle("implement", 0, Agent::Done("v2"));
    let reviews = sim.settle("test", 0, Agent::Pass);
    assert_eq!(
        spawned(&reviews),
        slots(&[("review", 1, 0), ("review", 1, 1), ("review", 1, 2)])
    );
    let models: Vec<String> = spawn_plans(&reviews)
        .iter()
        .map(|p| p.choice.model.to_string())
        .collect();
    assert_eq!(
        models,
        ["claude-code/opus", "codex/astra", "claude-code/opus"],
        "choices cycle"
    );
    let prompts: Vec<&str> = spawn_plans(&reviews)
        .iter()
        .map(|p| p.prompt.as_str())
        .collect();
    assert!(
        prompts[1].starts_with("# Independent review 2 of 3"),
        "fan.index is 1-based"
    );

    sim.settle("review", 0, Agent::Pass);
    sim.settle("review", 1, Agent::TimedOut);
    let back = sim.settle("review", 2, Agent::Pass);
    assert_eq!(
        spawned(&back),
        slots(&[("implement", 3, 0)]),
        "timed-out reviewer fails all_pass"
    );
    assert!(
        spawn_plans(&back)[0]
            .prompt
            .contains("timed out after 0 minutes")
    );
    sim.settle("implement", 0, Agent::Done("v3"));
    let reviews = sim.settle("test", 0, Agent::Pass);
    assert_eq!(
        spawned(&reviews)[0],
        ("review".to_owned(), 2, 0),
        "test attempt 3 leads to review attempt 2"
    );
    for fan in 0..3 {
        let actions = sim.settle("review", fan, Agent::Pass);
        if fan == 2 {
            let body = &shipping(&actions).body;
            assert!(body.contains("### `test`") && body.contains("### `review`"));
        }
    }
    succeeded(&sim.ship_pr(PR));
}

#[test]
fn e2e_failure_loops_to_setup_once_then_fails() {
    let mut sim = sim("e2e-testing");
    sim.start();
    sim.settle("setup", 0, Agent::Done("start.sh ready"));
    let back = sim.settle("e2e", 0, Agent::Fail("login page 500"));
    assert_eq!(spawned(&back), slots(&[("setup", 2, 0)]));
    assert!(spawn_plans(&back)[0].prompt.contains("login page 500"));
    sim.settle("setup", 0, Agent::Done("fixed port"));
    let (stage, error) = failed(&sim.settle("e2e", 0, Agent::Fail("login page 500")));
    assert_eq!(stage, "e2e");
    assert!(error.contains("after 1 of 1 fix loop(s)"), "{error}");
}

#[test]
fn a_markdown_summary_verdict_beside_structured_findings_passes_the_gate() {
    let mut sim = sim("e2e-testing");
    sim.start();
    sim.settle("setup", 0, Agent::Done("start.sh ready"));
    let report = sim.settle(
        "e2e",
        0,
        Agent::DoneStructured(
            "Ran the suite.\n\n**Verdict:** PASS — all 214 tests pass",
            serde_json::json!({"findings": []}),
        ),
    );
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
}

#[test]
fn e2e_passes_to_the_report_stage() {
    let mut sim = sim("e2e-testing");
    sim.start();
    sim.settle("setup", 0, Agent::Done("start.sh ready"));
    let report = sim.settle(
        "e2e",
        0,
        Agent::SummaryVerdict("all 5 scenarios passed\nVERDICT: PASS"),
    );
    assert_eq!(
        spawned(&report),
        slots(&[("report", 1, 0)]),
        "summary VERDICT line is read"
    );
    assert!(spawn_plans(&report)[0].instructions.contains("\"reports\""));
    succeeded(&sim.settle(
        "report",
        0,
        Agent::Done("report at /w/.archon/reports/e2e/index.html"),
    ));
}

#[test]
fn ungated_fan_out_fails_when_any_agent_fails() {
    let mut sim = sim("agent-testing");
    sim.start();
    let testers = sim.settle("plan", 0, Agent::Done("two charters"));
    assert_eq!(
        spawned(&testers),
        slots(&[("exercise", 1, 0), ("exercise", 1, 1)])
    );
    assert!(
        spawn_plans(&testers)[1]
            .prompt
            .contains("Take charter 2, then charter 2 +\n2, then 2 + 2 × 2")
    );
    sim.settle("exercise", 1, Agent::Done("found 2 defects"));
    let (stage, error) =
        failed(&sim.settle("exercise", 0, Agent::Error("could not start the app")));
    assert_eq!(stage, "exercise");
    assert!(
        error.contains("1 of 2 agent(s) did not complete"),
        "{error}"
    );
    assert!(error.contains("could not start the app"), "{error}");
}

#[test]
fn agent_testing_report_sees_every_testers_summary() {
    let mut sim = sim("agent-testing");
    sim.start();
    sim.settle("plan", 0, Agent::Done("two charters"));
    sim.settle("exercise", 0, Agent::Done("tester one findings"));
    let report = sim.settle("exercise", 1, Agent::Done("tester two findings"));
    let prompt = &spawn_plans(&report)[0].prompt;
    assert!(
        prompt.contains("[#1] tester one findings") && prompt.contains("[#2] tester two findings")
    );
    succeeded(&sim.settle("report", 0, Agent::Done("report written")));
}

#[test]
fn errand_completes_with_text_or_a_page_with_and_without_a_repository() {
    const WORKSPACE: &str = "/archon/scratch/errand-item";
    const REPORT: &str = "/archon/scratch/errand-item/.archon/reports/errand/index.html";
    for with_repo in [false, true] {
        for with_page in [false, true] {
            let mut sim = sim("errand");
            sim.base
                .insert("task.title".into(), "Compare options".into());
            sim.base.insert(
                "task.brief".into(),
                "Compare the two options and cite the evidence.".into(),
            );
            sim.base.insert("workspace".into(), WORKSPACE.into());
            if !with_repo {
                sim.base.insert("repo.name".into(), String::new());
                sim.base.insert("repo.path".into(), String::new());
            }
            sim.run.workspace = WORKSPACE.into();
            sim.driver = RunDriver::new(sim.spec.clone(), sim.plan.clone(), sim.base.clone())
                .expect("errand accepts optional repository variables");

            let start = sim.start();
            assert_eq!(spawned(&start), slots(&[("errand", 1, 0)]));
            let plan = spawn_plans(&start)[0];
            assert!(plan.prompt.contains("Compare options"));
            assert!(
                plan.prompt
                    .contains("Compare the two options and cite the evidence.")
            );
            assert!(plan.prompt.contains(REPORT));
            assert!(!plan.prompt.contains("{{"), "all placeholders rendered");
            assert_eq!(plan.prompt.contains("/src/demo"), with_repo);
            assert!(plan.instructions.contains(WORKSPACE));
            assert!(plan.instructions.contains("\"reports\""));
            assert_eq!(plan.timeout_minutes, 45);

            let reports: Vec<&str> = if with_page { vec![REPORT] } else { Vec::new() };
            let done = sim.settle(
                "errand",
                0,
                Agent::DoneStructured(
                    "Option A fits the request; option B needs more setup.",
                    serde_json::json!({"reports": reports, "sources": ["https://example.test/docs"]}),
                ),
            );
            assert_eq!(
                succeeded(&done),
                "Option A fits the request; option B needs more setup.",
                "the answer is preserved even without a page"
            );
            assert!(spawned(&done).is_empty(), "no ship or follow-up stage");
            assert_eq!(sim.stages.len(), 1);
            let stage = &sim.stages[0];
            assert_eq!(stage.state, StageState::Completed);
            assert_eq!(
                stage.structured.as_ref().unwrap()["reports"],
                serde_json::json!(reports)
            );
            assert!(sim.resume_fresh().is_empty(), "completed runs owe nothing");
        }
    }
}

#[test]
fn errand_reports_a_research_failure_without_shipping() {
    let mut sim = sim("errand");
    sim.start();
    let done = sim.settle("errand", 0, Agent::Error("required source is unreadable"));
    let (stage, error) = failed(&done);
    assert_eq!(stage, "errand");
    assert!(error.contains("required source is unreadable"), "{error}");
    assert!(spawned(&done).is_empty());
    assert!(sim.resume_fresh().is_empty());
}

#[test]
fn work_failure_without_on_fail_fails_the_run() {
    let mut sim = sim("ask");
    sim.start();
    let (stage, error) = failed(&sim.settle("answer", 0, Agent::Error("repository unreadable")));
    assert_eq!(stage, "answer");
    assert!(error.contains("repository unreadable"), "{error}");
}

#[test]
fn a_cancelled_work_agent_fails_the_run() {
    let mut sim = sim("direct");
    sim.start();
    let (stage, error) = failed(&sim.settle("implement", 0, Agent::Cancelled));
    assert_eq!(stage, "implement");
    assert!(error.contains("cancelled before it finished"), "{error}");
}

#[test]
fn partially_admitted_fan_out_is_owed_after_a_restart() {
    let toml = r#"
        [pipeline]
        name = "fan"
        description = "fan"
        version = 1
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        fan_out = 3
        gate = "all_pass"
    "#;
    let spec = Arc::new(support::spec_from(toml, &[]));
    let plan = support::plan_for(&spec, &[]);
    let mut sim = Sim::new(spec, plan);
    sim.start_partial(1);
    let owed = sim.resume_fresh();
    assert_eq!(spawned(&owed), slots(&[("review", 1, 1), ("review", 1, 2)]));
    assert!(
        sim.settle("review", 0, Agent::Pass).is_empty(),
        "slots still pending"
    );
    sim.admit(2);
    assert!(sim.resume_fresh().is_empty());
    sim.settle("review", 1, Agent::Pass);
    succeeded(&sim.settle("review", 2, Agent::Pass));
}

#[test]
fn an_infra_respawn_supersedes_the_failed_stage_run() {
    let mut sim = sim("direct");
    sim.start();
    let retry = sim.infra_respawn("implement", 0);
    assert_eq!((retry.stage_id.as_str(), retry.attempt), ("implement", 1));
    assert!(
        sim.resume_fresh().is_empty(),
        "the retry is live; nothing is owed"
    );
    let next = sim.settle("implement", 0, Agent::Done("done on retry"));
    assert_eq!(shipping(&next).summary, format!("done on retry{NO_REVIEW}"));
}

fn fan_sim(fan_out: u32) -> Sim {
    let toml = format!(
        r#"
        [pipeline]
        name = "fan"
        description = "fan"
        version = 1
        [[stage]]
        id = "build"
        kind = "work"
        prompt = "build.md"
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        fan_out = {fan_out}
        gate = "all_pass"
        on_fail = {{ goto = "build", max_loops = 1 }}
    "#
    );
    let spec = Arc::new(support::spec_from(
        &toml,
        &[(
            "review.md",
            "Review {{fan.index}}.\n{{previous.build.summary}}",
        )],
    ));
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    let mut sim = Sim::new(spec, plan);
    sim.start();
    sim.settle("build", 0, Agent::Done("built"));
    sim
}

#[test]
fn infra_respawn_repeats_a_fan_out_slot_until_it_settles() {
    let mut sim = fan_sim(3);
    sim.settle("review", 0, Agent::Pass);
    let first = sim.infra_respawn("review", 1);
    assert_eq!((first.fan_index, first.attempt), (1, 1));
    assert!(
        first.prompt.contains("Review 2.\nbuilt"),
        "{}",
        first.prompt
    );
    let second = sim.infra_respawn("review", 1);
    assert_eq!(first, second, "a second retry of the slot is identical");
    sim.settle("review", 1, Agent::Pass);
    succeeded(&sim.settle("review", 2, Agent::Pass));
}

#[test]
fn respawn_refuses_a_decided_run_or_a_stale_group() {
    let mut sim = fan_sim(2);
    // A sibling's `BLOCKED:` decided the run: the infra failure is moot.
    let live = sim.live()[0].clone();
    sim.settle("review", 1, Agent::Blocked("needs a token"));
    let mut failed = live;
    failed.state = StageState::Failed {
        error: "engine died".into(),
    };
    assert_eq!(sim.driver.respawn(&failed, &sim.stages), None);
    assert_eq!(sim.resumed_driver().respawn(&failed, &sim.stages), None);

    // A failure of an already-passed group is never re-run.
    let mut sim = fan_sim(1);
    sim.settle("review", 0, Agent::Fail("off by one"));
    let mut old = sim
        .stages
        .iter()
        .find(|s| s.stage_id == "build")
        .expect("build ran")
        .clone();
    old.state = StageState::Failed {
        error: "engine died".into(),
    };
    assert_eq!(sim.driver.respawn(&old, &sim.stages), None);

    // A `BLOCKED:` failure escalates; it is never re-spawned.
    let sim = fan_sim(1);
    let mut blocked = sim.live()[0].clone();
    blocked.state = StageState::Failed {
        error: "BLOCKED: needs a token".into(),
    };
    assert_eq!(sim.driver.respawn(&blocked, &sim.stages), None);
}

#[test]
fn resume_of_a_finished_run_owes_nothing() {
    let mut sim = sim("ask");
    sim.start();
    succeeded(&sim.settle("answer", 0, Agent::Done("42")));
    assert!(sim.resume_fresh().is_empty());
}

#[test]
fn resume_decides_a_group_that_settled_before_the_crash() {
    let mut sim = sim("direct");
    sim.start();
    // Persist the outcome but crash before the driver hears of it.
    let at = archon_core::now();
    let stage = sim.stages.last_mut().expect("implement");
    stage.state = archon_core::run::StageState::Completed;
    stage.summary = Some("done before crash".into());
    stage.structured = Some(sim::ship_record());
    stage.finished_at = Some(at);
    let owed = sim.resume_fresh();
    assert_eq!(
        shipping(&owed).summary,
        format!("done before crash{NO_REVIEW}")
    );
}

fn driver_error(pipeline: &str, plan: ModelPlan, vars: BTreeMap<String, String>) -> String {
    RunDriver::new(support::builtin(pipeline), plan, vars)
        .expect_err("refused")
        .to_string()
}

fn vars() -> BTreeMap<String, String> {
    support::base_vars(&PipelineRunId::new(), &QueueItemId::new())
}

#[test]
fn new_refuses_incomplete_or_mismatched_plans() {
    let spec = support::builtin("strict-reviewed");
    let full = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);

    let mut missing = full.clone();
    missing.0.remove("test");
    assert!(driver_error("strict-reviewed", missing, vars()).contains("\"test\""));

    let mut empty = full.clone();
    empty.0.insert("review".into(), Vec::new());
    assert!(driver_error("strict-reviewed", empty, vars()).contains("\"review\""));

    let mut extra = full.clone();
    extra
        .0
        .insert("reviews".into(), vec![support::choice("claude-code/opus")]);
    let error = driver_error("strict-reviewed", extra, vars());
    assert!(
        error.contains("\"reviews\"") && error.contains("(stages: implement, test, review)"),
        "{error}"
    );

    let mut too_many = full.clone();
    too_many.0.insert(
        "implement".into(),
        vec![support::choice("a/b"), support::choice("c/d")],
    );
    assert!(driver_error("strict-reviewed", too_many, vars()).contains("runs 1 agent(s)"));

    RunDriver::new(spec, full, vars()).expect("a complete plan is accepted");
}

#[test]
fn new_refuses_incomplete_or_foreign_run_variables() {
    let spec = support::builtin("direct");
    let plan = support::plan_for(&spec, &[]);

    let mut missing = vars();
    missing.remove("branch");
    assert!(driver_error("direct", plan.clone(), missing).contains("\"branch\""));

    let mut foreign = vars();
    foreign.insert("feedback".into(), "sneaky".into());
    assert!(driver_error("direct", plan.clone(), foreign).contains("\"feedback\""));

    let mut empty = vars();
    empty.insert("workspace".into(), String::new());
    assert!(driver_error("direct", plan, empty).contains("workspace"));
}

#[test]
fn new_refuses_a_prompt_that_cannot_render() {
    let toml = r#"
        [pipeline]
        name = "bad"
        description = "bad"
        version = 1
        [[stage]]
        id = "only"
        kind = "work"
        prompt = "only.md"
    "#;
    let spec = Arc::new(support::spec_from(
        toml,
        &[("only.md", "{{previous.later.summary}}")],
    ));
    let plan = support::plan_for(&spec, &[]);
    let error = RunDriver::new(spec, plan, vars())
        .expect_err("refused")
        .to_string();
    assert!(error.contains("previous.later.summary"), "{error}");
}

#[test]
fn loop_feedback_reaches_a_prompt_without_a_feedback_placeholder() {
    let toml = r#"
        [pipeline]
        name = "plain"
        description = "d"
        version = 1
        [[stage]]
        id = "build"
        kind = "work"
        prompt = "build.md"
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        gate = "all_pass"
        on_fail = { goto = "build", max_loops = 1 }
    "#;
    let spec = Arc::new(support::spec_from(
        toml,
        &[("build.md", "Build {{task.title}}.")],
    ));
    let plan = support::plan_for(&spec, &[]);
    let mut sim = Sim::new(spec, plan);
    let first = sim.start();
    assert!(
        !spawn_plans(&first)[0]
            .prompt
            .contains("## Feedback to address")
    );
    sim.settle("build", 0, Agent::Done("built"));
    let back = sim.settle("review", 0, Agent::Fail("off by one"));
    assert_eq!(spawned(&back), slots(&[("build", 2, 0)]));
    let prompt = &spawn_plans(&back)[0].prompt;
    assert!(prompt.contains("## Feedback to address"), "{prompt}");
    assert!(prompt.contains("off by one"), "{prompt}");
}

const TEAM_BRANCH: &str = "archon/w1-fix-the-widget";

/// plan → build (team api, ui) → review → ship; `build_on_fail` is the
/// team stage's `on_fail` table, if any.
fn team_sim(build_on_fail: &str) -> Sim {
    let toml = format!(
        r#"
        [pipeline]
        name = "team"
        description = "team"
        version = 1
        [[stage]]
        id = "plan"
        kind = "work"
        prompt = "plan.md"
        [[stage]]
        id = "build"
        kind = "team"
        prompt = "build.md"
        roles = ["api", "ui"]
        role_prompts = {{ ui = "Own the CSS." }}
        {build_on_fail}
        [[stage]]
        id = "review"
        kind = "review"
        prompt = "review.md"
        gate = "all_pass"
        on_fail = {{ goto = "build", max_loops = 1 }}
        [[stage]]
        id = "ship"
        kind = "ship"
        prompt = "ship.md"
    "#
    );
    let build = "I am {{team.role}} of {{team.roles}} in {{workspace}} on {{branch}}; \
                 branches {{team.branches}}.\n{{feedback}}";
    let review = "Review this.\n{{previous.build.summary}}";
    let prompts = [("build.md", build), ("review.md", review)];
    let spec = Arc::new(support::spec_from(&toml, &prompts));
    spec.validate("team").expect("the team pipeline validates");
    let plan = support::plan_for(&spec, &["claude-code/opus", "codex/astra"]);
    let mut sim = Sim::new(spec, plan);
    sim.start();
    sim
}

/// `role`'s branch for `attempt` in `sim`'s run.
fn role_branch(sim: &Sim, role: &str, attempt: u32) -> String {
    worker_branch(TEAM_BRANCH, &sim.run.id, role, attempt)
}

#[test]
fn team_slots_get_their_own_worktree_branch_and_role() {
    let mut sim = team_sim("");
    let build = sim.settle("plan", 0, Agent::Done("split into api and ui"));
    assert_eq!(spawned(&build), slots(&[("build", 1, 0), ("build", 1, 1)]));
    let plans = spawn_plans(&build);
    for (plan, role) in plans.iter().zip(["api", "ui"]) {
        let slot = plan.team.as_ref().expect("a team slot");
        let worktree = format!("/archon/worktrees/demo/w1/build-{role}");
        assert_eq!(slot.role, role);
        assert_eq!(slot.branch, role_branch(&sim, role, 1));
        assert!(slot.branch.starts_with(&format!("{TEAM_BRANCH}--{role}-")));
        assert_eq!(slot.workspace.to_str(), Some(worktree.as_str()));
        assert_eq!(plan.fan_count, 2);
        assert!(
            plan.prompt.contains(&format!(
                "I am {role} of api, ui in {worktree} on {TEAM_BRANCH}; branches \
                 {}, {}.",
                role_branch(&sim, "api", 1),
                role_branch(&sim, "ui", 1),
            )),
            "{}",
            plan.prompt
        );
        assert!(
            plan.instructions
                .contains(&format!("Work only in `{worktree}`"))
        );
        assert!(
            plan.instructions
                .contains(&format!("You are role `{role}` of roles [api, ui]"))
        );
    }
    assert!(!plans[0].prompt.contains("Own the CSS."));
    assert!(plans[1].prompt.contains("## Your role\n\nOwn the CSS."));
    let plan_stage = &sim.plans[&sim.stages[0].id];
    assert_eq!(plan_stage.team, None, "non-team stages have no slot");
    assert!(plan_stage.instructions.contains(support::TEAM_WORKSPACE));
    let commit = format!("Commit your changes on `{TEAM_BRANCH}` before you finish");
    assert!(
        plan_stage.instructions.contains(&commit),
        "the roles fork from commits"
    );
    assert!(
        !plans[0].instructions.contains(&commit),
        "roles commit on their own branch"
    );
}

#[test]
fn a_passed_team_merges_before_the_next_stage() {
    let mut sim = team_sim("");
    sim.settle("plan", 0, Agent::Done("planned"));
    assert!(sim.settle("build", 0, Agent::Done("api done")).is_empty());
    let merge = sim.settle("build", 1, Agent::Done("ui done"));
    let merges = sim::merge_plans(&merge);
    assert_eq!(merges.len(), 1, "{merge:?}");
    assert_eq!(merge.len(), 1, "the merge comes alone");
    assert_eq!(
        (merges[0].stage_id.as_str(), merges[0].attempt),
        ("build", 1)
    );
    assert_eq!(merges[0].into, TEAM_BRANCH);
    let order: Vec<(&str, &str)> = merges[0]
        .roles
        .iter()
        .map(|r| (r.role.as_str(), r.branch.as_str()))
        .collect();
    let api = role_branch(&sim, "api", 1);
    let ui = role_branch(&sim, "ui", 1);
    assert_eq!(order, [("api", api.as_str()), ("ui", ui.as_str())]);

    let review = sim.merge(MergeOutcome::Merged);
    assert_eq!(spawned(&review), slots(&[("review", 1, 0)]));
    let prompt = &spawn_plans(&review)[0].prompt;
    assert!(prompt.contains("[role api] api done"), "{prompt}");
    assert!(prompt.contains("[role ui] ui done"), "{prompt}");

    let rebuilt = sim.settle("review", 0, Agent::Fail("missing test"));
    assert_eq!(
        spawned(&rebuilt),
        slots(&[("build", 2, 0), ("build", 2, 1)])
    );
    let slot = spawn_plans(&rebuilt)[1].team.clone().expect("team slot");
    assert_eq!(
        slot.branch,
        role_branch(&sim, "ui", 2),
        "a fresh branch per attempt"
    );
    sim.settle("build", 0, Agent::Done("api again"));
    let merge = sim.settle("build", 1, Agent::Done("ui again"));
    assert_eq!(sim::merge_plans(&merge)[0].attempt, 2);
    sim.merge(MergeOutcome::Merged);
    sim.settle("review", 0, Agent::Pass);
    succeeded(&sim.settle("ship", 0, Agent::Done("shipped")));
}

#[test]
fn a_merge_conflict_loops_back_with_the_files_then_fails() {
    let mut sim = team_sim("on_fail = { goto = \"plan\", max_loops = 1 }");
    sim.settle("plan", 0, Agent::Done("planned"));
    sim.settle("build", 0, Agent::Done("api"));
    sim.settle("build", 1, Agent::Done("ui"));
    let conflict = MergeOutcome::Conflict {
        role: "ui".into(),
        files: vec!["src/app.css".into(), "src/api.rs".into()],
    };
    let looped = sim.merge(conflict.clone());
    assert_eq!(spawned(&looped), slots(&[("plan", 2, 0)]));
    let prompt = &spawn_plans(&looped)[0].prompt;
    assert!(prompt.contains("merging role `ui`"), "{prompt}");
    assert!(prompt.contains("`src/app.css`") && prompt.contains("`src/api.rs`"));

    sim.settle("plan", 0, Agent::Done("planned again"));
    sim.settle("build", 0, Agent::Done("api"));
    sim.settle("build", 1, Agent::Done("ui"));
    let (stage, error) = failed(&sim.merge(conflict));
    assert_eq!(stage, "build");
    assert!(error.contains("after 1 of 1 fix loop(s)"), "{error}");
    assert!(error.contains("`src/app.css`"), "{error}");
}

#[test]
fn a_merge_conflict_without_on_fail_fails_the_run() {
    let mut sim = team_sim("");
    sim.settle("plan", 0, Agent::Done("planned"));
    sim.settle("build", 0, Agent::Done("api"));
    sim.settle("build", 1, Agent::Done("ui"));
    let (stage, error) = failed(&sim.merge(MergeOutcome::Conflict {
        role: "api".into(),
        files: vec!["README.md".into()],
    }));
    assert_eq!(stage, "build");
    assert!(
        error.contains("merging role `api` conflicted on 1 file(s)"),
        "{error}"
    );
}

#[test]
fn a_refused_merge_loops_back_with_its_reason_and_branches() {
    let mut sim = team_sim("on_fail = { goto = \"plan\", max_loops = 1 }");
    sim.settle("plan", 0, Agent::Done("planned"));
    sim.settle("build", 0, Agent::Done("api"));
    sim.settle("build", 1, Agent::Done("ui"));
    let looped = sim.merge(MergeOutcome::Refused {
        role: "ui".into(),
        reason: "its worktree has `feature` checked out".into(),
    });
    assert_eq!(spawned(&looped), slots(&[("plan", 2, 0)]));
    let prompt = &spawn_plans(&looped)[0].prompt;
    assert!(
        prompt.contains("role `ui`'s work could not be merged"),
        "{prompt}"
    );
    assert!(prompt.contains("`feature` checked out"), "{prompt}");
    let ui = role_branch(&sim, "ui", 1);
    assert!(prompt.contains(&format!("`ui`: `{ui}`")), "{prompt}");
}

#[test]
fn a_failed_role_cancels_its_live_sibling_and_loops_with_its_name() {
    let mut sim = team_sim("on_fail = { goto = \"plan\", max_loops = 1 }");
    sim.settle("plan", 0, Agent::Done("planned"));
    let ui = sim.live()[1].id.clone();
    let looped = sim.settle("build", 0, Agent::Error("tests would not compile"));
    assert_eq!(sim::cancelled(&looped), vec![ui.clone()]);
    let reason = match &looped[0] {
        EngineAction::CancelStage { reason, .. } => reason.clone(),
        other => panic!("the sibling cancel comes first, got {other:?}"),
    };
    assert!(reason.contains("role `api` failed"), "{reason}");
    assert!(sim::merge_plans(&looped).is_empty());
    assert_eq!(finish_state(&looped), None, "a loop is left");
    assert_eq!(spawned(&looped), slots(&[("plan", 2, 0)]));
    let prompt = &spawn_plans(&looped)[0].prompt;
    assert!(
        prompt.contains("role `api` failed, role `ui` was cancelled"),
        "{prompt}"
    );
    assert!(prompt.contains("`build` role api: failed"), "{prompt}");
    assert!(prompt.contains(&reason), "the sibling's reason: {prompt}");
    let ui_run = sim.stages.iter().find(|s| s.id == ui).expect("ui");
    assert_eq!(ui_run.state, StageState::Cancelled);
    assert_eq!(ui_run.summary.as_deref(), Some(reason.as_str()));
}

#[test]
fn a_timed_out_role_with_no_loop_left_cancels_its_sibling_and_fails() {
    let mut sim = team_sim("");
    sim.settle("plan", 0, Agent::Done("planned"));
    let api = sim.live()[0].id.clone();
    let finished = sim.settle("build", 1, Agent::TimedOut);
    assert_eq!(sim::cancelled(&finished), vec![api]);
    let (stage, error) = failed(&finished);
    assert_eq!(stage, "build");
    assert!(
        error.contains("role `api` was cancelled, role `ui` timed out"),
        "{error}"
    );
    assert!(
        error.contains("cancelled because role `ui` timed out"),
        "{error}"
    );
}

#[test]
fn a_role_that_finished_first_is_not_cancelled_when_its_sibling_fails() {
    let mut sim = team_sim("on_fail = { goto = \"plan\", max_loops = 1 }");
    sim.settle("plan", 0, Agent::Done("planned"));
    assert!(sim.settle("build", 1, Agent::Done("ui")).is_empty());
    let looped = sim.settle("build", 0, Agent::Cancelled);
    assert!(sim::cancelled(&looped).is_empty(), "{looped:?}");
    assert_eq!(spawned(&looped), slots(&[("plan", 2, 0)]));
    let prompt = &spawn_plans(&looped)[0].prompt;
    assert!(prompt.contains("role `api` was cancelled"), "{prompt}");
}

#[test]
fn a_resumed_driver_owes_the_sibling_cancel_until_it_is_persisted() {
    let mut sim = team_sim("on_fail = { goto = \"plan\", max_loops = 1 }");
    sim.settle("plan", 0, Agent::Done("planned"));
    let ui = sim.live()[1].id.clone();
    let api = sim.live()[0].id.clone();
    let mut failed_api = sim.stages.clone();
    let at = failed_api.iter_mut().find(|s| s.id == api).expect("api");
    at.state = StageState::Failed {
        error: "crashed".into(),
    };
    let mut driver = sim.resumed_driver();
    let owed = driver.resume(&sim.run, &failed_api);
    assert_eq!(sim::cancelled(&owed), vec![ui]);
    assert_eq!(spawned(&owed), slots(&[("plan", 2, 0)]));
    assert!(
        driver
            .on_stage_settled(at_of(&failed_api, &api), &failed_api)
            .is_empty(),
        "owed actions are returned once per driver"
    );
}

fn at_of<'a>(stages: &'a [StageRun], id: &StageRunId) -> &'a StageRun {
    stages
        .iter()
        .find(|s| &s.id == id)
        .expect("known stage run")
}

#[test]
fn a_merge_outcome_is_recorded_once_for_a_known_role() {
    let sim = team_sim("");
    let mut driver = sim.resumed_driver();
    let error = driver
        .record_merge("plan", 1, MergeOutcome::Merged)
        .expect_err("not a team stage")
        .to_string();
    assert!(error.contains("not a team stage"), "{error}");
    let stranger = MergeOutcome::Conflict {
        role: "db".into(),
        files: Vec::new(),
    };
    let error = driver
        .record_merge("build", 1, stranger)
        .expect_err("no such role");
    assert!(error.to_string().contains("no role \"db\""), "{error}");
    let refused_stranger = MergeOutcome::Refused {
        role: "db".into(),
        reason: "dirty".into(),
    };
    assert!(driver.record_merge("build", 1, refused_stranger).is_err());
    driver
        .record_merge("build", 1, MergeOutcome::Merged)
        .expect("first");
    driver
        .record_merge("build", 1, MergeOutcome::Merged)
        .expect("the same again");
    let changed = MergeOutcome::Conflict {
        role: "api".into(),
        files: Vec::new(),
    };
    assert!(driver.record_merge("build", 1, changed).is_err());
}

#[test]
fn new_refuses_a_team_pipeline_outside_the_team_layout() {
    let sim = team_sim("");
    let flat = support::base_vars(&PipelineRunId::new(), &QueueItemId::new());
    let error = RunDriver::new(sim.spec.clone(), sim.plan.clone(), flat)
        .expect_err("flat workspace")
        .to_string();
    assert!(error.contains("must be <item>/main"), "{error}");
}

#[test]
fn team_roles_are_listed_for_team_stages_only() {
    let sim = team_sim("");
    let driver = sim.resumed_driver();
    let roles = driver.team_roles("build").expect("a team stage");
    assert_eq!(roles, ["api".to_owned(), "ui".to_owned()]);
    assert_eq!(driver.team_roles("plan"), None);
    assert_eq!(driver.team_roles("nope"), None);
}

#[test]
fn code_audit_scopes_then_spawns_three_independent_lenses() {
    let spec = support::builtin("code-audit");
    assert_eq!(spec.file.pipeline.workspace, WorkspaceMode::Worktree);
    assert!(spec.file.pipeline.requires_repo);
    assert_eq!(spec.file.nudges.tier, Some(Tier::Strong));
    assert!(spec.file.nudges.independent_reviewers);
    let stages: Vec<_> = spec
        .file
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.kind, s.fan_out, s.gate))
        .collect();
    assert_eq!(
        stages,
        [
            ("scope", StageKind::Work, 1, Gate::None),
            ("audit", StageKind::Report, 3, Gate::None),
            ("report", StageKind::Report, 1, Gate::None),
        ]
    );
    assert!(spec.file.stages.iter().all(|s| s.on_fail.is_none()));
    let plan = support::plan_for(&spec, &["codex/astra", "codex/sol", "codex/luna"]);
    let audit_choices = plan.0["audit"].clone();
    let mut sim = Sim::new(spec, plan);
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("scope", 1, 0)]));
    assert!(
        spawn_plans(&first)[0]
            .prompt
            .contains(&sim.base["task.brief"])
    );
    let auditors = sim.settle("scope", 0, Agent::Done("scope map and safe test commands"));
    assert_eq!(
        spawned(&auditors),
        slots(&[("audit", 1, 0), ("audit", 1, 1), ("audit", 1, 2)])
    );
    for (index, auditor) in spawn_plans(&auditors).iter().enumerate() {
        let slot = index + 1;
        assert_eq!(auditor.choice, audit_choices[index]);
        assert!(
            auditor
                .prompt
                .contains(&format!("Code auditor {slot} of 3"))
        );
        assert!(
            auditor
                .prompt
                .contains(&format!("Your lens is row {slot} of `lenses.md`"))
        );
        assert!(auditor.prompt.contains(&format!(
            ".archon/reports/code-audit/auditor-{slot}/findings.md"
        )));
        assert!(auditor.prompt.contains("scope map and safe test commands"));
        assert!(auditor.prompt.contains(&sim.base["task.brief"]));
        assert!(!auditor.prompt.contains("{{"));
        assert!(auditor.instructions.contains("do not modify tracked files"));
    }
}

#[test]
fn code_audit_report_waits_for_all_lenses_and_keeps_their_summaries() {
    let mut sim = sim("code-audit");
    sim.start();
    sim.settle("scope", 0, Agent::Done("scope covers persistence and API"));
    assert!(
        sim.settle("audit", 2, Agent::Done("no maintainability findings"))
            .is_empty()
    );
    assert!(
        sim.settle(
            "audit",
            0,
            Agent::Done("high security finding with evidence")
        )
        .is_empty()
    );
    let report = sim.settle("audit", 1, Agent::Done("no reliability findings"));
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    let editor = spawn_plans(&report)[0];
    for summary in [
        "scope covers persistence and API",
        "[#1] high security finding with evidence",
        "[#2] no reliability findings",
        "[#3] no maintainability findings",
    ] {
        assert!(editor.prompt.contains(summary), "missing {summary}");
    }
    assert!(editor.prompt.contains(&sim.base["task.brief"]));
    assert!(editor.prompt.contains(&format!(
        "{}/.archon/reports/code-audit/index.html",
        sim.base["workspace"]
    )));
    assert!(editor.instructions.contains("\"reports\""));
    assert!(!editor.prompt.contains("{{"));
    let done = sim.settle("report", 0, Agent::Done("HTML report with one issue draft"));
    assert_eq!(succeeded(&done), "HTML report with one issue draft");
    assert!(spawned(&done).is_empty(), "nothing ships after the report");
}

#[test]
fn code_audit_auditor_failure_prevents_a_partial_report() {
    let mut sim = sim("code-audit");
    sim.start();
    sim.settle("scope", 0, Agent::Done("scope map"));
    sim.settle("audit", 0, Agent::Done("security findings"));
    assert!(
        sim.settle(
            "audit",
            1,
            Agent::Error("could not write reliability evidence")
        )
        .is_empty()
    );
    let done = sim.settle("audit", 2, Agent::Done("no maintainability findings"));
    let (stage, error) = failed(&done);
    assert_eq!(stage, "audit");
    assert!(
        error.contains("could not write reliability evidence"),
        "{error}"
    );
    assert!(
        spawned(&done).is_empty(),
        "no report from incomplete audits"
    );
}

#[test]
fn performance_gate_retries_preserve_baseline_and_reverify_before_shipping() {
    let mut sim = sim("performance");
    assert_eq!(spawned(&sim.start()), slots(&[("measure", 1, 0)]));
    let optimize = sim.settle("measure", 0, Agent::Done("baseline: 100 ms, target: 80 ms"));
    assert_eq!(spawned(&optimize), slots(&[("optimize", 1, 0)]));
    let prompt = &spawn_plans(&optimize)[0].prompt;
    assert!(prompt.contains("baseline: 100 ms"));

    let verify = sim.settle("optimize", 0, Agent::Done("first optimization"));
    assert_eq!(spawned(&verify), slots(&[("verify", 1, 0)]));
    assert!(
        spawn_plans(&verify)[0]
            .prompt
            .contains("first optimization")
    );
    let retry = sim.settle("verify", 0, Agent::Fail("gain is within noise"));
    assert_eq!(spawned(&retry), slots(&[("optimize", 2, 0)]));
    let prompt = &spawn_plans(&retry)[0].prompt;
    assert!(prompt.contains("gain is within noise"));
    assert!(prompt.contains("src/lib.rs:1"));
    assert!(prompt.contains("baseline: 100 ms, target: 80 ms"));

    let verify = sim.settle("optimize", 0, Agent::Done("second optimization"));
    assert_eq!(spawned(&verify), slots(&[("verify", 2, 0)]));
    let review = sim.settle(
        "verify",
        0,
        Agent::SummaryVerdict("measured: 75 ms, tests pass.\nVERDICT: PASS"),
    );
    assert_eq!(spawned(&review), slots(&[("review", 1, 0)]));
    assert!(spawn_plans(&review)[0].prompt.contains("measured: 75 ms"));
    let retry = sim.settle("review", 0, Agent::Fail("concurrent callers race"));
    assert_eq!(spawned(&retry), slots(&[("optimize", 3, 0)]));
    let prompt = &spawn_plans(&retry)[0].prompt;
    assert!(prompt.contains("concurrent callers race"));
    assert!(prompt.contains("baseline: 100 ms, target: 80 ms"));

    let verify = sim.settle("optimize", 0, Agent::Done("final optimization"));
    assert_eq!(spawned(&verify), slots(&[("verify", 3, 0)]));
    let review = sim.settle(
        "verify",
        0,
        Agent::SummaryVerdict("measured: 78 ms, tests pass.\nVERDICT: PASS"),
    );
    assert_eq!(spawned(&review), slots(&[("review", 2, 0)]));
    let prompt = &spawn_plans(&review)[0].prompt;
    assert!(prompt.contains("final optimization"));
    assert!(prompt.contains("measured: 78 ms"));
    assert!(!prompt.contains("measured: 75 ms"));

    let ship = sim.settle("review", 0, Agent::Pass);
    let plan = shipping(&ship);
    assert_eq!(plan.record_stage, "optimize");
    let body = &plan.body;
    assert!(body.contains("### `verify`\n\nmeasured: 78 ms"), "{body}");
    assert!(body.contains("### `review`"));
    assert!(!body.contains("measured: 75 ms"), "only the passes");
    succeeded(&sim.ship_pr(PR));
}

#[test]
fn performance_gates_exhaust_their_own_retry_budgets_without_shipping() {
    for (gate, max_loops) in [("verify", 3), ("review", 2)] {
        let mut sim = sim("performance");
        sim.start();
        sim.settle("measure", 0, Agent::Done("original baseline"));
        for attempt in 1..=max_loops + 1 {
            sim.settle("optimize", 0, Agent::Done("optimization attempt"));
            if gate == "review" {
                let review = sim.settle("verify", 0, Agent::Pass);
                assert_eq!(spawned(&review), slots(&[("review", attempt, 0)]));
            }
            let actions = sim.settle(gate, 0, Agent::Fail("requirement unmet"));
            if attempt <= max_loops {
                assert_eq!(spawned(&actions), slots(&[("optimize", attempt + 1, 0)]));
                assert!(
                    spawn_plans(&actions)[0]
                        .prompt
                        .contains("original baseline")
                );
                continue;
            }
            assert!(
                spawned(&actions).is_empty(),
                "no optimization or ship after exhaustion"
            );
            let (stage, error) = failed(&actions);
            assert_eq!(stage, gate);
            assert!(
                error.contains(&format!("after {max_loops} of {max_loops} fix loop(s)")),
                "{error}"
            );
            assert!(error.contains("requirement unmet"), "{error}");
            assert!(sim.stages.iter().all(|stage| stage.stage_id != "ship"));
        }
    }
}

#[test]
fn refactor_loops_preserve_baseline_and_reverify_before_shipping() {
    let mut sim = sim("refactor");
    let baseline = sim.start();
    assert_eq!(spawned(&baseline), slots(&[("baseline", 1, 0)]));
    assert!(
        spawn_plans(&baseline)[0]
            .prompt
            .contains("The widget breaks on empty input.")
    );

    let refactor = sim.settle("baseline", 0, Agent::Done("baseline reference: abc123"));
    assert_eq!(spawned(&refactor), slots(&[("refactor", 1, 0)]));
    assert!(
        spawn_plans(&refactor)[0]
            .prompt
            .contains("baseline reference: abc123")
    );
    let verify = sim.settle("refactor", 0, Agent::Done("first structure"));
    assert_eq!(spawned(&verify), slots(&[("verify", 1, 0)]));
    let prompt = &spawn_plans(&verify)[0].prompt;
    assert!(prompt.contains("baseline reference: abc123") && prompt.contains("first structure"));

    let retry = sim.settle("verify", 0, Agent::Fail("changed CLI output"));
    assert_eq!(spawned(&retry), slots(&[("refactor", 2, 0)]));
    let prompt = &spawn_plans(&retry)[0].prompt;
    assert!(prompt.contains("changed CLI output") && prompt.contains("src/lib.rs:1"));
    assert!(prompt.contains("baseline reference: abc123"));

    sim.settle("refactor", 0, Agent::Done("preserved output"));
    let review = sim.settle("verify", 0, Agent::Pass);
    assert_eq!(spawned(&review), slots(&[("review", 1, 0)]));
    let prompt = &spawn_plans(&review)[0].prompt;
    assert!(prompt.contains("baseline reference: abc123") && prompt.contains("preserved output"));
    assert!(prompt.contains("VERDICT: PASS") && !prompt.contains("first structure"));

    let retry = sim.settle("review", 0, Agent::Fail("unnecessary coupling"));
    assert_eq!(spawned(&retry), slots(&[("refactor", 3, 0)]));
    let prompt = &spawn_plans(&retry)[0].prompt;
    assert!(
        prompt.contains("unnecessary coupling") && prompt.contains("baseline reference: abc123")
    );

    let verify = sim.settle("refactor", 0, Agent::Done("simpler structure"));
    assert_eq!(spawned(&verify), slots(&[("verify", 3, 0)]));
    let review = sim.settle("verify", 0, Agent::Pass);
    assert_eq!(spawned(&review), slots(&[("review", 2, 0)]));
    let ship = sim.settle("review", 0, Agent::Pass);
    let plan = shipping(&ship);
    assert_eq!(plan.record_stage, "refactor");
    assert!(plan.body.contains("### `verify`") && plan.body.contains("### `review`"));
    assert!(plan.body.contains("VERDICT: PASS"));
    succeeded(&sim.ship_pr(PR));
    let baselines = sim.stages.iter().filter(|s| s.stage_id == "baseline");
    assert_eq!(baselines.count(), 1, "gate retries must not rebaseline");
}

#[test]
fn refactor_gates_enforce_their_own_fix_loop_limits() {
    for (gate, max_loops) in [("verify", 3), ("review", 2)] {
        let mut sim = sim("refactor");
        sim.start();
        sim.settle("baseline", 0, Agent::Done("original behaviour"));
        for attempt in 1..=max_loops + 1 {
            sim.settle("refactor", 0, Agent::Done("structural change"));
            if gate == "review" {
                sim.settle("verify", 0, Agent::Pass);
            }
            let actions = sim.settle(gate, 0, Agent::Fail("still not preserved"));
            if attempt <= max_loops {
                assert_eq!(spawned(&actions), slots(&[("refactor", attempt + 1, 0)]));
                continue;
            }
            assert!(spawned(&actions).is_empty(), "no ship or extra retry");
            let (stage, error) = failed(&actions);
            assert_eq!(stage, gate);
            assert!(
                error.contains(&format!("after {max_loops} of {max_loops} fix loop(s)")),
                "{error}"
            );
            assert!(error.contains("still not preserved"), "{error}");
        }
    }
}

#[test]
fn design_doc_passes_critique_then_finishes_with_a_report() {
    let mut sim = sim("design-doc");
    assert_eq!(spawned(&sim.start()), slots(&[("research", 1, 0)]));
    let critique = sim.settle("research", 0, Agent::Done("Recommend a minimal interface"));
    assert_eq!(spawned(&critique), slots(&[("critique", 1, 0)]));
    assert!(
        spawn_plans(&critique)[0]
            .prompt
            .contains("Recommend a minimal interface")
    );
    let report = sim.settle("critique", 0, Agent::Pass);
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    let prompt = &spawn_plans(&report)[0].prompt;
    assert!(prompt.contains("Recommend a minimal interface"));
    assert!(prompt.contains("Looks correct"), "critique forwarded");
    assert!(prompt.contains("/archon/worktrees/demo/w1/.archon/reports/design-doc/index.html"));
    assert!(!prompt.contains("{{"), "all placeholders rendered");
    assert_eq!(
        succeeded(&sim.settle("report", 0, Agent::Done("Published the design RFC"))),
        "Published the design RFC"
    );
}

#[test]
fn design_doc_critique_loops_with_feedback_and_reports_the_latest_design() {
    let mut sim = sim("design-doc");
    sim.start();
    for (attempt, draft, finding) in [
        (1, "First design draft", "missing migration plan"),
        (2, "Second design draft", "unsafe rollback assumption"),
    ] {
        let critique = sim.settle("research", 0, Agent::Done(draft));
        assert_eq!(spawned(&critique), slots(&[("critique", attempt, 0)]));
        assert!(spawn_plans(&critique)[0].prompt.contains(draft));
        let back = sim.settle("critique", 0, Agent::Fail(finding));
        assert_eq!(spawned(&back), slots(&[("research", attempt + 1, 0)]));
        let prompt = &spawn_plans(&back)[0].prompt;
        assert!(prompt.contains(finding), "finding forwarded: {prompt}");
        assert!(
            prompt.contains("src/lib.rs:1"),
            "finding evidence forwarded"
        );
        assert!(prompt.contains(&format!("attempt {}", attempt + 1)));
        assert!(finish_state(&back).is_none(), "retry remains active");
    }
    sim.settle(
        "research",
        0,
        Agent::Done("Final design with safe rollback"),
    );
    let report = sim.settle("critique", 0, Agent::Pass);
    assert_eq!(spawned(&report), slots(&[("report", 1, 0)]));
    let prompt = &spawn_plans(&report)[0].prompt;
    assert!(prompt.contains("Final design with safe rollback"));
    assert!(prompt.contains("Looks correct"));
    for superseded in [
        "First design draft",
        "Second design draft",
        "Found a problem",
    ] {
        assert!(!prompt.contains(superseded), "superseded input: {prompt}");
    }
    succeeded(&sim.settle("report", 0, Agent::Done("Published final RFC")));
}

#[test]
fn design_doc_exhausted_or_missing_critique_verdict_never_publishes_a_report() {
    for verdict in [Agent::Fail("unsupported recommendation"), Agent::NoVerdict] {
        let mut sim = sim("design-doc");
        sim.start();
        for attempt in 1..=3 {
            sim.settle("research", 0, Agent::Done("Design draft"));
            let actions = sim.settle("critique", 0, verdict.clone());
            if attempt < 3 {
                assert_eq!(spawned(&actions), slots(&[("research", attempt + 1, 0)]));
                continue;
            }
            assert!(spawned(&actions).is_empty(), "no report or third retry");
            let (stage, error) = failed(&actions);
            assert_eq!(stage, "critique");
            assert!(error.contains("after 2 of 2 fix loop(s)"), "{error}");
        }
    }
}

/// A lead's passed review as stored once Archon found its reviewer in Aether.
fn reviewed_by(model: &str) -> serde_json::Value {
    let mut result =
        serde_json::json!({"review": {"verdict": "pass", "reviewers": ["agt_reviewer"]}});
    let reviewer = Reviewer {
        model: model.into(),
        agent: "agt_reviewer".into(),
    };
    archon_pipeline::review::record_check(&mut result, Some(Ok(vec![reviewer])));
    result
}

/// A review holder's spawn carries the reviewer models its plan names, none
/// when it names none: the daemon advises within the policy current then.
#[test]
fn a_review_holder_carries_its_planned_reviewer_models() {
    let named = ModelChoice {
        model: "codex/astra".parse().unwrap(),
        effort: Effort::Low,
        tier: None,
    };
    let sim = sim("adaptive");
    let mut planned = sim.plan.clone();
    planned
        .0
        .insert(reviewers_key("build"), vec![named.clone()]);
    for (plan, carried) in [(sim.plan.clone(), Vec::new()), (planned, vec![named])] {
        let mut driver =
            RunDriver::new(sim.spec.clone(), plan, sim.base.clone()).expect("valid driver");
        let first = driver.start();
        let lead = spawn_plans(&first)[0];
        assert_eq!(lead.reviewers.as_ref(), Some(&carried));
        assert!(lead.allow_delegation, "reviewers are delegated");
        assert!(
            !lead.instructions.contains("Delegate every reviewer"),
            "{}",
            lead.instructions
        );
    }
}

#[test]
fn adaptive_keeps_one_build_lead_and_archond_ships_its_reviewed_change() {
    let mut sim = sim("adaptive");
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("build", 1, 0)]));
    let lead = spawn_plans(&first)[0];
    assert!(lead.allow_delegation);
    assert_eq!(lead.workspace, WorkspaceMode::Worktree);
    assert!(!lead.prompt.contains("{{"));
    // Archon adds the rules for children where it decides who delegates.
    assert!(
        lead.prompt
            .contains("Delegation section of your stage contract")
    );
    assert!(
        lead.prompt
            .contains("never to touch the operator's checkout at `")
    );
    assert!(lead.instructions.contains("## Review on record"));
    assert!(
        lead.instructions
            .contains("\"reviewers\": [\"<agent id>\"]")
    );
    assert!(lead.instructions.contains("## Ship record"));
    let ship = sim.settle(
        "build",
        0,
        Agent::DoneStructured("Integrated; checks pass.", reviewed_by("codex/astra")),
    );
    let plan = shipping(&ship);
    assert_eq!(plan.record_stage, "build");
    let review = "REVIEW: pass, by codex/astra (agent agt_reviewer)";
    assert_eq!(
        plan.summary,
        format!("Integrated; checks pass.\n\n{review}")
    );
    assert!(
        plan.body
            .starts_with(&format!("What changed and why.\n\n{review}")),
        "{}",
        plan.body
    );
    let done = sim.ship_pr(PR);
    assert!(succeeded(&done).ends_with(&format!("SHIPPED: {PR} (commit {})", sim::COMMIT)));
}

/// A lead that only says it reviewed has put nothing on record: the run
/// ends at `build`, and no ship agent is ever created.
#[test]
fn adaptive_fails_a_build_that_completed_without_a_review_on_record() {
    let unreviewed = [
        (
            Agent::Done("Integrated; checks pass; review PASS."),
            "no `review` object",
        ),
        (
            Agent::DoneStructured("Done.", serde_json::json!({"review": {"verdict": "fail"}})),
            "verdict is `fail`",
        ),
        (
            Agent::DoneStructured("Done.", serde_json::json!({"review": {"verdict": "pass"}})),
            "not checked its reviewers",
        ),
        (
            Agent::DoneStructured(
                "Done.",
                serde_json::json!({"review": {"verdict": "skipped"}}),
            ),
            "`review.reason`",
        ),
        (
            Agent::DoneStructured(
                "Done.",
                serde_json::json!({"review": {"verdict": "pass", "reviewers": ["agt_1"],
                    "reviewer_models": ["a/1"], "reviewer_agents": ["agt_1"],
                    "unverified": "reviewer agt_1 failed the change"}}),
            ),
            "reviewer agt_1 failed the change",
        ),
    ];
    for (outcome, why) in unreviewed {
        let mut sim = sim("adaptive");
        sim.start();
        let done = sim.settle("build", 0, outcome.clone());
        assert!(spawned(&done).is_empty(), "{outcome:?} reached ship");
        let (stage, error) = failed(&done);
        assert_eq!(stage, "build");
        assert!(error.contains("is not on record"), "{error}");
        assert!(error.contains(why), "{outcome:?}: {error}");
    }
}

/// Only a ship agent could judge whether a skip was warranted; archond
/// cannot, so the change is reviewed or the item asks for no review at all.
#[test]
fn adaptive_refuses_a_skipped_review_now_that_archond_ships() {
    let mut sim = sim("adaptive");
    sim.start();
    let skipped =
        serde_json::json!({"review": {"verdict": "skipped", "reason": "a typo in a comment"}});
    let done = sim.settle(
        "build",
        0,
        Agent::DoneStructured("Fixed the typo.", skipped),
    );
    assert!(ship_plans(&done).is_empty(), "nothing ships unreviewed");
    let (stage, error) = failed(&done);
    assert_eq!(stage, "build");
    assert!(error.contains("needs 1 independent reviewer(s)"), "{error}");
}

/// A lead of `pipeline` at review `level`, as the daemon builds its run.
fn sim_reviewed(pipeline: &str, level: ReviewLevel) -> Sim {
    let knobs = Autonomy {
        review: level,
        ..Autonomy::default()
    };
    let spec = Arc::new(augment(&support::builtin(pipeline), &knobs).expect("knobs apply"));
    let plan = support::plan_for(&spec, &["claude-code/opus"]);
    Sim::new(spec, plan)
}

/// A passed review by the `found` agents on `models`, as Archon checked it.
fn reviewed_by_all(models: &[&str], found: &[&str]) -> serde_json::Value {
    let mut result = serde_json::json!({"review": {"verdict": "pass", "reviewers": found}});
    let reviewers = models.iter().zip(found).map(|(model, agent)| Reviewer {
        model: (*model).to_owned(),
        agent: (*agent).to_owned(),
    });
    let reviewers: Vec<Reviewer> = reviewers.collect();
    // Archon checks no reviewer where the item's level owes none.
    let found = (!reviewers.is_empty()).then_some(Ok(reviewers));
    archon_pipeline::review::record_check(&mut result, found);
    result
}

/// The item's review level, not the pipeline, says how many independent
/// reviewers the lead owes: none, one, or three working alone.
#[test]
fn a_run_s_review_level_sets_how_many_reviewers_its_lead_owes() {
    for (level, owed) in [
        (ReviewLevel::None, 0),
        (ReviewLevel::Standard, 1),
        (ReviewLevel::Strict, 3),
    ] {
        let mut sim = sim_reviewed("adaptive", level);
        let first = sim.start();
        assert_eq!(sim.driver.reviewers_owed("build"), owed, "{level:?}");
        let contract = &spawn_plans(&first)[0].instructions;
        let said = match owed {
            0 => "review level is `none`: delegate no reviewer".to_owned(),
            1 => "reviewed by an agent you delegate to".to_owned(),
            n => format!("reviewed by {n} agents you delegate to, each working alone"),
        };
        assert!(contract.contains(&said), "{level:?}: {contract}");
        let models = ["a/1", "b/2", "c/3"];
        let agents = ["agt_1", "agt_2", "agt_3"];
        let result = reviewed_by_all(&models[..owed as usize], &agents[..owed as usize]);
        let done = sim.settle("build", 0, Agent::DoneStructured("Built.", result));
        let line = match owed {
            0 => "REVIEW: none, as this item's review level asks".to_owned(),
            1 => "REVIEW: pass, by a/1 (agent agt_1)".to_owned(),
            _ => "REVIEW: pass, by a/1 (agent agt_1), b/2 (agent agt_2), c/3 (agent agt_3)"
                .to_owned(),
        };
        assert_eq!(
            shipping(&done).summary,
            format!("Built.\n\n{line}"),
            "{level:?}"
        );
    }
}

/// A strict review that names fewer reviewers than Archon found for it, or
/// fewer than three, is not the review the item asked for.
#[test]
fn a_strict_review_with_too_few_reviewers_fails_the_run() {
    let mut sim = sim_reviewed("adaptive", ReviewLevel::Strict);
    sim.start();
    let two = reviewed_by_all(&["a/1", "b/2"], &["agt_1", "agt_2"]);
    let done = sim.settle("build", 0, Agent::DoneStructured("Built.", two));
    let (stage, error) = failed(&done);
    assert_eq!(stage, "build");
    assert!(error.contains("needs 3 independent reviewers"), "{error}");
    assert!(error.contains("the record names 2"), "{error}");
}

/// `direct` is one agent from start to shipped: no delegation, no review
/// section and no later stage, unless the item's review level asks for one.
#[test]
fn direct_runs_one_agent_and_archond_ships_what_it_recorded() {
    let mut sim = sim_reviewed("direct", ReviewLevel::None);
    let first = sim.start();
    assert_eq!(spawned(&first), slots(&[("implement", 1, 0)]));
    let agent = spawn_plans(&first)[0];
    assert!(!agent.allow_delegation, "a leaf at review level none");
    assert!(!agent.instructions.contains("## Review on record"));
    assert!(agent.instructions.contains("## Ship record"));
    assert!(!agent.prompt.contains("{{"));
    let done = sim.settle("implement", 0, Agent::Done("Fixed the typo; tests pass."));
    let plan = shipping(&done);
    assert_eq!(plan.record_stage, "implement");
    assert!(
        plan.summary
            .ends_with("REVIEW: none, as this item's review level asks")
    );
    assert!(succeeded(&sim.ship_pr(PR)).contains("Fixed the typo; tests pass."));
    assert_eq!(sim.stages.len(), 1, "one agent ran, end to end");

    let mut reviewed = sim_reviewed("direct", ReviewLevel::Standard);
    let first = reviewed.start();
    let agent = spawn_plans(&first)[0];
    assert!(agent.allow_delegation, "it delegates its one reviewer");
    assert!(agent.instructions.contains("## Review on record"));
    let unreviewed = reviewed.settle("implement", 0, Agent::Done("Fixed."));
    assert!(failed(&unreviewed).1.contains("no `review` object"));
}

/// The record names what ships, so a record stage that leaves none has
/// delivered nothing archond could check.
#[test]
fn a_record_stage_without_its_ship_record_fails_the_run() {
    let mut sim = sim("direct");
    sim.writes_record = false;
    sim.start();
    let done = sim.settle("implement", 0, Agent::Done("Fixed it."));
    assert!(ship_plans(&done).is_empty());
    let (stage, error) = failed(&done);
    assert_eq!(stage, "implement");
    assert!(
        error.contains("the ship record archond delivers is not on record"),
        "{error}"
    );
    assert!(error.contains("no `structured` object"), "{error}");
}

/// A task that needs no change succeeds honestly, with no review owed and
/// nothing shipped; a ship record beside the claim still ships.
#[test]
fn a_record_stage_that_reports_no_change_succeeds_without_shipping() {
    let mut sim = sim_reviewed("direct", ReviewLevel::Standard);
    sim.writes_record = false;
    sim.start();
    let claim = serde_json::json!({"no_change": {"reason": "the typo is already fixed"}});
    let done = sim.settle("implement", 0, Agent::DoneStructured("Checked.", claim));
    assert!(ship_plans(&done).is_empty(), "nothing is delivered");
    let summary = succeeded(&done);
    assert!(summary.contains("No change was needed, so nothing was shipped"));
    assert!(summary.contains("the typo is already fixed"), "{summary}");

    let mut sim = sim_reviewed("direct", ReviewLevel::None);
    sim.writes_record = false;
    sim.start();
    let empty = serde_json::json!({"no_change": {"reason": "  "}});
    let done = sim.settle("implement", 0, Agent::DoneStructured("Checked.", empty));
    assert!(failed(&done).1.contains("is not on record"));

    let mut sim = sim_reviewed("direct", ReviewLevel::None);
    sim.start();
    let both = serde_json::json!({"no_change": {"reason": "nothing to do"}});
    let done = sim.settle("implement", 0, Agent::DoneStructured("Fixed.", both));
    assert_eq!(ship_plans(&done).len(), 1, "a record beats the claim");
}

/// A delivery archond refused ends the run at the ship step, with its
/// reason as the failure the orchestrator reads.
#[test]
fn a_failed_delivery_fails_the_run_at_the_ship_step() {
    let mut sim = sim("direct");
    sim.start();
    sim.settle("implement", 0, Agent::Done("Fixed it."));
    let refused = ShipOutcome::Failed {
        error: "the worktree's tree is not the recorded one".into(),
    };
    let done = sim.ship(refused);
    let (stage, error) = failed(&done);
    assert_eq!(stage, "ship", "no agent ran it: archond did");
    assert_eq!(error, "the worktree's tree is not the recorded one");
}

/// A recorded delivery survives a restart and is never replaced: the run
/// is delivered once, whatever a second attempt would say.
#[test]
fn a_delivery_is_recorded_once_and_replays_after_a_restart() {
    let mut answering = sim("ask");
    let mut sim = sim("direct");
    sim.start();
    sim.settle("implement", 0, Agent::Done("Fixed it."));
    let waiting = sim.resume_fresh();
    assert_eq!(
        ship_plans(&waiting).len(),
        1,
        "a restart re-asks for delivery"
    );
    let shipped = ShipOutcome::Shipped {
        commit: sim::COMMIT.into(),
        pr_url: Some(PR.into()),
        updated: false,
        risk: None,
    };
    let mut driver = sim.resumed_driver();
    driver.record_ship(shipped.clone()).expect("recorded once");
    driver
        .record_ship(shipped.clone())
        .expect("the same again is a no-op");
    let other = ShipOutcome::Failed { error: "e".into() };
    let refused = driver.record_ship(other).unwrap_err().to_string();
    assert!(refused.contains("already recorded"), "{refused}");
    succeeded(&driver.resume(&sim.run, &sim.stages));

    answering.start();
    let refused = answering
        .driver
        .record_ship(shipped)
        .unwrap_err()
        .to_string();
    assert!(refused.contains("is not shipped by archond"), "{refused}");
}

/// A task that does not fit its pipeline goes back to the orchestrator at
/// once: no fix loop, no retry, and the stage's recommendation as the error.
#[test]
fn an_escalation_ends_the_run_without_a_fix_loop() {
    let mut sim = sim("reviewed");
    sim.start();
    sim.settle("implement", 0, Agent::Done("first try"));
    sim.settle("review", 0, Agent::Fail("off by one"));
    let escalate = "ESCALATE: this needs a schema migration; use adaptive";
    let done = sim.settle("implement", 0, Agent::Error(escalate));
    assert!(spawned(&done).is_empty(), "nothing is retried");
    let (stage, error) = failed(&done);
    assert_eq!(stage, "implement");
    assert!(error.contains(escalate), "{error}");
}

#[test]
fn adaptive_never_ships_a_failed_or_blocked_build() {
    for outcome in [
        Agent::Error("review failed after two rounds"),
        Agent::Blocked("review unavailable"),
    ] {
        let mut sim = sim("adaptive");
        sim.start();
        let done = sim.settle("build", 0, outcome);
        assert!(spawned(&done).is_empty());
        assert!(sim.finished().is_some());
        assert!(!matches!(
            finish_state(&done),
            Some(RunState::Succeeded { .. })
        ));
    }
}

/// The `swarm` built-in, started: its plan stage is live.
fn swarm_sim() -> Sim {
    let mut sim = sim("swarm");
    assert_eq!(spawned(&sim.start()), slots(&[("plan", 1, 0)]));
    sim
}

/// The roles a plan of `tasks` gives `build`, recorded as archond does
/// before the plan stage's settle reaches the rebuilt driver.
fn assign_tasks(sim: &mut Sim, tasks: &[&str]) {
    let edit = archon_core::steer::RunEdit::AssignRoles {
        stage: "build".into(),
        roles: tasks.iter().map(|t| (*t).to_owned()).collect(),
        role_prompts: tasks
            .iter()
            .map(|t| ((*t).to_owned(), format!("You build task `{t}`.")))
            .collect(),
    };
    let spec = archon_pipeline::reshape::apply(&sim.spec, &edit).expect("assignable");
    let plan = sim.plan.clone();
    assert!(
        sim.reshape(spec, plan).is_empty(),
        "the planner is still live"
    );
}

#[test]
fn a_swarm_plan_that_assigns_no_roles_fails_the_run_at_the_team() {
    let mut sim = swarm_sim();
    let done = sim.settle("plan", 0, Agent::Done("planned"));
    assert!(spawned(&done).is_empty(), "no worker without a task");
    let (stage, error) = failed(&done);
    assert_eq!(stage, "build");
    assert!(
        error.contains("stage `plan` passed without the swarm plan"),
        "{error}"
    );
}

#[test]
fn a_swarm_spawns_one_worker_per_planned_task_with_its_task_only() {
    let mut sim = swarm_sim();
    assign_tasks(&mut sim, &["api", "cli", "docs"]);
    let split = "api: PRIVATE_API in api/. cli: PRIVATE_CLI in cli/.";
    let build = sim.settle("plan", 0, Agent::Done(split));
    assert_eq!(
        spawned(&build),
        slots(&[("build", 1, 0), ("build", 1, 1), ("build", 1, 2)])
    );
    for (plan, task) in spawn_plans(&build).iter().zip(["api", "cli", "docs"]) {
        let slot = plan.team.as_ref().expect("a worker is a team slot");
        assert_eq!(slot.role, task);
        assert!(slot.workspace.ends_with(format!("build-{task}")));
        assert_eq!(plan.fan_count, 3, "one worker per task");
        assert!(plan.prompt.contains(&format!("You build task `{task}`.")));
        assert!(plan.prompt.contains("Build task `"), "{}", plan.prompt);
        assert!(!plan.prompt.contains("{{"), "{}", plan.prompt);
        assert!(!plan.prompt.contains("PRIVATE_"), "{}", plan.prompt);
        let others = ["api", "cli", "docs"].into_iter().filter(|t| *t != task);
        for other in others {
            let theirs = format!("You build task `{other}`.");
            assert!(!plan.prompt.contains(&theirs), "{}", plan.prompt);
        }
    }
}

/// The integrator sends tasks back twice at most: a third failure ends the run.
#[test]
fn a_swarm_re_dispatches_failed_tasks_at_most_twice() {
    let mut sim = swarm_sim();
    assign_tasks(&mut sim, &["api", "cli"]);
    sim.settle("plan", 0, Agent::Done("api, cli"));
    for attempt in 1..=3 {
        sim.settle("build", 0, Agent::Done("api built"));
        let merge = sim.settle("build", 1, Agent::Done("cli built"));
        assert_eq!(sim::merge_plans(&merge)[0].attempt, attempt);
        let integrate = sim.merge(MergeOutcome::Merged);
        assert_eq!(spawned(&integrate), slots(&[("integrate", attempt, 0)]));
        let prompt = &spawn_plans(&integrate)[0].prompt;
        assert!(prompt.contains(&format!("This is attempt {attempt} of at most 3.")));
        let back = sim.settle(
            "integrate",
            0,
            Agent::Error("REDISPATCH: cli\ncli: test fails"),
        );
        if attempt == 3 {
            let (stage, error) = failed(&back);
            assert_eq!(stage, "integrate");
            assert!(error.contains("after 2 of 2 fix loop(s)"), "{error}");
            return;
        }
        let rebuilt = spawn_plans(&back);
        assert_eq!(
            spawned(&back),
            slots(&[("build", attempt + 1, 0), ("build", attempt + 1, 1)])
        );
        assert!(
            rebuilt[1].prompt.contains("REDISPATCH: cli"),
            "{}",
            rebuilt[1].prompt
        );
        let peers_report = "api built";
        assert!(
            !rebuilt[1].prompt.contains(peers_report),
            "{}",
            rebuilt[1].prompt
        );
    }
    unreachable!("the third failure ends the run");
}

/// An integrator that fails without naming tasks sends its workers no
/// report of other tasks: they change nothing and complete.
#[test]
fn a_swarm_integrator_failing_without_redispatch_tells_workers_nothing_else() {
    let mut sim = swarm_sim();
    assign_tasks(&mut sim, &["api", "cli"]);
    sim.settle("plan", 0, Agent::Done("api, cli"));
    sim.settle("build", 0, Agent::Done("api built"));
    sim.settle("build", 1, Agent::Done("cli built"));
    sim.merge(MergeOutcome::Merged);
    let unrecorded = "api: PRIVATE_API done; cli: PRIVATE_CLI done.";
    let back = sim.settle("integrate", 0, Agent::Done(unrecorded));
    assert_eq!(spawned(&back), slots(&[("build", 2, 0), ("build", 2, 1)]));
    for worker in spawn_plans(&back) {
        assert!(!worker.prompt.contains("PRIVATE_"), "{}", worker.prompt);
        assert!(
            worker.prompt.contains("no task is yours to redo"),
            "{}",
            worker.prompt
        );
    }
}

#[test]
fn a_swarm_integrator_ships_its_reviewed_change() {
    let mut sim = swarm_sim();
    assign_tasks(&mut sim, &["api", "cli"]);
    sim.settle("plan", 0, Agent::Done("api, cli"));
    sim.settle("build", 0, Agent::Done("api built"));
    sim.settle("build", 1, Agent::Done("cli built"));
    let integrate = sim.merge(MergeOutcome::Merged);
    let lead = spawn_plans(&integrate)[0];
    assert!(
        lead.allow_delegation,
        "the integrator delegates its reviewers"
    );
    assert!(lead.instructions.contains("## Review on record"));
    assert!(
        lead.prompt.contains("[role cli] cli built"),
        "{}",
        lead.prompt
    );
    let ship = sim.settle(
        "integrate",
        0,
        Agent::DoneStructured("Integrated; checks pass.", reviewed_by("codex/astra")),
    );
    assert_eq!(shipping(&ship).record_stage, "integrate");
    let done = sim.ship_pr(PR);
    assert!(succeeded(&done).contains(&format!("SHIPPED: {PR}")));
}
