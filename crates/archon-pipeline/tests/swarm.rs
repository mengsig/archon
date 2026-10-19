//! Swarm plans: the schema a planner's `.archon/swarm-plan.json` keeps, and
//! the roles and prompts it gives the `swarm` pipeline's team stage.

use archon_core::catalog::StageKind;
use archon_core::steer::RunEdit;
use archon_pipeline::swarm::{self, SwarmPlan};
use archon_pipeline::{Catalog, PipelineSpec, reshape};
use serde_json::{Value, json};

fn task(id: &str, paths: &[&str]) -> Value {
    json!({
        "id": id,
        "objective": format!("Build the {id} part."),
        "paths": paths,
        "acceptance": [format!("cargo test -p {id}")],
    })
}

/// Three tasks: `api` provides `parse`, `cli` consumes it, `docs` stands alone.
fn plan() -> Value {
    let mut api = task("api", &["crates/api/"]);
    api["provides"] = json!([{"name": "parse", "signature": "pub fn parse(s: &str) -> Plan"}]);
    let mut cli = task("cli", &["crates/cli/src/main.rs"]);
    cli["consumes"] = json!(["parse"]);
    cli["depends_on"] = json!(["api"]);
    json!({
        "version": 1,
        "summary": "Parse plans and print them.",
        "checks": ["cargo check --workspace"],
        "tasks": [api, cli, task("docs", &["docs/plans.md"])],
    })
}

fn parse(plan: &Value) -> Result<SwarmPlan, String> {
    swarm::parse(&plan.to_string()).map_err(|e| e.to_string())
}

fn assert_refused(plan: &Value, needle: &str) {
    let error = parse(plan).expect_err(needle);
    assert!(error.contains(needle), "expected {needle:?} in {error}");
    assert!(
        error.contains("swarm plan `.archon/swarm-plan.json` is refused"),
        "{error}"
    );
}

fn swarm_spec() -> PipelineSpec {
    Catalog::builtin().get("swarm").expect("built-in").clone()
}

#[test]
fn a_valid_plan_parses_into_roles_in_plan_order() {
    let plan = parse(&plan()).expect("valid");
    assert_eq!(plan.roles(), ["api", "cli", "docs"]);
    assert!(plan.problems().is_empty());
    assert!(plan.tasks[2].provides.is_empty(), "interfaces are optional");
}

#[test]
fn a_plan_has_two_to_eight_tasks() {
    let mut one = plan();
    one["tasks"] = json!([task("api", &["a"])]);
    assert_refused(&one, "a plan has 2 to 8 tasks, found 1");
    let mut nine = plan();
    let tasks: Vec<Value> = (0..9)
        .map(|n| task(&format!("t{n}"), &[&format!("p{n}")]))
        .collect();
    nine["tasks"] = json!(tasks);
    assert_refused(&nine, "found 9");
    let mut eight = plan();
    eight["tasks"] = json!(tasks[..8]);
    parse(&eight).expect("eight tasks fit");
}

#[test]
fn owned_paths_are_disjoint_by_component() {
    let mut nested = plan();
    nested["tasks"][1]["paths"] = json!(["crates/api/src/lib.rs"]);
    assert_refused(&nested, "tasks \"api\" and \"cli\" both own");
    let mut same = plan();
    same["tasks"][2]["paths"] = json!(["crates/cli/src/main.rs"]);
    assert_refused(&same, "owned paths must be disjoint");
    let mut prefix = plan();
    prefix["tasks"][1]["paths"] = json!(["crates/apis"]);
    parse(&prefix).expect("a shared name prefix is no overlap");
    let mut own = plan();
    own["tasks"][0]["paths"] = json!(["crates/api", "crates/api/src"]);
    parse(&own).expect("a task may list nested paths of its own");
}

#[test]
fn paths_are_relative_normalised_and_outside_git_and_archon() {
    let cases = [
        ("/etc/passwd", "must be relative"),
        ("src/*.rs", "no glob"),
        ("src\\lib.rs", "no glob or backslash"),
        ("src/../lib.rs", "must be normalised"),
        ("./src", "must be normalised"),
        ("src//lib.rs", "must be normalised"),
        (".git/config", "is inside `.git` or `.archon`"),
        (".archon/swarm-plan.json", "is inside `.git` or `.archon`"),
        ("", "must be 1 to 256 bytes"),
    ];
    for (path, needle) in cases {
        let mut bad = plan();
        bad["tasks"][2]["paths"] = json!([path]);
        assert_refused(&bad, needle);
    }
    let mut none = plan();
    none["tasks"][2]["paths"] = json!([]);
    assert_refused(&none, "task \"docs\" owns 1 to 64 paths, found 0");
}

#[test]
fn checks_and_acceptance_commands_are_present_single_lines() {
    let mut no_checks = plan();
    no_checks["checks"] = json!([]);
    assert_refused(&no_checks, "checks lists 1 to 16 commands, found 0");
    let mut no_acceptance = plan();
    no_acceptance["tasks"][0]["acceptance"] = json!([]);
    assert_refused(
        &no_acceptance,
        "task \"api\" acceptance lists 1 to 16 commands",
    );
    let mut two_lines = plan();
    two_lines["checks"] = json!(["make\nrm -rf /"]);
    assert_refused(&two_lines, "a command is one non-empty line");
    let mut missing = plan();
    missing.as_object_mut().unwrap().remove("checks");
    assert_refused(&missing, "missing field `checks`");
}

#[test]
fn task_ids_name_branches_and_are_unique() {
    for bad in ["Api", "a--b", "-a", "a b", ""] {
        let mut plan = plan();
        plan["tasks"][2]["id"] = json!(bad);
        assert_refused(&plan, "must be 1 to 32 of [a-z0-9-]");
    }
    let mut twice = plan();
    twice["tasks"][2]["id"] = json!("api");
    assert_refused(&twice, "task id \"api\" is used more than once");
}

#[test]
fn dependencies_name_earlier_tasks_only() {
    let mut forward = plan();
    forward["tasks"][0]["depends_on"] = json!(["docs"]);
    assert_refused(
        &forward,
        "task \"api\" depends on \"docs\", which is no earlier task",
    );
    let mut itself = plan();
    itself["tasks"][2]["depends_on"] = json!(["docs"]);
    assert_refused(&itself, "task \"docs\" depends on itself");
}

#[test]
fn consumed_interfaces_are_provided_by_another_task() {
    let mut unknown = plan();
    unknown["tasks"][2]["consumes"] = json!(["render"]);
    assert_refused(
        &unknown,
        "task \"docs\" consumes \"render\", which no task provides",
    );
    let mut own = plan();
    own["tasks"][0]["consumes"] = json!(["parse"]);
    assert_refused(&own, "which it provides itself");
    let mut twice = plan();
    twice["tasks"][2]["provides"] = json!([{"name": "parse", "signature": "fn parse()"}]);
    assert_refused(
        &twice,
        "interface \"parse\" is provided by both \"api\" and \"docs\"",
    );
    let mut unsigned = plan();
    unsigned["tasks"][0]["provides"][0]["signature"] = json!(" ");
    assert_refused(&unsigned, "without a name and a signature");
}

#[test]
fn the_file_is_bounded_versioned_and_strict() {
    let mut v2 = plan();
    v2["version"] = json!(2);
    assert_refused(&v2, "version must be 1, got 2");
    let mut extra = plan();
    extra["owner"] = json!("me");
    assert_refused(&extra, "unknown field `owner`");
    let mut blank = plan();
    blank["summary"] = json!("");
    assert_refused(&blank, "summary is empty");
    let huge = " ".repeat(swarm::MAX_PLAN_BYTES + 1);
    let error = swarm::parse(&huge).expect_err("too big").to_string();
    assert!(error.contains("a plan has at most 65536"), "{error}");
    let error = swarm::parse("not json").expect_err("not json").to_string();
    assert!(error.contains("it is not a plan"), "{error}");
}

/// A worker sees its own task and the shared interfaces, never another
/// task's objective.
#[test]
fn a_task_prompt_holds_its_task_and_the_shared_interfaces_only() {
    let plan = parse(&plan()).expect("valid");
    let RunEdit::AssignRoles {
        stage,
        roles,
        role_prompts,
    } = plan.assignment("build")
    else {
        panic!("a plan assigns roles");
    };
    assert_eq!(stage, "build");
    assert_eq!(roles, ["api", "cli", "docs"]);
    let cli = &role_prompts["cli"];
    for said in [
        "You build task `cli` (2 of 3) of the swarm plan.",
        "Tasks merge in plan order: `api`, `cli`, `docs`.",
        "Build the cli part.",
        "- `crates/cli/src/main.rs`",
        "- `parse` from task `api`, built in parallel",
        "```text\npub fn parse(s: &str) -> Plan\n```",
        "`api`, merged before yours.",
        "- `cargo test -p cli`",
    ] {
        assert!(cli.contains(said), "missing {said:?} in {cli}");
    }
    assert!(!cli.contains("Build the api part."), "{cli}");
    assert!(!cli.contains("Build the docs part."), "{cli}");
    let api = &role_prompts["api"];
    assert!(
        api.contains("### Interfaces you provide\n\n- `parse`, exactly:"),
        "{api}"
    );
    assert!(role_prompts["docs"].contains("No other task."));
}

#[test]
fn the_swarm_team_waits_for_the_plan_stage() {
    let spec = swarm_spec();
    let ids: Vec<&str> = spec.file.stages.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        ["plan", "build", "integrate"],
        "the model_plan stage ids"
    );
    let build = spec.stage("build").expect("a build stage");
    assert_eq!(build.kind, StageKind::Team);
    assert!(build.awaits_roles());
    assert_eq!(
        spec.assigned_by("plan").map(|s| s.id.as_str()),
        Some("build")
    );
    let loops = spec.stage("integrate").and_then(|s| s.on_fail.as_ref());
    let loops = loops.expect("integrate sends tasks back");
    assert_eq!((loops.goto.as_str(), loops.max_loops), ("build", 2));
}

#[test]
fn a_plan_assigns_the_team_once_and_keeps_roles_that_ran() {
    let spec = swarm_spec();
    let plan = parse(&plan()).expect("valid");
    let edit = swarm::assign(&spec, "build", &plan, false)
        .expect("assignable")
        .expect("an unassigned team gets an edit");
    let assigned = reshape::apply(&spec, &edit).expect("applies");
    let build = assigned.stage("build").expect("kept");
    assert_eq!(build.roles, ["api", "cli", "docs"]);
    assert_eq!(build.fan_out, 3, "one slot per task");
    assert!(!build.awaits_roles());
    assert!(build.role_prompts["docs"].contains("Build the docs part."));
    let again = swarm::assign(&assigned, "build", &plan, true).expect("same plan");
    assert_eq!(again, None, "the same plan records nothing");

    let mut reworded = plan.clone();
    reworded.tasks[2].objective = "Write the plans page.".into();
    let edit = swarm::assign(&assigned, "build", &reworded, true).expect("same roles");
    assert!(edit.is_some(), "new prompts for the same roles");
    let mut renamed = plan.clone();
    renamed.tasks[2].id = "guide".into();
    let error = swarm::assign(&assigned, "build", &renamed, true).expect_err("roles ran");
    assert!(
        error
            .to_string()
            .contains("already ran tasks api, cli, docs"),
        "{error}"
    );
    let fresh = swarm::assign(&assigned, "build", &renamed, false).expect("not run yet");
    assert!(fresh.is_some(), "a team that never ran takes the new roles");
}

#[test]
fn only_a_team_waiting_for_a_plan_takes_assigned_roles() {
    let spec = swarm_spec();
    let plan = parse(&plan()).expect("valid");
    let error = reshape::apply(&spec, &plan.assignment("plan")).expect_err("a work stage");
    assert!(
        error
            .to_string()
            .contains("no team stage that takes its roles"),
        "{error}"
    );
    let error = reshape::apply(&spec, &plan.assignment("ghost")).expect_err("no such stage");
    assert!(error.to_string().contains("\"ghost\""), "{error}");
}

#[test]
fn the_integrator_can_read_the_plan_after_an_earlier_attempt_removed_it() {
    let prompt = include_str!("../../../pipelines/swarm/integrate.md");
    assert!(prompt.contains("git rm .archon/swarm-plan.json"));
    assert!(
        prompt.contains("--diff-filter=A -- .archon/swarm-plan.json"),
        "a retry must be told how to recover the removed plan from history"
    );
}

#[test]
fn the_planner_is_told_the_task_limit_and_a_shape_that_deserialises() {
    assert_eq!(swarm::task_limit(0), swarm::MAX_TASKS);
    assert_eq!(swarm::task_limit(5), 5);
    assert_eq!(swarm::task_limit(100), swarm::MAX_TASKS);
    let told = swarm::planner_limits(5);
    assert!(told.contains("2 to 5 tasks (the agent caps"), "{told}");
    let fence = told.split("```json\n").nth(1).expect("a shape");
    let shape = fence.split("\n```").next().expect("a closed fence");
    let plan: SwarmPlan = serde_json::from_str(shape).expect("the shape is a plan");
    assert_eq!(plan.tasks[0].provides[0].name, "parse_plan");
}
