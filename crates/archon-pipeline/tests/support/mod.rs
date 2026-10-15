//! Shared test helpers: pipeline specs from inline TOML, model plans and
//! run variables.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use archon_core::catalog::PipelineSource;
use archon_core::id::{PipelineRunId, QueueItemId};
use archon_core::model::{Effort, ModelChoice, ModelPlan};
use archon_core::repo::ShipMode;
use archon_pipeline::spec::{PipelineFile, PipelineSpec};
use archon_pipeline::{Catalog, CatalogSources, RunVars};

pub fn choice(model: &str) -> ModelChoice {
    ModelChoice {
        model: model.parse().expect("provider/model"),
        effort: Effort::High,
        tier: None,
    }
}

/// A spec parsed from inline TOML; every stage gets a small default
/// prompt unless `prompts` overrides it.
pub fn spec_from(toml_text: &str, prompts: &[(&str, &str)]) -> PipelineSpec {
    let file: PipelineFile = toml::from_str(toml_text).expect("test TOML parses");
    let mut map: BTreeMap<String, String> = file
        .stages
        .iter()
        .map(|s| {
            (
                s.prompt.clone(),
                "Do {{task.title}} as {{stage.id}}.\n{{feedback}}".to_owned(),
            )
        })
        .collect();
    for (name, text) in prompts {
        map.insert((*name).to_owned(), (*text).to_owned());
    }
    PipelineSpec {
        file,
        source: PipelineSource::User,
        dir: None,
        prompts: map,
    }
}

/// `implement`, then one gated review that loops back twice; archond ships.
pub const REVIEWED: &str = r#"
[pipeline]
name = "reviewed"
description = "Implement, one gated review, then ship"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.ship]
stage = "implement"

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
gate = "all_pass"
timeout_minutes = 60
on_fail = { goto = "implement", max_loops = 2 }
"#;

/// `implement`, a gated test, then three gated reviewers; each gate loops
/// back three times of its own.
pub const STRICT_REVIEWED: &str = r#"
[pipeline]
name = "strict-reviewed"
description = "Implement, a gated test, three gated reviews, then ship"
version = 1
workspace = "worktree"
requires_repo = true

[pipeline.ship]
stage = "implement"

[[stage]]
id = "implement"
kind = "work"
prompt = "implement.md"

[[stage]]
id = "test"
kind = "test"
prompt = "test.md"
gate = "all_pass"
on_fail = { goto = "implement", max_loops = 3 }

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 3
gate = "all_pass"
timeout_minutes = 90
on_fail = { goto = "implement", max_loops = 3 }
"#;

const IMPLEMENT_PROMPT: (&str, &str) = (
    "implement.md",
    "# Implement: {{task.title}}\n\n{{task.brief}}\n\nThis is attempt {{attempt}}.\n{{feedback}}",
);

/// The prompts of [`REVIEWED`].
const REVIEWED_PROMPTS: [(&str, &str); 2] = [
    IMPLEMENT_PROMPT,
    (
        "review.md",
        "# Review: {{task.title}}\n\n{{previous.implement.summary}}",
    ),
];

/// The prompts of [`STRICT_REVIEWED`].
const STRICT_PROMPTS: [(&str, &str); 3] = [
    IMPLEMENT_PROMPT,
    (
        "test.md",
        "# Verify: {{task.title}}\n\n{{previous.implement.summary}}",
    ),
    (
        "review.md",
        "# Independent review {{fan.index}} of {{fan.count}}: {{task.title}}\n\n\
         {{previous.implement.summary}}\n\n{{previous.test.summary}}",
    ),
];

/// A built-in, a documented example from `docs/examples/pipelines`, or one
/// of the shapes above by its name.
pub fn builtin(name: &str) -> Arc<PipelineSpec> {
    let spec = match name {
        "reviewed" => spec_from(REVIEWED, &REVIEWED_PROMPTS),
        "strict-reviewed" => spec_from(STRICT_REVIEWED, &STRICT_PROMPTS),
        _ => examples().get(name).expect("pipeline exists").clone(),
    };
    assert_eq!(spec.name(), name, "found by its name");
    Arc::new(spec)
}

/// The built-ins under the documented example pipelines.
fn examples() -> Catalog {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/pipelines");
    let sources = CatalogSources {
        user_dir: Some(dir),
        repo_root: None,
        disabled: Vec::new(),
    };
    let catalog = Catalog::discover(&sources).expect("the examples load");
    assert!(catalog.problems().is_empty(), "{:?}", catalog.problems());
    catalog
}

/// One choice per stage, and `fan_choices` choices for every fan-out stage.
pub fn plan_for(spec: &PipelineSpec, fan_choices: &[&str]) -> ModelPlan {
    let mut plan = BTreeMap::new();
    for stage in &spec.file.stages {
        let choices: Vec<ModelChoice> = if stage.fan_out > 1 && !fan_choices.is_empty() {
            fan_choices
                .iter()
                .take(stage.fan_out as usize)
                .map(|m| choice(m))
                .collect()
        } else {
            vec![choice("claude-code/opus")]
        };
        plan.insert(stage.id.clone(), choices);
    }
    ModelPlan(plan)
}

pub fn base_vars(run: &PipelineRunId, item: &QueueItemId) -> BTreeMap<String, String> {
    base_vars_merging(run, item, false)
}

/// The run worktree of a team pipeline: `main` under the item's directory.
pub const TEAM_WORKSPACE: &str = "/archon/worktrees/demo/w1/main";

/// As [`base_vars`], laid out as `spec` needs: a team pipeline runs in
/// [`TEAM_WORKSPACE`].
pub fn base_vars_for(
    spec: &PipelineSpec,
    run: &PipelineRunId,
    item: &QueueItemId,
) -> BTreeMap<String, String> {
    let mut vars = base_vars(run, item);
    let team = spec
        .file
        .stages
        .iter()
        .any(|s| s.kind == archon_core::catalog::StageKind::Team);
    if team {
        vars.insert("workspace".into(), TEAM_WORKSPACE.into());
    }
    vars
}

/// As [`base_vars`], with `ship.auto_merge` as the item's merge knob says.
pub fn base_vars_merging(
    run: &PipelineRunId,
    item: &QueueItemId,
    auto_merge: bool,
) -> BTreeMap<String, String> {
    RunVars {
        task_title: "Fix the widget",
        task_brief: "The widget breaks on empty input.",
        repo_name: Some("demo"),
        repo_path: Some(Path::new("/src/demo")),
        workspace: Path::new("/archon/worktrees/demo/w1"),
        branch: "archon/w1-fix-the-widget",
        base_branch: "main",
        ship_mode: ShipMode::Pr,
        auto_merge,
        run_id: run,
        item_id: item,
    }
    .to_map()
}
