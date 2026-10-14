//! Steering a run (ARCHITECTURE §6.6, §6.7): aborts, edits to the stages
//! that have not started, the agent kept through a fix loop, and the digest
//! a stage is handed. Every scenario runs through `sim::Sim`, so each step
//! also holds for a driver rebuilt from persisted state.

mod sim;
mod support;

use std::sync::Arc;

use archon_core::run::RunState;
use archon_core::steer::{RunEdit, Then};
use archon_pipeline::RunDriver;
use archon_pipeline::reshape::{apply, apply_all, insert_edit, started_prefix, switch_edit};
use archon_pipeline::spec::PipelineSpec;
use sim::{Agent, Sim, finish_state, ship_plans, slots, spawn_plans, spawned};

const FANS: [&str; 2] = ["claude-code/opus", "codex/astra"];

fn sim(name: &str) -> Sim {
    let spec = support::builtin(name);
    let plan = support::plan_for(&spec, &FANS);
    Sim::new(spec, plan)
}

fn ids(spec: &PipelineSpec) -> Vec<&str> {
    spec.file.stages.iter().map(|s| s.id.as_str()).collect()
}

/// The stage whose ship record archond delivers for a run on `spec`.
fn recorder(spec: &PipelineSpec) -> Option<&str> {
    spec.ship_record_stage().map(|stage| stage.id.as_str())
}

fn refusal<T: std::fmt::Debug>(result: Result<T, archon_core::error::ArchonError>) -> String {
    result.expect_err("refused").to_string()
}

#[test]
fn an_inserted_stage_lands_after_the_started_ones_and_replays_from_the_edit_alone() {
    let run = support::builtin("quick-pr");
    let source = support::builtin("default-pr-review");
    let prefix = started_prefix(&run, ["implement"]);
    assert_eq!(prefix, 1);
    let (edit, reshaped) = insert_edit(&run, prefix, &source, "review", None).unwrap();
    assert_eq!(ids(&reshaped), ["implement", "review"]);
    assert_eq!(reshaped.name(), "quick-pr", "the run keeps its snapshot");
    assert_eq!(recorder(&reshaped), Some("implement"), "it still ships");
    let review = reshaped.stage("review").unwrap();
    assert_eq!(review.on_fail.as_ref().unwrap().goto, "implement");
    assert_eq!(
        reshaped.prompts[&review.prompt],
        source.prompts[&source.stage("review").unwrap().prompt]
    );

    // A restart has only the snapshot and the recorded edit.
    let stored = serde_json::to_string(&edit).unwrap();
    let replayed: RunEdit = serde_json::from_str(&stored).unwrap();
    assert_eq!(apply(&run, &replayed).unwrap(), reshaped);
    let RunEdit::InsertStage { after, .. } = &edit else {
        panic!("an insert is recorded as one: {edit:?}");
    };
    assert_eq!(after.as_deref(), Some("implement"));
}

#[test]
fn inserting_inside_the_started_stages_is_refused() {
    let run = support::builtin("default-pr-review");
    let source = support::builtin("strict-pr-review");
    let prefix = started_prefix(&run, ["implement", "review"]);
    assert_eq!(prefix, 2);
    let early = refusal(insert_edit(
        &run,
        prefix,
        &source,
        "test",
        Some("implement"),
    ));
    assert!(early.contains("\"review\" already started"), "{early}");
    assert!(early.contains("insert after \"review\""), "{early}");
    let unknown = refusal(insert_edit(&run, prefix, &source, "lint", None));
    assert!(unknown.contains("has no stage \"lint\""), "{unknown}");
    let nowhere = refusal(insert_edit(&run, prefix, &source, "test", Some("deploy")));
    assert!(
        nowhere.contains("the run has no stage \"deploy\""),
        "{nowhere}"
    );
    // A work stage after the ship record would commit what nobody records.
    let bugfix = support::builtin("bugfix");
    let unrecorded = refusal(insert_edit(&run, prefix, &bugfix, "reproduce", None));
    assert!(
        unrecorded.contains("the run could not continue"),
        "{unrecorded}"
    );
    assert!(unrecorded.contains("can still commit"), "{unrecorded}");

    let (_, ok) = insert_edit(&run, prefix, &source, "test", None).unwrap();
    assert_eq!(ids(&ok), ["implement", "review", "test"]);
}

/// The stage a pipeline has archond ship from keeps that role in a run it is
/// carried into, once it runs after the stage the run shipped from so far.
#[test]
fn a_carried_stage_that_wrote_the_ship_record_writes_the_run_s() {
    let run = support::builtin("quick-pr");
    let docs = support::builtin("docs");
    assert_eq!(
        (recorder(&run), recorder(&docs)),
        (Some("implement"), Some("write"))
    );
    let (edit, written) = insert_edit(&run, 1, &docs, "write", None).unwrap();
    assert_eq!(ids(&written), ["implement", "write"]);
    assert_eq!(recorder(&written), Some("write"), "the last commits ship");
    assert_eq!(apply(&run, &edit).unwrap(), written, "a restart agrees");

    let (edit, switched) = switch_edit(&run, 1, &support::builtin("team-pr")).unwrap();
    assert_eq!(
        ids(&switched),
        ["implement", "plan", "build", "e2e", "review"]
    );
    assert_eq!(recorder(&switched), Some("review"));
    assert_eq!(apply_all((*run).clone(), [&edit]).unwrap(), switched);

    // Ahead of the stage that ships, the carried one records nothing.
    let review = support::builtin("default-pr-review");
    let (_, early) = insert_edit(&review, 0, &docs, "write", None).unwrap();
    assert_eq!(ids(&early), ["write", "implement", "review"]);
    assert_eq!(recorder(&early), Some("implement"));

    // A pipeline that ships nothing leaves a run nothing to ship either.
    let audit = support::builtin("code-audit");
    assert_eq!(recorder(&audit), None);
    let (_, report) = switch_edit(&run, 0, &audit).unwrap();
    assert_eq!((ids(&report), recorder(&report)), (ids(&audit), None));
}

#[test]
fn a_clashing_id_or_prompt_gets_a_free_name() {
    let run = support::builtin("quick-pr");
    let source = support::builtin("default-pr-review");
    let theirs = &source.prompts[&source.stage("implement").unwrap().prompt];
    let ours = &run.prompts[&run.stage("implement").unwrap().prompt];
    assert_ne!(theirs, ours, "the two pipelines word the stage differently");

    let (_, reshaped) = insert_edit(&run, 1, &source, "implement", None).unwrap();
    assert_eq!(ids(&reshaped), ["implement", "implement-2"]);
    assert_eq!(recorder(&reshaped), Some("implement-2"));
    let second = reshaped.stage("implement-2").unwrap();
    assert_eq!(second.prompt, "implement-2-steered.md");
    assert_eq!(&reshaped.prompts[&second.prompt], theirs);
    assert_eq!(&reshaped.prompts["implement.md"], ours, "ours is untouched");

    let (_, again) = insert_edit(&reshaped, 1, &source, "implement", None).unwrap();
    assert_eq!(ids(&again), ["implement", "implement-3", "implement-2"]);
    assert_eq!(recorder(&again), Some("implement-2"), "the later one");
    let third = again.stage("implement-3").unwrap();
    assert_eq!(&again.prompts[&third.prompt], theirs);
}

#[test]
fn a_switch_keeps_started_stages_and_takes_the_rest_of_the_new_pipeline() {
    let run = support::builtin("quick-pr");
    let target = support::builtin("strict-pr-review");
    let (edit, reshaped) = switch_edit(&run, 1, &target).unwrap();
    assert_eq!(ids(&reshaped), ["implement", "test", "review"]);
    assert_eq!(reshaped.name(), "quick-pr");
    let kept = reshaped.stage("implement").unwrap();
    assert_eq!(
        kept,
        run.stage("implement").unwrap(),
        "history is not rewritten"
    );
    assert_eq!(recorder(&reshaped), Some("implement"));
    let review = reshaped.stage("review").unwrap();
    assert_eq!(
        reshaped.prompts[&review.prompt],
        target.prompts[&target.stage("review").unwrap().prompt],
        "an unstarted stage is the new pipeline's"
    );
    let RunEdit::SwitchPipeline { pipeline, stages } = &edit else {
        panic!("a switch is recorded as one: {edit:?}");
    };
    assert_eq!((pipeline.as_str(), stages.len()), ("strict-pr-review", 3));
    assert_eq!(stages[0], serde_json::json!({"keep": "implement"}));
    assert_eq!(apply_all((*run).clone(), [&edit]).unwrap(), reshaped);

    // Nothing started: the run takes the new pipeline whole.
    let (_, whole) = switch_edit(&run, 0, &target).unwrap();
    assert_eq!(ids(&whole), ids(&target));
}

#[test]
fn a_switch_to_another_workspace_or_with_nothing_left_is_refused() {
    let run = support::builtin("quick-pr");
    let other = refusal(switch_edit(&run, 1, &support::builtin("ask")));
    assert!(other.contains("a run keeps its workspace"), "{other}");
    let run = support::builtin("default-pr-review");
    let done = started_prefix(&run, ["review"]);
    assert_eq!(done, 2, "every stage up to the last started one");
    let target = support::builtin("default-pr-review");
    let nothing = refusal(switch_edit(&run, done, &target));
    assert!(
        nothing.contains("nothing left to run after stage \"review\""),
        "{nothing}"
    );
}

#[test]
fn a_run_continues_on_an_inserted_stage_and_loops_through_it() {
    let mut sim = sim("quick-pr");
    sim.start();
    let source = support::builtin("default-pr-review");
    let (_, reshaped) = insert_edit(&sim.spec, 1, &source, "review", None).unwrap();
    let plan = support::plan_for(&reshaped, &FANS);
    let owed = sim.reshape(reshaped, plan);
    assert!(owed.is_empty(), "the running stage goes on untouched");

    let review = sim.settle("implement", 0, Agent::Done("first try"));
    assert_eq!(spawned(&review), slots(&[("review", 1, 0)]));
    let back = sim.settle("review", 0, Agent::Fail("misses the empty case"));
    assert_eq!(spawned(&back), slots(&[("implement", 2, 0)]));
    assert!(
        spawn_plans(&back)[0]
            .prompt
            .contains("misses the empty case")
    );
    sim.settle("implement", 0, Agent::Done("handles the empty case"));
    let ship = sim.settle("review", 0, Agent::Pass);
    assert_eq!(ship_plans(&ship).len(), 1, "archond ships what passed");
    assert_eq!(ship_plans(&ship)[0].record_stage, "implement");
    let done = sim.ship_pr("https://github.com/o/r/pull/1");
    assert!(matches!(
        finish_state(&done),
        Some(RunState::Succeeded { .. })
    ));
}

#[test]
fn a_switch_mid_run_runs_what_the_new_pipeline_has_left() {
    let mut sim = sim("quick-pr");
    sim.start();
    let target = support::builtin("strict-pr-review");
    let (_, reshaped) = switch_edit(&sim.spec, 1, &target).unwrap();
    let plan = support::plan_for(&reshaped, &FANS);
    sim.reshape(reshaped, plan);
    let test = sim.settle("implement", 0, Agent::Done("built"));
    assert_eq!(spawned(&test)[0], ("test".to_owned(), 1, 0));
    let review = sim.settle("test", 0, Agent::Pass);
    let reviewers = sim.spec.stage("review").unwrap().fan_out;
    assert_eq!(spawned(&review).len(), reviewers as usize);
    assert!(spawned(&review).iter().all(|slot| slot.0 == "review"));
}

#[test]
fn an_aborted_stage_is_retried_by_a_fresh_agent() {
    let mut sim = sim("default-pr-review");
    sim.start();
    let again = sim.abort("implement", 1, Then::Retry);
    assert_eq!(spawned(&again), slots(&[("implement", 2, 0)]));
    let plan = spawn_plans(&again)[0];
    assert!(plan.resume.is_none(), "an aborted agent is not resumed");
    assert!(plan.keep, "the new one is kept for the fix loop");
    let review = sim.settle("implement", 0, Agent::Done("second go"));
    assert_eq!(spawned(&review), slots(&[("review", 1, 0)]));
}

#[test]
fn an_aborted_stage_is_skipped_and_leaves_a_note_where_its_summary_would_be() {
    let mut sim = sim("strict-pr-review");
    sim.start();
    sim.settle("implement", 0, Agent::Done("built it"));
    let review = sim.abort("test", 1, Then::Skip);
    assert!(spawned(&review).iter().all(|slot| slot.0 == "review"));
    let prompt = &spawn_plans(&review)[0].prompt;
    assert!(prompt.contains("skipped by a steer"), "{prompt}");
    assert!(
        prompt.contains("built it"),
        "earlier work is still handed on"
    );
    for fan in 0..spawned(&review).len() as u32 {
        sim.settle("review", fan, Agent::Pass);
    }
    // The pull request says so too: nobody reads it as a stage that passed.
    let body = &sim.open_ship.as_ref().expect("archond ships it").body;
    assert!(
        body.contains("### `test`\n\n(This stage was skipped by a steer"),
        "{body}"
    );
    assert!(finish_state(&sim.ship_pr("https://github.com/o/r/pull/1")).is_some());
}

#[test]
fn an_abort_can_send_the_run_back_with_a_fresh_loop_budget() {
    let mut sim = sim("default-pr-review");
    sim.start();
    sim.settle("implement", 0, Agent::Done("one"));
    sim.settle("review", 0, Agent::Fail("first finding"));
    sim.settle("implement", 0, Agent::Done("two"));
    sim.settle("review", 0, Agent::Fail("second finding"));
    sim.settle("implement", 0, Agent::Done("three"));
    // Two of two fix loops are spent; a failed third review would end the run.
    let to = "implement".to_owned();
    let back = sim.abort("review", 3, Then::Goto { to });
    assert_eq!(spawned(&back), slots(&[("implement", 4, 0)]));
    assert!(spawn_plans(&back)[0].resume.is_none(), "no findings to fix");
    sim.settle("implement", 0, Agent::Done("four"));
    let looped = sim.settle("review", 0, Agent::Fail("third finding"));
    assert_eq!(spawned(&looped), slots(&[("implement", 5, 0)]));
}

#[test]
fn a_removed_stage_is_stepped_over_every_time_the_run_reaches_it() {
    let mut sim = sim("strict-pr-review");
    sim.start();
    let nothing = sim.abort("test", 0, Then::Skip);
    assert!(nothing.is_empty(), "the running stage goes on untouched");
    let review = sim.settle("implement", 0, Agent::Done("built"));
    assert!(spawned(&review).iter().all(|slot| slot.0 == "review"));
    assert!(
        spawn_plans(&review)[0]
            .prompt
            .contains("skipped by a steer")
    );
    let fans = spawned(&review).len() as u32;
    for fan in 1..fans {
        sim.settle("review", fan, Agent::Pass);
    }
    let back = sim.settle("review", 0, Agent::Fail("no tests"));
    assert_eq!(spawned(&back), slots(&[("implement", 2, 0)]));
    let again = sim.settle("implement", 0, Agent::Done("tested"));
    assert!(
        spawned(&again)
            .iter()
            .all(|slot| slot.0 == "review" && slot.1 == 2),
        "the removed stage stays removed after a loop: {again:#?}"
    );
}

#[test]
fn aborts_the_pipeline_cannot_do_are_refused() {
    let spec = support::builtin("default-pr-review");
    let plan = support::plan_for(&spec, &FANS);
    let run = archon_core::id::PipelineRunId::new();
    let item = archon_core::id::QueueItemId::new();
    let vars = support::base_vars_for(&spec, &run, &item);
    let mut driver = RunDriver::new(spec, plan, vars).unwrap();
    let to = |stage: &str| Then::Goto { to: stage.into() };
    let unknown = refusal(driver.record_abort("lint", 1, Then::Retry));
    assert!(
        unknown.contains("has no stage \"lint\" to abort"),
        "{unknown}"
    );
    let last = refusal(driver.record_abort("review", 1, Then::Skip));
    assert!(last.contains("cancel the item instead"), "{last}");
    let recorder = refusal(driver.record_abort("implement", 1, Then::Skip));
    assert!(recorder.contains("writes the ship record"), "{recorder}");
    let nowhere = refusal(driver.record_abort("review", 1, to("deploy")));
    assert!(
        nowhere.contains("has no stage \"deploy\" to go to"),
        "{nowhere}"
    );
    let unstarted = refusal(driver.record_abort("review", 0, Then::Retry));
    assert!(unstarted.contains("it can only be skipped"), "{unstarted}");
    driver.record_abort("review", 2, to("implement")).unwrap();
}

#[test]
fn the_implementing_agent_is_kept_and_resumed_with_the_findings() {
    let mut sim = sim("default-pr-review");
    let first = sim.start();
    let implement = spawn_plans(&first)[0];
    assert!(implement.keep, "a later stage loops back to it");
    assert!(implement.resume.is_none(), "a first attempt starts cold");
    let review = sim.settle("implement", 0, Agent::Done("first try"));
    assert!(!spawn_plans(&review)[0].keep, "reviewers stay fresh");

    let back = sim.settle("review", 0, Agent::Fail("off-by-one in window end"));
    let fix = spawn_plans(&back)[0];
    assert!(fix.keep);
    let message = fix.resume.as_deref().expect("the kept agent is resumed");
    assert!(
        message.contains("stage `implement`, attempt 2"),
        "{message}"
    );
    assert!(message.contains("off-by-one in window end"), "{message}");
    assert!(message.contains("do not call `complete_task`"), "{message}");
    // Its task took `structured` once, so the new ship record comes in the message.
    assert!(message.contains("\"commit\": \"<git rev-parse HEAD>\""));
    assert!(message.contains("as its last fenced block"), "{message}");
    assert!(
        fix.prompt
            .contains("`implement`, your stage's last attempt"),
        "a fresh agent would get a digest of the attempt it replaces"
    );
    assert!(fix.prompt.contains("first try"));

    let quick = self::sim("quick-pr").start();
    assert!(!spawn_plans(&quick)[0].keep, "nothing loops back to it");
}

/// Three work stages in a row, with no gate.
const RELAY: &str = r#"
    [pipeline]
    name = "relay"
    description = "d"
    version = 1
    [[stage]]
    id = "survey"
    kind = "work"
    prompt = "survey.md"
    [[stage]]
    id = "build"
    kind = "work"
    prompt = "build.md"
    [[stage]]
    id = "polish"
    kind = "work"
    prompt = "polish.md"
"#;

#[test]
fn a_stage_is_handed_a_bounded_digest_of_what_earlier_stages_reported() {
    let prompts = [
        (
            "build.md",
            "Build it. Survey said: {{previous.survey.summary}}",
        ),
        ("polish.md", "Polish {{task.title}}."),
    ];
    let spec = Arc::new(support::spec_from(RELAY, &prompts));
    let plan = support::plan_for(&spec, &[]);
    let mut sim = Sim::new(spec, plan);
    let first = sim.start();
    assert!(!spawn_plans(&first)[0].prompt.contains("## Handoff"));

    let build = sim.settle(
        "survey",
        0,
        Agent::Done("The parser lives in src/parse.rs."),
    );
    let prompt = &spawn_plans(&build)[0].prompt;
    assert!(prompt.contains("Survey said: The parser lives in src/parse.rs."));
    assert!(
        !prompt.contains("## Handoff"),
        "its template already shows it"
    );

    let long: &'static str = Box::leak("changed src/parse.rs; ".repeat(400).into_boxed_str());
    let polish = sim.settle("build", 0, Agent::Done(long));
    let prompt = &spawn_plans(&polish)[0].prompt;
    let digest = prompt
        .split("## Handoff from earlier stages")
        .nth(1)
        .unwrap();
    assert!(digest.contains("### `survey`") && digest.contains("### `build`"));
    assert!(digest.contains("The parser lives in src/parse.rs."));
    assert!(digest.contains("changed src/parse.rs;"));
    assert!(
        digest.chars().count() < 2_000,
        "one stage's share is capped"
    );
    let (survey, build) = (digest.find("`survey`"), digest.find("`build`"));
    assert!(survey < build, "in pipeline order");
}
