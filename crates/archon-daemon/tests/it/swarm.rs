//! The built-in `swarm` pipeline against the scripted backend: a planner
//! commits `.archon/swarm-plan.json`, archond checks it and gives `build`
//! one worker per task, and the integrator sends tasks back at most twice.

use crate::common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use archon_core::model::{Effort, ModelPlan};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::run::{PipelineRun, StageOutcome, StageRun};
use archon_core::steer::RunEdit;
use archon_daemon::executor;
use archon_pipeline::swarm::MAX_CORRECTIONS;
use archon_pipeline::team::worker_branch;
use common::{Harness, MODEL, choice, completed, git};
use serde_json::{Value, json};

const TASKS: [&str; 3] = ["api", "cli", "docs"];

fn swarm_model_plan() -> ModelPlan {
    let stages = ["plan", "build", "integrate"]
        .into_iter()
        .map(|stage| (stage.to_owned(), vec![choice(MODEL, Effort::Medium)]));
    ModelPlan(stages.collect::<BTreeMap<_, _>>())
}

/// A plan of `tasks`; of two or more, the first provides `parse` to the second.
fn swarm_plan(tasks: &[&str]) -> Value {
    let tasks: Vec<Value> = tasks
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "objective": format!("Build the {id} part."),
                "paths": [format!("{id}/")],
                "acceptance": [format!("test -f {id}/done.txt")],
            })
        })
        .collect();
    let mut plan = json!({
        "version": 1,
        "summary": "Three parts.",
        "checks": ["true"],
        "tasks": tasks,
    });
    if plan["tasks"]
        .as_array()
        .is_some_and(|tasks| tasks.len() > 1)
    {
        plan["tasks"][0]["provides"] = json!([{"name": "parse", "signature": "fn parse() -> u8"}]);
        plan["tasks"][1]["consumes"] = json!(["parse"]);
    }
    plan
}

fn commit_file(tree: &Path, file: &str, text: &str) {
    if let Some(dir) = Path::new(file).parent() {
        std::fs::create_dir_all(tree.join(dir)).expect("mkdir");
    }
    std::fs::write(tree.join(file), text).expect("write");
    git(tree, &["add", "-f", file]);
    git(tree, &["commit", "--quiet", "-m", &format!("write {file}")]);
}

/// The planner's file: written to the run worktree, never committed.
fn write_plan(tree: &Path, text: &str) {
    let file = tree.join(".archon/swarm-plan.json");
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("mkdir");
    std::fs::write(file, text).expect("write");
}

fn worker_tree(run: &PipelineRun, task: &str) -> PathBuf {
    let root = run.team_root.as_ref().expect("a team run");
    root.join(format!("build-{task}"))
}

/// Enqueue a swarm item and let its planner commit `plan`, if any, then pass.
/// A JSON string is committed as the file's raw text.
async fn planned(h: &Harness, title: &str, plan: Option<&Value>) -> (QueueItem, PipelineRun) {
    let item = h
        .enqueue_in("swarm", title, swarm_model_plan())
        .await
        .expect("the swarm built-in enqueues");
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].stage_id, "plan");
    if let Some(plan) = plan {
        let text = match plan {
            Value::String(raw) => raw.clone(),
            plan => serde_json::to_string_pretty(plan).expect("json"),
        };
        write_plan(&run.workspace, &text);
    }
    h.settle_running(|_| completed("split into tasks")).await;
    h.dispatch().await;
    (item, run)
}

/// The running workers, in task order, all at `attempt`.
async fn running_workers(h: &Harness, attempt: u32) -> Vec<StageRun> {
    let mut running = h.running().await;
    running.sort_by_key(|s| s.fan_index);
    let shape: Vec<(&str, u32)> = running
        .iter()
        .map(|s| (s.stage_id.as_str(), s.attempt))
        .collect();
    assert_eq!(shape, vec![("build", attempt); TASKS.len()], "{running:?}");
    running
}

/// No commit of the run branch ever held the plan file.
fn assert_no_plan_in_history(tree: &Path) {
    let log = git(
        tree,
        &["log", "--format=%h %s", "--", ".archon/swarm-plan.json"],
    );
    assert_eq!(log, "", "the plan is in the branch history");
}

/// The merged workers' branches are gone; only the run branch is left.
fn assert_no_role_branches(run: &PipelineRun) {
    let pattern = format!("{}--*", run.branch);
    let left = git(&run.workspace, &["branch", "--list", &pattern]);
    assert_eq!(left, "", "role branches outlive their merge");
}

/// Each worker commits in its own paths on its own branch and completes.
async fn build_round(h: &Harness, run: &PipelineRun, attempt: u32) {
    running_workers(h, attempt).await;
    for task in TASKS {
        let tree = worker_tree(run, task);
        let branch = worker_branch(&run.branch, &run.id, task, attempt);
        assert_eq!(git(&tree, &["rev-parse", "--abbrev-ref", "HEAD"]), branch);
        assert!(
            !tree.join(".archon/swarm-plan.json").exists(),
            "{task} forks from a branch without the plan"
        );
        commit_file(&tree, &format!("{task}/v{attempt}.txt"), "work\n");
    }
    let settled =
        h.settle_running(|s| completed(&format!("{} built", TASKS[s.fan_index as usize])));
    assert_eq!(settled.await.len(), TASKS.len());
    h.dispatch().await;
}

/// The only running stage, which must be `integrate` at `attempt`.
async fn running_integrator(h: &Harness, attempt: u32) {
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    let now = (running[0].stage_id.as_str(), running[0].attempt);
    assert_eq!(now, ("integrate", attempt));
}

fn redispatch(task: &str) -> StageOutcome {
    StageOutcome::Failed {
        error: format!("REDISPATCH: {task}\n{task}: its acceptance command failed"),
        infra: false,
    }
}

/// Each worker runs in its own tree with its task and the shared interfaces,
/// never another task's objective.
fn assert_worker_prompts(h: &Harness, run: &PipelineRun) {
    let requests = h.backend.requests();
    assert!(requests.len() > TASKS.len(), "a planner and the workers");
    for task in TASKS {
        let request = requests
            .iter()
            .find(|r| r.objective.contains(&format!("You build task `{task}`")))
            .unwrap_or_else(|| panic!("no worker for {task}"));
        assert!(
            request
                .objective
                .contains(&format!("Build the {task} part."))
        );
        assert!(
            request
                .objective
                .contains("Tasks merge in plan order: `api`, `cli`, `docs`.")
        );
        assert_eq!(request.workspace, worker_tree(run, task));
        for other in TASKS.iter().filter(|t| **t != task) {
            let theirs = format!("Build the {other} part.");
            assert!(
                !request.objective.contains(&theirs),
                "{}",
                request.objective
            );
        }
    }
    let cli = requests
        .iter()
        .find(|r| r.objective.contains("You build task `cli`"));
    let cli = &cli.expect("cli's worker").objective;
    assert!(
        cli.contains("- `parse` from task `api`, built in parallel"),
        "{cli}"
    );
    assert!(cli.contains("fn parse() -> u8"), "{cli}");
}

#[tokio::test]
async fn a_written_plan_gives_each_task_a_worker_with_its_task_and_peers() {
    let h = Harness::new().await;
    let (_, run) = planned(&h, "split three ways", Some(&swarm_plan(&TASKS))).await;
    let workers = running_workers(&h, 1).await;
    assert_worker_prompts(&h, &run);
    let edits = h.ctx.store.lock().await.run_edits(&run.id).expect("edits");
    assert_eq!(edits.len(), 1, "one assignment: {edits:?}");
    let RunEdit::AssignRoles { stage, roles, .. } = &edits[0].edit else {
        panic!("the plan assigns roles: {edits:?}");
    };
    assert_eq!(
        (stage.as_str(), roles.as_slice()),
        ("build", &TASKS.map(String::from)[..])
    );
    let roster = h.backend.messages();
    assert_eq!(
        roster.len(),
        TASKS.len(),
        "each worker gets its peers: {roster:?}"
    );
    for worker in &workers {
        let agent = worker.aether_agent.as_ref().expect("an agent");
        let (_, text) = roster.iter().find(|(to, _)| to == agent).expect("sent");
        assert!(text.starts_with("Archon team roster v1 for team stage `build` attempt 1"));
    }
    let notes = h
        .notices(archon_core::notify::NotificationKind::Progress)
        .await;
    let assigned = "The swarm plan assigned tasks api, cli, docs to team stage `build`.";
    assert!(notes.iter().any(|n| n == assigned), "{notes:?}");
}

/// A task consuming a module the base already holds is accepted (issue #275),
/// and its worker is told to use that module as it is.
#[tokio::test]
async fn a_task_may_consume_a_file_already_in_the_repository() {
    let h = Harness::new().await;
    commit_file(&h.repo, "greet.py", "def greet():\n    return 'hi'\n");
    let mut plan = swarm_plan(&TASKS);
    plan["tasks"][2]["consumes"] = json!(["greet.py"]);
    let (_, run) = planned(&h, "consume greet", Some(&plan)).await;
    let workers = running_workers(&h, 1).await;
    assert_eq!(workers.len(), TASKS.len(), "the plan was accepted");
    let edits = h.ctx.store.lock().await.run_edits(&run.id).expect("edits");
    let RunEdit::AssignRoles { role_prompts, .. } = &edits[0].edit else {
        panic!("the plan assigns roles: {edits:?}");
    };
    let docs = &role_prompts["docs"];
    assert!(
        docs.contains("- `greet.py`, already in the repository at the base"),
        "{docs}"
    );
}

/// A restart rebuilds the run from the recorded assignment, never the file.
#[tokio::test]
async fn a_restart_keeps_the_assigned_workers() {
    let mut h = Harness::new().await;
    let (_, run) = planned(&h, "restart mid-swarm", Some(&swarm_plan(&TASKS))).await;
    running_workers(&h, 1).await;
    let created = h.backend.created();
    std::fs::remove_file(run.workspace.join(".archon/swarm-plan.json")).expect("drop the plan");
    h.restart();
    executor::recover(&h.ctx).await.expect("recover");
    h.dispatch().await;
    running_workers(&h, 1).await;
    assert_eq!(h.backend.created(), created, "no worker is spawned twice");
}

#[tokio::test]
async fn the_integrator_sends_tasks_back_at_most_twice() {
    let h = Harness::new().await;
    let (item, run) = planned(&h, "loop the swarm", Some(&swarm_plan(&TASKS))).await;
    for attempt in 1..=3 {
        build_round(&h, &run, attempt).await;
        running_integrator(&h, attempt).await;
        let integrator = h.backend.requests().pop().expect("an integrator");
        assert!(
            integrator.objective.contains("[role cli] cli built"),
            "{}",
            integrator.objective
        );
        let merged = format!("cli/v{attempt}.txt");
        assert!(run.workspace.join(&merged).exists(), "{merged} is merged");
        assert_no_plan_in_history(&run.workspace);
        assert_no_role_branches(&run);
        h.settle_running(|_| redispatch("cli")).await;
        h.dispatch().await;
        if attempt < 3 {
            let rebuild = h.backend.requests().pop().expect("a worker again");
            assert!(
                rebuild.objective.contains("REDISPATCH: cli"),
                "{}",
                rebuild.objective
            );
        }
    }
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!(
            "a third failure ends the run: {:?}",
            h.state(&item.id).await
        );
    };
    assert!(error.contains("after 2 of 2 fix loop(s)"), "{error}");
    let builds = h.stages(&item.id).await;
    let workers = builds.iter().filter(|s| s.stage_id == "build").count();
    assert_eq!(workers, 3 * TASKS.len(), "three rounds of three workers");
}

/// The planner finishes again with `plan` committed, if any.
async fn replan(h: &Harness, run: &PipelineRun, plan: Option<&Value>) {
    let running = h.running().await;
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(running[0].stage_id, "plan");
    if let Some(plan) = plan {
        let text = serde_json::to_string_pretty(plan).expect("json");
        write_plan(&run.workspace, &text);
    }
    h.settle_running(|_| completed("fixed the plan")).await;
    h.dispatch().await;
}

/// The plan stage fails with the reason once its corrections are used up, and no worker starts.
async fn plan_refusal(h: &Harness, plan: Option<&Value>, needle: &str) -> String {
    let (item, run) = planned(h, needle, plan).await;
    for _ in 0..MAX_CORRECTIONS {
        let state = h.state(&item.id).await;
        assert!(
            matches!(state, QueueState::Running { .. }),
            "sent back: {state:?}"
        );
        replan(h, &run, None).await;
    }
    let QueueState::Failed { error, .. } = h.state(&item.id).await else {
        panic!("{needle}: {:?}", h.state(&item.id).await);
    };
    assert!(error.contains("stage `plan` passed, but"), "{error}");
    assert!(error.contains(needle), "{error}");
    let stages = h.stages(&item.id).await;
    assert!(stages.iter().all(|s| s.stage_id == "plan"), "{stages:?}");
    assert_eq!(stages.len(), 1 + MAX_CORRECTIONS, "{stages:?}");
    error
}

/// The refused plan goes back to the planner in the same stage with the
/// problem and the limits; its fixed file then gives `build` its workers.
async fn corrected(h: &Harness, refused: &Value, problem: &str) {
    let (item, run) = planned(h, problem, Some(refused)).await;
    let sent = h.backend.requests().pop().expect("the planner again");
    for needle in [problem, "Your plan was refused (correction 1 of 2)"] {
        assert!(
            sent.objective.contains(needle),
            "{needle}: {}",
            sent.objective
        );
    }
    for limit in [
        "## Limits on the plan file",
        "at most 8000 bytes",
        "`name` and a `signature`",
    ] {
        assert!(
            sent.objective.contains(limit),
            "{limit}: {}",
            sent.objective
        );
    }
    replan(h, &run, Some(&swarm_plan(&TASKS))).await;
    running_workers(h, 1).await;
    let stages = h.stages(&item.id).await;
    let plans = stages.iter().filter(|s| s.stage_id == "plan").count();
    assert_eq!(plans, 2, "one refusal, one correction: {stages:?}");
}

#[tokio::test]
async fn a_task_objective_over_the_limit_is_corrected_in_the_plan_stage() {
    let h = Harness::new().await;
    let mut plan = swarm_plan(&TASKS);
    plan["tasks"][1]["objective"] = json!("x".repeat(8001));
    corrected(&h, &plan, "has more than 8000 bytes").await;
}

#[tokio::test]
async fn more_tasks_than_the_agent_caps_allow_are_corrected_in_the_plan_stage() {
    let h = Harness::new().await;
    h.repo_config("[limits]\nmax_per_repo = 3\n");
    let plan = swarm_plan(&["a", "b", "c", "d"]);
    corrected(&h, &plan, "its plan has 4 tasks").await;
    let sent = h.backend.requests();
    let planner = sent
        .iter()
        .find(|r| r.objective.contains("## Limits on the plan file"));
    let text = &planner.expect("the planner was told its limits").objective;
    assert!(
        text.contains("2 to 3 tasks (the agent caps allow that many at once here)"),
        "{text}"
    );
}

#[tokio::test]
async fn an_interface_written_as_a_string_is_corrected_in_the_plan_stage() {
    let h = Harness::new().await;
    let mut plan = swarm_plan(&TASKS);
    plan["tasks"][0]["provides"] = json!(["parse"]);
    corrected(&h, &plan, "it is not a plan").await;
}

/// Only Archon's own refusal is sent back: an agent's failure that happens to
/// say "passed, but" ends the stage.
#[tokio::test]
async fn a_planner_s_own_failure_is_never_sent_back() {
    let h = Harness::new().await;
    let item = h
        .enqueue_in("swarm", "planner gives up", swarm_model_plan())
        .await
        .expect("the swarm built-in enqueues");
    h.dispatch().await;
    let failure = StageOutcome::Failed {
        error: "FAILED: tests passed, but packaging failed".into(),
        infra: false,
    };
    h.settle_running(|_| failure.clone()).await;
    h.dispatch().await;
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Failed { .. }), "{state:?}");
    assert_eq!(h.stages(&item.id).await.len(), 1);
}

#[tokio::test]
async fn a_committed_plan_is_refused() {
    let h = Harness::new().await;
    let item = h
        .enqueue_in("swarm", "plan committed", swarm_model_plan())
        .await
        .expect("the swarm built-in enqueues");
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let text = serde_json::to_string_pretty(&swarm_plan(&TASKS)).expect("json");
    commit_file(&run.workspace, ".archon/swarm-plan.json", &text);
    h.settle_running(|_| completed("split into tasks")).await;
    h.dispatch().await;
    let sent = h.backend.requests().pop().expect("the planner again");
    assert!(
        sent.objective.contains("is staged or tracked"),
        "{}",
        sent.objective
    );
}

/// Committing the plan and then untracking it still leaves it in history.
#[tokio::test]
async fn a_plan_committed_then_untracked_is_refused() {
    let h = Harness::new().await;
    let item = h
        .enqueue_in("swarm", "plan untracked", swarm_model_plan())
        .await
        .expect("the swarm built-in enqueues");
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let text = serde_json::to_string_pretty(&swarm_plan(&TASKS)).expect("json");
    commit_file(&run.workspace, ".archon/swarm-plan.json", &text);
    git(
        &run.workspace,
        &["rm", "--cached", "--quiet", ".archon/swarm-plan.json"],
    );
    git(
        &run.workspace,
        &["commit", "--quiet", "-m", "untrack the plan"],
    );
    h.settle_running(|_| completed("split into tasks")).await;
    h.dispatch().await;
    let sent = h.backend.requests().pop().expect("the planner again");
    assert!(
        sent.objective
            .contains("it committed `.archon/swarm-plan.json`"),
        "{}",
        sent.objective
    );
}

#[tokio::test]
async fn a_staged_plan_is_refused() {
    let h = Harness::new().await;
    let item = h
        .enqueue_in("swarm", "plan staged", swarm_model_plan())
        .await
        .expect("the swarm built-in enqueues");
    h.dispatch().await;
    let run = h.run(&item.id).await;
    let text = serde_json::to_string_pretty(&swarm_plan(&TASKS)).expect("json");
    write_plan(&run.workspace, &text);
    git(&run.workspace, &["add", "-f", ".archon/swarm-plan.json"]);
    h.settle_running(|_| completed("split into tasks")).await;
    h.dispatch().await;
    let sent = h.backend.requests().pop().expect("the planner again");
    assert!(
        sent.objective.contains("is staged or tracked"),
        "{}",
        sent.objective
    );
}

/// A merged role branch a crash left behind goes when the run ends.
#[tokio::test]
async fn a_run_that_ends_deletes_the_role_branches_left_merged() {
    let h = Harness::new().await;
    let (item, run) = planned(&h, "branch left", Some(&swarm_plan(&TASKS))).await;
    for attempt in 1..=3 {
        build_round(&h, &run, attempt).await;
        running_integrator(&h, attempt).await;
        if attempt == 3 {
            let stray = worker_branch(&run.branch, &run.id, "api", 1);
            git(&run.workspace, &["branch", &stray, "HEAD"]);
        }
        h.settle_running(|_| redispatch("cli")).await;
        h.dispatch().await;
    }
    let state = h.state(&item.id).await;
    assert!(matches!(state, QueueState::Failed { .. }), "{state:?}");
    assert_no_role_branches(&run);
}

#[tokio::test]
async fn an_invalid_or_missing_plan_fails_the_plan_stage_with_every_problem() {
    let h = Harness::new().await;
    let mut overlapping = swarm_plan(&TASKS);
    overlapping["tasks"][2]["paths"] = json!(["api/docs.md"]);
    overlapping["checks"] = json!([]);
    let needle = "tasks \"api\" and \"docs\" both own";
    let error = plan_refusal(&h, Some(&overlapping), needle).await;
    assert!(
        error.contains("checks lists 1 to 16 commands, found 0"),
        "{error}"
    );
    let solo = swarm_plan(&["solo"]);
    plan_refusal(&h, Some(&solo), "a plan has 2 to 8 tasks, found 1").await;
    let missing = "it wrote no `.archon/swarm-plan.json` in the run worktree";
    plan_refusal(&h, None, missing).await;
    let blank = Value::String(" \n".into());
    plan_refusal(&h, Some(&blank), "its `.archon/swarm-plan.json` is blank").await;
}

/// A team is admitted whole: more tasks than may run at once would wait forever.
#[tokio::test]
async fn a_plan_beyond_the_agent_caps_is_refused() {
    let h = Harness::new().await;
    h.repo_config("[limits]\nmax_per_repo = 2\n");
    let error = plan_refusal(&h, Some(&swarm_plan(&TASKS)), "its plan has 3 tasks").await;
    assert!(error.contains("run at most 2 at once here"), "{error}");
}
