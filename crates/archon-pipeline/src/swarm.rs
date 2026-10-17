//! Swarm plans (docs/PIPELINES.md "Swarm plans"): the split a planner stage
//! commits for the team stage that names it in `roles_from`. The daemon
//! reads the file; this module parses it, enforces its rules and renders
//! the prompt each task's worker gets. No I/O.

use std::collections::{BTreeMap, BTreeSet};

use archon_core::error::ArchonError;
use archon_core::steer::RunEdit;
use serde::{Deserialize, Serialize};

use crate::reshape;
use crate::spec::{MAX_ROLE_LEN, MAX_ROLES, MIN_ROLES, PipelineSpec, valid_role};

/// Where the planner commits its plan, relative to the run worktree.
pub const PLAN_FILE: &str = ".archon/swarm-plan.json";
pub const PLAN_VERSION: u32 = 1;
pub const MAX_PLAN_BYTES: usize = 64 * 1024;
pub const MIN_TASKS: usize = MIN_ROLES;
pub const MAX_TASKS: usize = MAX_ROLES;
const MAX_PATHS: usize = 64;
const MAX_PATH_LEN: usize = 256;
const MAX_COMMANDS: usize = 16;
const MAX_COMMAND_LEN: usize = 500;
const MAX_INTERFACES: usize = 32;
const MAX_TEXT: usize = 8000;

/// A planner's split of one change into tasks that workers build at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmPlan {
    pub version: u32,
    pub summary: String,
    /// Fast checks the integrator runs on the merged change.
    pub checks: Vec<String>,
    /// In plan order, which is merge order: a task depends only on earlier ones.
    pub tasks: Vec<SwarmTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmTask {
    /// The worker's role: its branch and worktree are named after it.
    pub id: String,
    pub objective: String,
    /// Files and directories, relative to the repository root, that only
    /// this task changes.
    pub paths: Vec<String>,
    #[serde(default)]
    pub provides: Vec<Interface>,
    /// Names of interfaces other tasks provide.
    #[serde(default)]
    pub consumes: Vec<String>,
    /// Commands that pass in the task's worktree once it is done.
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub name: String,
    /// The exact signature, as the providing task writes it.
    pub signature: String,
}

/// Parse and validate a plan file's text; the error lists every problem.
pub fn parse(text: &str) -> Result<SwarmPlan, ArchonError> {
    if text.len() > MAX_PLAN_BYTES {
        return Err(refused(&[format!(
            "the file has {} bytes; a plan has at most {MAX_PLAN_BYTES}",
            text.len()
        )]));
    }
    let plan: SwarmPlan =
        serde_json::from_str(text).map_err(|e| refused(&[format!("it is not a plan: {e}")]))?;
    let problems = plan.problems();
    if !problems.is_empty() {
        return Err(refused(&problems));
    }
    assert!(
        (MIN_TASKS..=MAX_TASKS).contains(&plan.tasks.len()),
        "a valid plan is within the task bounds"
    );
    Ok(plan)
}

/// The edit that gives team stage `team` of `spec` the roles of `plan`;
/// `None` when it already has them, prompts and all. A team stage that
/// `ran` keeps its roles: earlier attempts' slots are counted by role.
pub fn assign(
    spec: &PipelineSpec,
    team: &str,
    plan: &SwarmPlan,
    ran: bool,
) -> Result<Option<RunEdit>, ArchonError> {
    let current = spec.stage(team).ok_or_else(|| {
        ArchonError::Pipeline(format!("the run has no team stage {team:?} to assign"))
    })?;
    let roles = plan.roles();
    if ran && current.roles != roles {
        return Err(refused(&[format!(
            "team stage {team:?} already ran tasks {}; a new plan keeps those task ids, not {}",
            current.roles.join(", "),
            roles.join(", ")
        )]));
    }
    let edit = plan.assignment(team);
    let assigned = reshape::apply(spec, &edit)?;
    let stage = assigned
        .stage(team)
        .expect("an edit keeps the stage it assigns");
    assert_eq!(stage.roles, roles, "the team runs one role per task");
    Ok((assigned != *spec).then_some(edit))
}

fn refused(problems: &[String]) -> ArchonError {
    assert!(!problems.is_empty(), "a refusal names a problem");
    let mut text = format!("swarm plan `{PLAN_FILE}` is refused:");
    for problem in problems {
        text.push_str(&format!("\n- {problem}"));
    }
    ArchonError::Invalid(text)
}

impl SwarmPlan {
    /// Every rule the plan breaks; empty for a valid plan.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.version != PLAN_VERSION {
            problems.push(format!(
                "version must be {PLAN_VERSION}, got {}",
                self.version
            ));
        }
        check_text("summary", &self.summary, &mut problems);
        check_commands("checks", &self.checks, &mut problems);
        if !(MIN_TASKS..=MAX_TASKS).contains(&self.tasks.len()) {
            problems.push(format!(
                "a plan has {MIN_TASKS} to {MAX_TASKS} tasks, found {}",
                self.tasks.len()
            ));
        }
        for task in &self.tasks {
            check_task(task, &mut problems);
        }
        check_order(&self.tasks, &mut problems);
        check_disjoint(&self.tasks, &mut problems);
        check_interfaces(&self.tasks, &mut problems);
        problems
    }

    /// The task ids, in plan order: the team stage's roles.
    pub fn roles(&self) -> Vec<String> {
        self.tasks.iter().map(|t| t.id.clone()).collect()
    }

    /// The edit that gives team stage `stage` one role per task, each with
    /// the prompt of its task.
    pub fn assignment(&self, stage: &str) -> RunEdit {
        assert!(self.problems().is_empty(), "only a valid plan is assigned");
        let role_prompts: BTreeMap<String, String> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(at, task)| (task.id.clone(), self.task_prompt(at)))
            .collect();
        assert_eq!(role_prompts.len(), self.tasks.len(), "one prompt per task");
        RunEdit::AssignRoles {
            stage: stage.to_owned(),
            roles: self.roles(),
            role_prompts,
        }
    }

    /// What task `at`'s worker is told: its task and the shared interfaces,
    /// never the other tasks' objectives.
    fn task_prompt(&self, at: usize) -> String {
        let task = &self.tasks[at];
        let order: Vec<String> = self.tasks.iter().map(|t| format!("`{}`", t.id)).collect();
        let mut text = format!(
            "You build task `{}` ({} of {}) of the swarm plan. Tasks merge in plan order: {}.\n\n\
             ### Objective\n\n{}\n\n\
             ### Paths you own\n\nChange files only here; every other path belongs to \
             another task or to nobody:\n\n",
            task.id,
            at + 1,
            self.tasks.len(),
            order.join(", "),
            task.objective.trim()
        );
        for path in &task.paths {
            text.push_str(&format!("- `{path}`\n"));
        }
        text.push_str(&self.interface_sections(task));
        text.push_str("\n### Depends on\n\n");
        match task.depends_on.is_empty() {
            true => text.push_str("No other task.\n"),
            false => {
                let deps: Vec<String> = task.depends_on.iter().map(|d| format!("`{d}`")).collect();
                text.push_str(&format!(
                    "{}, merged before yours. Build against the interfaces they provide.\n",
                    deps.join(", ")
                ));
            }
        }
        text.push_str(
            "\n### Acceptance\n\nEach must pass in your worktree before you complete:\n\n",
        );
        for command in &task.acceptance {
            text.push_str(&format!("- `{command}`\n"));
        }
        debug_assert!(text.contains(&task.id), "the prompt names its task");
        text
    }

    /// What the task provides and consumes, then every interface of the plan.
    fn interface_sections(&self, task: &SwarmTask) -> String {
        let mut text = String::from("\n### Interfaces you provide\n\n");
        if task.provides.is_empty() {
            text.push_str("None.\n");
        }
        for interface in &task.provides {
            text.push_str(&format!(
                "- `{}`, exactly:\n\n{}\n",
                interface.name,
                fenced(&interface.signature)
            ));
        }
        text.push_str("\n### Interfaces you consume\n\n");
        if task.consumes.is_empty() {
            text.push_str("None.\n");
        }
        for name in &task.consumes {
            let (owner, interface) = self
                .provider(name)
                .expect("validated: consumed is provided");
            text.push_str(&format!(
                "- `{name}` from task `{owner}`, built in parallel: code against this, not \
                 its files:\n\n{}\n",
                fenced(&interface.signature)
            ));
        }
        text.push_str("\n### Shared interfaces of the plan\n\n");
        for (owner, interface) in self.interfaces() {
            text.push_str(&format!("- `{}` (task `{owner}`)\n", interface.name));
        }
        text
    }

    fn interfaces(&self) -> impl Iterator<Item = (&str, &Interface)> {
        let tasks = self.tasks.iter();
        tasks.flat_map(|t| t.provides.iter().map(move |i| (t.id.as_str(), i)))
    }

    fn provider(&self, name: &str) -> Option<(&str, &Interface)> {
        self.interfaces()
            .find(|(_, interface)| interface.name == name)
    }
}

/// `text` in a code fence longer than any backtick run inside it.
fn fenced(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}text\n{}\n{fence}", text.trim_end())
}

fn check_text(field: &str, text: &str, problems: &mut Vec<String>) {
    if text.trim().is_empty() {
        problems.push(format!("{field} is empty"));
    } else if text.len() > MAX_TEXT {
        problems.push(format!("{field} has more than {MAX_TEXT} bytes"));
    }
}

/// One to [`MAX_COMMANDS`] commands, each one non-empty line.
fn check_commands(field: &str, commands: &[String], problems: &mut Vec<String>) {
    if commands.is_empty() || commands.len() > MAX_COMMANDS {
        problems.push(format!(
            "{field} lists 1 to {MAX_COMMANDS} commands, found {}",
            commands.len()
        ));
    }
    for command in commands {
        let one_line = !command.contains(['\n', '\r']);
        if command.trim().is_empty() || !one_line || command.len() > MAX_COMMAND_LEN {
            problems.push(format!(
                "{field} has {command:?}; a command is one non-empty line of at most \
                 {MAX_COMMAND_LEN} bytes"
            ));
        }
    }
}

fn check_task(task: &SwarmTask, problems: &mut Vec<String>) {
    if !valid_role(&task.id) {
        problems.push(format!(
            "task id {:?} must be 1 to {MAX_ROLE_LEN} of [a-z0-9-], start and end with a \
             letter or digit, and not contain \"--\"",
            task.id
        ));
    }
    let id = &task.id;
    check_text(&format!("task {id:?} objective"), &task.objective, problems);
    check_commands(
        &format!("task {id:?} acceptance"),
        &task.acceptance,
        problems,
    );
    if task.paths.is_empty() || task.paths.len() > MAX_PATHS {
        problems.push(format!(
            "task {id:?} owns 1 to {MAX_PATHS} paths, found {}",
            task.paths.len()
        ));
    }
    for path in &task.paths {
        if let Err(why) = path_parts(path) {
            problems.push(format!("task {id:?} path {path:?} {why}"));
        }
    }
    if task.provides.len() > MAX_INTERFACES {
        problems.push(format!(
            "task {id:?} provides more than {MAX_INTERFACES} interfaces"
        ));
    }
    for interface in &task.provides {
        let empty = interface.name.trim().is_empty() || interface.signature.trim().is_empty();
        if empty || interface.signature.len() > MAX_TEXT {
            problems.push(format!(
                "task {id:?} provides {:?} without a name and a signature of at most \
                 {MAX_TEXT} bytes",
                interface.name
            ));
        }
    }
}

/// A path's components: relative, normalised, outside `.git` and `.archon`.
fn path_parts(path: &str) -> Result<Vec<&str>, &'static str> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() || trimmed.len() > MAX_PATH_LEN {
        return Err("must be 1 to 256 bytes");
    }
    if trimmed.starts_with('/') || trimmed.contains(['\\', '*', '?', '[']) {
        return Err("must be relative to the repository root, with no glob or backslash");
    }
    if trimmed.chars().any(char::is_control) {
        return Err("has a control character");
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.iter().any(|p| matches!(*p, "" | "." | "..")) {
        return Err("must be normalised: no empty, `.` or `..` component");
    }
    if parts.contains(&".git") || parts[0] == ".archon" {
        return Err("is inside `.git` or `.archon`, which no task owns");
    }
    Ok(parts)
}

/// Ids are unique, and each dependency names an earlier task.
fn check_order(tasks: &[SwarmTask], problems: &mut Vec<String>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for task in tasks {
        let id = task.id.as_str();
        for dep in &task.depends_on {
            if dep == id {
                problems.push(format!("task {id:?} depends on itself"));
            } else if !seen.contains(dep.as_str()) {
                problems.push(format!(
                    "task {id:?} depends on {dep:?}, which is no earlier task; list tasks so \
                     that each depends only on earlier ones"
                ));
            }
        }
        if !seen.insert(id) {
            problems.push(format!("task id {id:?} is used more than once"));
        }
    }
}

/// No path of one task is, contains or lies inside a path of another.
fn check_disjoint(tasks: &[SwarmTask], problems: &mut Vec<String>) {
    let owned: Vec<(&str, &str, Vec<&str>)> = tasks
        .iter()
        .flat_map(|t| t.paths.iter().map(move |p| (t.id.as_str(), p.as_str())))
        .filter_map(|(id, path)| path_parts(path).ok().map(|parts| (id, path, parts)))
        .collect();
    for (at, (id, path, parts)) in owned.iter().enumerate() {
        for (other, other_path, other_parts) in &owned[at + 1..] {
            let overlaps = parts.starts_with(other_parts) || other_parts.starts_with(parts);
            if overlaps && id != other {
                problems.push(format!(
                    "tasks {id:?} and {other:?} both own {path:?} and {other_path:?}; owned \
                     paths must be disjoint"
                ));
            }
        }
    }
}

/// Interface names are unique, and each consumed one is another task's.
fn check_interfaces(tasks: &[SwarmTask], problems: &mut Vec<String>) {
    let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
    for task in tasks {
        for interface in &task.provides {
            if let Some(first) = owners.insert(&interface.name, &task.id) {
                problems.push(format!(
                    "interface {:?} is provided by both {first:?} and {:?}",
                    interface.name, task.id
                ));
            }
        }
    }
    for task in tasks {
        for name in &task.consumes {
            match owners.get(name.as_str()) {
                None => problems.push(format!(
                    "task {:?} consumes {name:?}, which no task provides",
                    task.id
                )),
                Some(owner) if *owner == task.id => problems.push(format!(
                    "task {:?} consumes {name:?}, which it provides itself",
                    task.id
                )),
                Some(_) => {}
            }
        }
    }
}
