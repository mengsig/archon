//! `pipeline.toml` as parsed (ARCHITECTURE §8.2). Parsing is serde; the
//! cross-field rules live in [`PipelineSpec::validate`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use archon_core::autonomy::ReviewLevel;
use archon_core::catalog::{
    Gate, Nudges, PipelineSource, PipelineSummary, StageKind, StageSummary, WorkspaceMode,
};
use archon_core::error::ArchonError;
use serde::{Deserialize, Serialize};

use crate::template;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_FAN_OUT: u32 = 16;
pub const MAX_LOOPS: u32 = 10;
/// 30 days. Stage agents get exactly the configured timeout; a larger one
/// is refused at load, never clamped.
pub const MAX_TIMEOUT_MINUTES: u32 = 30 * 24 * 60;
/// Bounds what one pipeline directory can make the daemon read per enqueue.
pub const MAX_STAGES: usize = 32;
pub const MIN_ROLES: usize = 2;
pub const MAX_ROLES: usize = 8;
/// Role names end up in branch and directory names; keep them short.
pub const MAX_ROLE_LEN: usize = 32;

/// Serialize writes a run's frozen snapshot, stages inserted by the
/// autonomy knobs included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineFile {
    pub pipeline: PipelineHeader,
    #[serde(default)]
    pub nudges: Nudges,
    /// Missing entirely is a validation error, not a parse error, so the
    /// message can say what is wrong.
    #[serde(rename = "stage", default)]
    pub stages: Vec<StageSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineHeader {
    pub name: String,
    pub description: String,
    pub version: u32,
    #[serde(default)]
    pub workspace: WorkspaceMode,
    #[serde(default = "default_true")]
    pub requires_repo: bool,
    /// The review one of the pipeline's own stages holds, when its gates
    /// do not show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<InStageReview>,
}

fn default_true() -> bool {
    true
}

/// `[pipeline.review]`: the pipeline runs at `level` because `stage` has its
/// change reviewed, and passes only with that on record (ARCHITECTURE §8.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InStageReview {
    pub level: ReviewLevel,
    pub stage: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "StageToml")]
pub struct StageSpec {
    pub id: String,
    pub kind: StageKind,
    pub prompt: String,
    pub fan_out: u32,
    pub gate: Gate,
    pub timeout_minutes: u32,
    /// Allows child delegation even with guarded tools; Archon tool exposure
    /// still follows the item’s tools mode.
    pub allow_delegation: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nudge: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<OnFail>,
    /// A team stage's role names, one worker each, in merge order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Extra prompt text for a team role, appended verbatim to its prompt.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub role_prompts: BTreeMap<String, String>,
}

/// A `[[stage]]` table as written: an omitted `fan_out` is one agent, or
/// one per role on a team stage.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageToml {
    id: String,
    kind: StageKind,
    prompt: String,
    #[serde(default)]
    fan_out: Option<u32>,
    #[serde(default)]
    gate: Gate,
    #[serde(default = "default_timeout")]
    timeout_minutes: u32,
    #[serde(default)]
    allow_delegation: bool,
    #[serde(default)]
    nudge: Option<String>,
    #[serde(default)]
    on_fail: Option<OnFail>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    role_prompts: BTreeMap<String, String>,
}

impl From<StageToml> for StageSpec {
    fn from(raw: StageToml) -> Self {
        let implied = match raw.kind {
            StageKind::Team => u32::try_from(raw.roles.len()).unwrap_or(u32::MAX),
            _ => 1,
        };
        Self {
            id: raw.id,
            kind: raw.kind,
            prompt: raw.prompt,
            fan_out: raw.fan_out.unwrap_or(implied),
            gate: raw.gate,
            timeout_minutes: raw.timeout_minutes,
            allow_delegation: raw.allow_delegation,
            nudge: raw.nudge,
            on_fail: raw.on_fail,
            roles: raw.roles,
            role_prompts: raw.role_prompts,
        }
    }
}

fn default_timeout() -> u32 {
    120
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnFail {
    /// An earlier stage to re-enter with `{{feedback}}`.
    pub goto: String,
    pub max_loops: u32,
}

/// A loaded, validated pipeline with its prompt templates in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSpec {
    pub file: PipelineFile,
    pub source: PipelineSource,
    /// `None` for built-ins.
    pub dir: Option<PathBuf>,
    /// Prompt file name to its template text.
    pub prompts: BTreeMap<String, String>,
}

impl PipelineSpec {
    pub fn name(&self) -> &str {
        &self.file.pipeline.name
    }

    pub fn stage(&self, id: &str) -> Option<&StageSpec> {
        self.file.stages.iter().find(|s| s.id == id)
    }

    pub fn stage_index(&self, id: &str) -> Option<usize> {
        self.file.stages.iter().position(|s| s.id == id)
    }

    /// `pipeline.toml` text for this spec, stages inserted by autonomy
    /// knobs included; loading it yields the same [`PipelineFile`].
    pub fn manifest(&self) -> Result<String, ArchonError> {
        let text = toml::to_string(&self.file).map_err(|e| {
            ArchonError::Pipeline(format!(
                "pipeline {:?} does not serialize: {e}",
                self.name()
            ))
        })?;
        debug_assert!(toml::from_str::<PipelineFile>(&text).is_ok_and(|f| f == self.file));
        Ok(text)
    }

    /// Enforce every load-time rule of ARCHITECTURE §8.2, given the
    /// directory name it was found under. Errors name the stage and field.
    pub fn validate(&self, dir_name: &str) -> Result<(), ArchonError> {
        self.validate_header(dir_name)?;
        let stages = &self.file.stages;
        check_stage_count(stages.len())?;
        for (index, stage) in stages.iter().enumerate() {
            self.validate_stage(index, stage)
                .map_err(|e| invalid(format!("stage {:?}: {e}", stage.id)))?;
        }
        validate_propose_placement(stages)?;
        validate_ship_placement(stages, self.file.pipeline.workspace)?;
        validate_team_names(stages)?;
        self.validate_review()
    }

    /// Whether `stage` holds the review the pipeline declares.
    pub fn holds_review(&self, stage: &StageSpec) -> bool {
        let review = self.file.pipeline.review.as_ref();
        review.is_some_and(|review| review.stage == stage.id)
    }

    /// A declared review is `standard`, and held by one delegating work
    /// stage that runs before anything ships.
    fn validate_review(&self) -> Result<(), ArchonError> {
        let Some(review) = &self.file.pipeline.review else {
            return Ok(());
        };
        let refuse = |why: &str| Err(invalid(format!("[pipeline.review] {why}")));
        match review.level {
            ReviewLevel::Standard => {}
            ReviewLevel::Strict => {
                return refuse(
                    "level = \"strict\" cannot be declared: strict is gated review and test stages",
                );
            }
            ReviewLevel::None => {
                return refuse("level = \"none\" declares no review: leave the table out");
            }
        }
        let Some(index) = self.stage_index(&review.stage) else {
            return refuse(&format!("stage {:?} names no stage", review.stage));
        };
        let stage = &self.file.stages[index];
        // A work stage is one agent: `validate_stage` refuses its fan-out.
        if stage.kind != StageKind::Work {
            return refuse(&format!("stage {:?} must be a work stage", stage.id));
        }
        if !stage.allow_delegation {
            return refuse(&format!(
                "stage {:?} needs allow_delegation = true: its reviewer is an agent it delegates to",
                stage.id
            ));
        }
        Ok(())
    }

    fn validate_header(&self, dir_name: &str) -> Result<(), ArchonError> {
        let header = &self.file.pipeline;
        if header.name != dir_name {
            return Err(invalid(format!(
                "[pipeline] name {:?} must equal its directory name {dir_name:?}",
                header.name
            )));
        }
        if !valid_id(&header.name) {
            return Err(invalid(format!(
                "[pipeline] name {:?} must match [a-z0-9-]+",
                header.name
            )));
        }
        if header.version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "[pipeline] version {} is not supported (only {SCHEMA_VERSION} exists)",
                header.version
            )));
        }
        if header.workspace == WorkspaceMode::Worktree && !header.requires_repo {
            return Err(invalid(
                "[pipeline] workspace = \"worktree\" needs requires_repo = true",
            ));
        }
        Ok(())
    }

    /// Rules for one stage; `index` is its position, for `goto` and
    /// `previous.*` ordering. Errors omit the stage id; the caller adds it.
    fn validate_stage(&self, index: usize, stage: &StageSpec) -> Result<(), String> {
        if !valid_id(&stage.id) {
            return Err("id must match [a-z0-9-]+".into());
        }
        if self.stage_index(&stage.id) != Some(index) {
            return Err("id is used by more than one stage".into());
        }
        validate_roles(stage)?;
        if !(1..=MAX_FAN_OUT).contains(&stage.fan_out) {
            return Err(format!("fan_out must be between 1 and {MAX_FAN_OUT}"));
        }
        self.validate_kind(stage)?;
        if !(1..=MAX_TIMEOUT_MINUTES).contains(&stage.timeout_minutes) {
            return Err(format!(
                "timeout_minutes must be between 1 and {MAX_TIMEOUT_MINUTES} (30 days), got {}",
                stage.timeout_minutes
            ));
        }
        if let Some(on_fail) = &stage.on_fail {
            self.validate_on_fail(index, on_fail)?;
        }
        self.validate_prompt(index, stage)
    }

    /// What the stage's kind allows: fan-out, a gate, and the workspace.
    fn validate_kind(&self, stage: &StageSpec) -> Result<(), String> {
        let team = stage.kind == StageKind::Team;
        if stage.fan_out > 1 && !team && !stage.kind.allows_fan_out() {
            return Err(format!(
                "fan_out > 1 is refused for {:?} stages: parallel agents would write one worktree",
                stage.kind
            ));
        }
        if stage.gate != Gate::None && !stage.kind.allows_gate() {
            return Err("a gate is only allowed on review, test or report stages".into());
        }
        if team && self.file.pipeline.workspace != WorkspaceMode::Worktree {
            return Err(
                "a team stage gives each role its own git worktree, so it needs workspace = \"worktree\""
                    .into(),
            );
        }
        let judges = matches!(stage.kind, StageKind::Review | StageKind::Test);
        if judges && stage.gate == Gate::None {
            return Err(
                "a review or test stage needs gate = \"all_pass\", \"majority\" or \"any\": with no gate its verdict is ignored"
                    .into(),
            );
        }
        let writes_reports = matches!(stage.kind, StageKind::Propose | StageKind::Report);
        if writes_reports && self.file.pipeline.workspace == WorkspaceMode::RepoReadonly {
            return Err(
                "propose and report stages write .archon/reports/ into the workspace, so they need workspace = \"worktree\" or \"scratch\""
                    .into(),
            );
        }
        Ok(())
    }

    fn validate_on_fail(&self, index: usize, on_fail: &OnFail) -> Result<(), String> {
        match self.stage_index(&on_fail.goto) {
            Some(target) if target < index => {}
            Some(_) => {
                return Err(format!(
                    "on_fail.goto {:?} must name an earlier stage",
                    on_fail.goto
                ));
            }
            None => return Err(format!("on_fail.goto {:?} names no stage", on_fail.goto)),
        }
        if on_fail.max_loops > MAX_LOOPS {
            return Err(format!("on_fail.max_loops must be at most {MAX_LOOPS}"));
        }
        Ok(())
    }

    fn validate_prompt(&self, index: usize, stage: &StageSpec) -> Result<(), String> {
        if !valid_file_name(&stage.prompt) {
            return Err(format!(
                "prompt {:?} must be a plain file name in the pipeline directory",
                stage.prompt
            ));
        }
        let Some(text) = self.prompts.get(&stage.prompt) else {
            return Err(format!("prompt file {:?} does not exist", stage.prompt));
        };
        let names =
            template::placeholders(text).map_err(|e| format!("prompt {:?}: {e}", stage.prompt))?;
        for name in names {
            let team_only = template::TEAM_VARIABLES.contains(&name.as_str());
            if team_only && stage.kind != StageKind::Team {
                return Err(format!(
                    "prompt {:?} uses {{{{{name}}}}}, which only a team stage's prompt may use",
                    stage.prompt
                ));
            }
            if !team_only && !self.placeholder_known_at(&name, index) {
                return Err(format!(
                    "prompt {:?} uses unknown placeholder {{{{{name}}}}}; known: {}, previous.<earlier-stage>.summary, and in team stages {}",
                    stage.prompt,
                    template::KNOWN_VARIABLES.join(", "),
                    template::TEAM_VARIABLES.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// A fixed variable, or `previous.<id>.summary` of a stage before `index`.
    fn placeholder_known_at(&self, name: &str, index: usize) -> bool {
        if template::KNOWN_VARIABLES.contains(&name) {
            return true;
        }
        template::previous_stage(name)
            .and_then(|id| self.stage_index(id))
            .is_some_and(|earlier| earlier < index)
    }

    pub fn summary(&self) -> PipelineSummary {
        let header = &self.file.pipeline;
        PipelineSummary {
            name: header.name.clone(),
            description: header.description.clone(),
            source: self.source,
            path: self.dir.clone(),
            workspace: header.workspace,
            requires_repo: header.requires_repo,
            nudges: self.file.nudges.clone(),
            stages: self
                .file
                .stages
                .iter()
                .map(|s| StageSummary {
                    id: s.id.clone(),
                    kind: s.kind,
                    fan_out: s.fan_out,
                    gate: s.gate,
                    timeout_minutes: s.timeout_minutes,
                    nudge: s.nudge.clone(),
                    loops_to: s.on_fail.as_ref().map(|f| f.goto.clone()),
                    max_loops: s.on_fail.as_ref().map_or(0, |f| f.max_loops),
                    roles: s.roles.clone(),
                })
                .collect(),
        }
    }
}

/// One to [`MAX_STAGES`] stages.
pub(crate) fn check_stage_count(count: usize) -> Result<(), ArchonError> {
    match count {
        0 => Err(invalid("at least one [[stage]] is required")),
        n if n > MAX_STAGES => Err(invalid(format!(
            "{n} stages is too many; a pipeline has at most {MAX_STAGES}"
        ))),
        _ => Ok(()),
    }
}

fn invalid(message: impl Into<String>) -> ArchonError {
    ArchonError::Pipeline(message.into())
}

/// Stage ids and pipeline names: `[a-z0-9-]+`, safe in roles, labels and
/// template variable names.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Role names: 1 to [`MAX_ROLE_LEN`] of `[a-z0-9-]`, starting and ending
/// alphanumeric with no `--`, so a worker branch
/// `<run branch>--<role>-<run8>-a<n>` parses back.
pub fn valid_role(role: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let (Some(first), Some(last)) = (role.chars().next(), role.chars().last()) else {
        return false;
    };
    role.len() <= MAX_ROLE_LEN
        && alnum(first)
        && alnum(last)
        && role.chars().all(|c| alnum(c) || c == '-')
        && !role.contains("--")
}

/// `roles` and `role_prompts` belong to team stages only; a team stage
/// has [`MIN_ROLES`] to [`MAX_ROLES`] unique roles and one agent per role.
fn validate_roles(stage: &StageSpec) -> Result<(), String> {
    if stage.kind != StageKind::Team {
        if !stage.roles.is_empty() || !stage.role_prompts.is_empty() {
            return Err("roles and role_prompts are only allowed on team stages".into());
        }
        return Ok(());
    }
    if !(MIN_ROLES..=MAX_ROLES).contains(&stage.roles.len()) {
        return Err(format!(
            "a team stage needs {MIN_ROLES} to {MAX_ROLES} roles, found {}",
            stage.roles.len()
        ));
    }
    for (at, role) in stage.roles.iter().enumerate() {
        if !valid_role(role) {
            return Err(format!(
                "role {role:?} must be 1 to {MAX_ROLE_LEN} of [a-z0-9-], start and end with a letter or digit, and not contain \"--\""
            ));
        }
        if stage.roles[..at].contains(role) {
            return Err(format!("role {role:?} is listed more than once"));
        }
    }
    if usize::try_from(stage.fan_out).ok() != Some(stage.roles.len()) {
        return Err(format!(
            "fan_out = {} must equal the number of roles ({}); omit it to imply one agent per role",
            stage.fan_out,
            stage.roles.len()
        ));
    }
    match stage.role_prompts.keys().find(|r| !stage.roles.contains(r)) {
        Some(stray) => Err(format!(
            "role_prompts names {stray:?}, which is not in roles"
        )),
        None => Ok(()),
    }
}

/// Worker branches omit the stage and worker directories join stage and
/// role, so both must be unique across every team stage of a pipeline.
fn validate_team_names(stages: &[StageSpec]) -> Result<(), ArchonError> {
    let mut roles = BTreeMap::new();
    let mut dirs = BTreeMap::new();
    for stage in stages.iter().filter(|s| s.kind == StageKind::Team) {
        for role in &stage.roles {
            if let Some(other) = roles.insert(role.as_str(), stage.id.as_str()) {
                return Err(invalid(format!(
                    "role {role:?} is used by team stages {other:?} and {:?}; roles must be unique across a pipeline",
                    stage.id
                )));
            }
            let dir = crate::team::worker_dir(&stage.id, role);
            if let Some(other) = dirs.insert(dir.clone(), stage.id.as_str()) {
                return Err(invalid(format!(
                    "team stages {other:?} and {:?} would both put a worker in directory {dir:?}",
                    stage.id
                )));
            }
        }
    }
    debug_assert_eq!(roles.len(), dirs.len(), "one directory per role");
    Ok(())
}

/// A prompt must live directly in the pipeline directory: no separators and
/// no `..`, so a repository pipeline cannot read files outside its folder.
pub(crate) fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && name != "pipeline.toml"
}

/// At most one propose stage, and it must come first: it proposes what
/// every later stage does.
fn validate_propose_placement(stages: &[StageSpec]) -> Result<(), ArchonError> {
    let proposes: Vec<usize> = stages
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == StageKind::Propose)
        .map(|(i, _)| i)
        .collect();
    match proposes.as_slice() {
        [] | [0] => Ok(()),
        [only] => Err(invalid(format!(
            "propose stage {:?} must be the first stage",
            stages[*only].id
        ))),
        _ => Err(invalid("at most one propose stage is allowed")),
    }
}

/// At most one ship stage, it must be last, and it needs a worktree to
/// commit in.
fn validate_ship_placement(
    stages: &[StageSpec],
    workspace: WorkspaceMode,
) -> Result<(), ArchonError> {
    let ships: Vec<usize> = stages
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == StageKind::Ship)
        .map(|(i, _)| i)
        .collect();
    match ships.as_slice() {
        [] => Ok(()),
        [only] if *only + 1 == stages.len() && workspace == WorkspaceMode::Worktree => Ok(()),
        [only] if *only + 1 == stages.len() => {
            Err(invalid("a ship stage needs workspace = \"worktree\""))
        }
        [only] => Err(invalid(format!(
            "ship stage {:?} must be the last stage",
            stages[*only].id
        ))),
        _ => Err(invalid("at most one ship stage is allowed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_pipeline_file_parses() {
        for builtin in crate::builtin::BUILTINS {
            let text = builtin
                .file("pipeline.toml")
                .expect("pipeline.toml present");
            let parsed: PipelineFile =
                toml::from_str(text).unwrap_or_else(|e| panic!("{}: {e}", builtin.name));
            assert_eq!(parsed.pipeline.name, builtin.name);
            assert_eq!(parsed.pipeline.version, SCHEMA_VERSION);
            for stage in &parsed.stages {
                assert!(
                    builtin.file(&stage.prompt).is_some(),
                    "{}: missing prompt {}",
                    builtin.name,
                    stage.prompt
                );
            }
        }
    }
}
