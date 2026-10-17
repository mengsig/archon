//! Changing the stages of a run that have not started (ARCHITECTURE §6.6):
//! building the edit a steer records, and applying recorded edits to the
//! run's frozen pipeline. An edit holds the stages and prompt texts it adds,
//! so replay after a restart never reads the catalog.

use std::collections::BTreeMap;

use archon_core::autonomy::ReviewLevel;
use archon_core::catalog::StageKind;
use archon_core::error::ArchonError;
use archon_core::steer::RunEdit;
use serde::{Deserialize, Serialize};

use crate::spec::{DaemonShip, InStageReview, MAX_ROLES, PipelineSpec, StageSpec};

/// A stage with what a run needs to hold it: its prompt text, the review its
/// pipeline declared it holds, and whether archond shipped what it recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedStage {
    pub stage: StageSpec,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holds_review: Option<ReviewLevel>,
    /// Its pipeline's `[pipeline.ship]` named it (§8.7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writes_ship_record: bool,
}

/// A stage the run already has and keeps where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeptStage {
    keep: String,
}

/// One entry of a switch's complete stage list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Entry {
    Kept(KeptStage),
    Carried(Box<CarriedStage>),
}

fn refused(why: impl Into<String>) -> ArchonError {
    ArchonError::Invalid(why.into())
}

fn to_value<T: Serialize>(what: &T) -> Result<serde_json::Value, ArchonError> {
    serde_json::to_value(what)
        .map_err(|e| ArchonError::Pipeline(format!("a run edit does not serialize: {e}")))
}

fn from_value<T: for<'de> Deserialize<'de>>(
    value: &serde_json::Value,
    what: &str,
) -> Result<T, ArchonError> {
    T::deserialize(value)
        .map_err(|e| ArchonError::Pipeline(format!("recorded {what} is unreadable: {e}")))
}

/// How many leading stages of `spec` are settled history: every stage up to
/// the last one in `started`. Only the stages after them can change.
pub fn started_prefix<'a>(
    spec: &PipelineSpec,
    started: impl IntoIterator<Item = &'a str>,
) -> usize {
    let prefix = started
        .into_iter()
        .filter_map(|id| spec.stage_index(id))
        .map(|index| index + 1)
        .max()
        .unwrap_or(0);
    assert!(prefix <= spec.file.stages.len(), "a prefix of the stages");
    prefix
}

/// `stage` of `source` as an edit carries it into `spec`: under an id no
/// `staying` stage has and a prompt name `spec` uses for no other text.
fn carried(
    spec: &PipelineSpec,
    staying: &[StageSpec],
    source: &PipelineSpec,
    stage: &StageSpec,
) -> CarriedStage {
    let text = source.prompts.get(&stage.prompt).cloned();
    let text = text.expect("a loaded pipeline holds every stage's prompt");
    let id = free_name(&stage.id, "", |id| staying.iter().all(|s| s.id != id));
    let same_text = |name: &str| spec.prompts.get(name).is_none_or(|have| *have == text);
    let prompt = if same_text(&stage.prompt) {
        stage.prompt.clone()
    } else {
        free_name(&format!("{id}-steered"), ".md", same_text)
    };
    let review = source.file.pipeline.review.as_ref();
    let holds_review = review.filter(|r| r.stage == stage.id).map(|r| r.level);
    let ship = source.file.pipeline.ship.as_ref();
    CarriedStage {
        stage: StageSpec {
            id,
            prompt,
            ..stage.clone()
        },
        prompt: text,
        holds_review,
        writes_ship_record: ship.is_some_and(|ship| ship.stage == stage.id),
    }
}

/// `base` plus `suffix`, numbered from 2 until `free` takes it.
fn free_name(base: &str, suffix: &str, free: impl Fn(&str) -> bool) -> String {
    let plain = format!("{base}{suffix}");
    if free(&plain) {
        return plain;
    }
    let numbered = (2u32..)
        .map(|n| format!("{base}-{n}{suffix}"))
        .find(|name| free(name));
    numbered.expect("some number is free")
}

/// The edit that puts `stage` of `source` into `spec` after `after` (else
/// at `prefix`, the [`started_prefix`]), with the pipeline it yields.
pub fn insert_edit(
    spec: &PipelineSpec,
    prefix: usize,
    source: &PipelineSpec,
    stage: &str,
    after: Option<&str>,
) -> Result<(RunEdit, PipelineSpec), ArchonError> {
    assert!(prefix <= spec.file.stages.len(), "a prefix of the stages");
    let Some(wanted) = source.stage(stage) else {
        let name = source.name();
        return Err(refused(format!("pipeline {name:?} has no stage {stage:?}")));
    };
    let at = match after {
        None => prefix,
        Some(after) => match spec.stage_index(after) {
            Some(index) => index + 1,
            None => return Err(refused(format!("the run has no stage {after:?}"))),
        },
    };
    if at < prefix {
        let next = &spec.file.stages[at].id;
        return Err(refused(format!(
            "stage {next:?} already started, so nothing can be inserted before it; insert after {:?} or later",
            spec.file.stages[prefix - 1].id
        )));
    }
    let edit = RunEdit::InsertStage {
        after: at.checked_sub(1).map(|i| spec.file.stages[i].id.clone()),
        spec: to_value(&carried(spec, &spec.file.stages, source, wanted))?,
    };
    let reshaped = apply(spec, &edit).map_err(not_runnable)?;
    assert_eq!(reshaped.file.stages.len(), spec.file.stages.len() + 1);
    Ok((edit, reshaped))
}

/// The edit that replaces every unstarted stage of `spec` with what `target`
/// runs after the stages they share, with the pipeline it yields.
pub fn switch_edit(
    spec: &PipelineSpec,
    prefix: usize,
    target: &PipelineSpec,
) -> Result<(RunEdit, PipelineSpec), ArchonError> {
    assert!(prefix <= spec.file.stages.len(), "a prefix of the stages");
    let (from, to) = (&spec.file.pipeline, &target.file.pipeline);
    if from.workspace != to.workspace {
        return Err(refused(format!(
            "pipeline {:?} works in a {:?} workspace and this run in a {:?} one; a run keeps its workspace",
            to.name, to.workspace, from.workspace
        )));
    }
    let done = &spec.file.stages[..prefix];
    let shared = target
        .file
        .stages
        .iter()
        .rposition(|s| done.iter().any(|d| d.id == s.id));
    let rest = &target.file.stages[shared.map_or(0, |index| index + 1)..];
    // A proposal is approved before any work; a run under way is past it.
    let rest = rest
        .iter()
        .filter(|s| prefix == 0 || s.kind != StageKind::Propose);
    let mut stages: Vec<Entry> = done.iter().map(|s| kept(&s.id)).collect();
    stages.extend(rest.map(|s| Entry::Carried(Box::new(carried(spec, done, target, s)))));
    if stages.len() == prefix {
        return Err(refused(format!(
            "pipeline {:?} has nothing left to run after stage {:?}",
            to.name,
            done.last().map_or("", |s| s.id.as_str())
        )));
    }
    let edit = switch(&to.name, &stages)?;
    let reshaped = apply(spec, &edit).map_err(not_runnable)?;
    Ok((edit, reshaped))
}

fn kept(id: &str) -> Entry {
    Entry::Kept(KeptStage {
        keep: id.to_owned(),
    })
}

fn switch(pipeline: &str, stages: &[Entry]) -> Result<RunEdit, ArchonError> {
    assert!(!stages.is_empty(), "a run keeps at least one stage");
    let stages = stages.iter().map(to_value).collect::<Result<_, _>>()?;
    Ok(RunEdit::SwitchPipeline {
        pipeline: pipeline.to_owned(),
        stages,
    })
}

/// An edit a steer asked for that yields no valid pipeline is the steer's
/// mistake, not a broken snapshot.
fn not_runnable(error: ArchonError) -> ArchonError {
    match error {
        ArchonError::Pipeline(why) => refused(format!("the run could not continue: {why}")),
        other => other,
    }
}

/// `spec` after `edit`. An abort changes no stage, so it returns `spec` as
/// it is; the result of a shape edit is a valid pipeline of the same name.
pub fn apply(spec: &PipelineSpec, edit: &RunEdit) -> Result<PipelineSpec, ArchonError> {
    let mut out = spec.clone();
    // The last stage the edit brought that wrote its own pipeline's ship record.
    let mut recorder = None;
    match edit {
        RunEdit::AbortStage { .. } => return Ok(out),
        RunEdit::InsertStage { after, spec: stage } => {
            let stage: CarriedStage = from_value(stage, "inserted stage")?;
            let at = match after {
                None => 0,
                Some(after) => match out.stage_index(after) {
                    Some(index) => index + 1,
                    None => return Err(missing(after)),
                },
            };
            let stage = take_in(&mut out, stage, &mut recorder)?;
            out.file.stages.insert(at, stage);
        }
        RunEdit::SwitchPipeline { stages, .. } => {
            out.file.stages = switched(&mut out, stages, &mut recorder)?;
        }
        RunEdit::AssignRoles {
            stage,
            roles,
            role_prompts,
        } => {
            assign_roles(&mut out, stage, roles, role_prompts)?;
        }
    }
    let review = out.file.pipeline.review.take();
    out.file.pipeline.review = review.filter(|r| out.stage(&r.stage).is_some());
    out.file.pipeline.ship = ship_after(&out, recorder.as_deref());
    out.validate(spec.name())?;
    assert_eq!(out.name(), spec.name(), "a run keeps its snapshot's name");
    Ok(out)
}

/// Give `stage`, a team stage with `roles_from`, its roles. Replacing roles
/// after the stage ran is the daemon's to refuse: only it sees the attempts.
fn assign_roles(
    spec: &mut PipelineSpec,
    stage: &str,
    roles: &[String],
    role_prompts: &BTreeMap<String, String>,
) -> Result<(), ArchonError> {
    let index = spec.stage_index(stage).ok_or_else(|| missing(stage))?;
    let team = &mut spec.file.stages[index];
    if team.kind != StageKind::Team || team.roles_from.is_none() {
        return Err(refused(format!(
            "stage {stage:?} is no team stage that takes its roles from a plan"
        )));
    }
    if roles.is_empty() || roles.len() > MAX_ROLES {
        return Err(refused(format!(
            "a plan assigns {} roles to team stage {stage:?}",
            roles.len()
        )));
    }
    team.roles = roles.to_vec();
    team.role_prompts = role_prompts.clone();
    team.fan_out = roles.len() as u32;
    assert_eq!(team.roles.len(), team.fan_out as usize, "one slot per role");
    Ok(())
}

fn missing(stage: &str) -> ArchonError {
    ArchonError::Pipeline(format!(
        "a recorded edit names stage {stage:?}, which the run lacks"
    ))
}

/// What archond ships after an edit (§8.7): the record of `recorder`, a stage
/// it brought, if that runs after the one named so far; else that one, if any.
fn ship_after(spec: &PipelineSpec, recorder: Option<&str>) -> Option<DaemonShip> {
    let named = spec.file.pipeline.ship.as_ref().map(|s| s.stage.as_str());
    let named = named.filter(|id| spec.stage_index(id).is_some());
    assert!(
        recorder.is_none_or(|id| spec.stage_index(id).is_some()),
        "the edit placed the stage it brought"
    );
    let later = |a: &str, b: &str| spec.stage_index(a) > spec.stage_index(b);
    let stage = match (named, recorder) {
        (Some(named), Some(recorder)) if later(named, recorder) => named,
        (_, Some(recorder)) => recorder,
        (named, None) => named?,
    };
    Some(DaemonShip {
        stage: stage.to_owned(),
    })
}

/// The stage list a switch recorded, with the texts it carries taken into
/// `spec`.
fn switched(
    spec: &mut PipelineSpec,
    stages: &[serde_json::Value],
    recorder: &mut Option<String>,
) -> Result<Vec<StageSpec>, ArchonError> {
    let entries: Vec<Entry> = stages
        .iter()
        .map(|entry| from_value(entry, "stage list"))
        .collect::<Result<_, _>>()?;
    // What the old pipeline declared goes with it unless the run keeps the
    // stage, even where a new stage of that id comes in.
    let keeps = |id: &str| {
        let kept = |entry: &Entry| matches!(entry, Entry::Kept(k) if k.keep == id);
        entries.iter().any(kept)
    };
    let declared = &mut spec.file.pipeline;
    declared.review = declared.review.take().filter(|r| keeps(&r.stage));
    declared.ship = declared.ship.take().filter(|s| keeps(&s.stage));
    let mut list = Vec::with_capacity(entries.len());
    for entry in entries {
        let stage = match entry {
            Entry::Kept(KeptStage { keep }) => match spec.stage(&keep) {
                Some(stage) => stage.clone(),
                None => return Err(missing(&keep)),
            },
            Entry::Carried(stage) => take_in(spec, *stage, recorder)?,
        };
        list.push(stage);
    }
    assert_eq!(list.len(), stages.len(), "one stage per entry");
    let held = spec.file.pipeline.review.as_ref();
    assert!(
        held.is_none_or(|r| list.iter().any(|s| s.id == r.stage)),
        "a stage of the run holds the review"
    );
    Ok(list)
}

/// Take a carried stage's prompt and review into `spec`, noting in `recorder`
/// whether it writes a ship record, and return it for the caller to place.
fn take_in(
    spec: &mut PipelineSpec,
    carried: CarriedStage,
    recorder: &mut Option<String>,
) -> Result<StageSpec, ArchonError> {
    let CarriedStage {
        stage,
        prompt,
        holds_review,
        writes_ship_record,
    } = carried;
    if writes_ship_record {
        *recorder = Some(stage.id.clone());
    }
    let clash = |have: &String| *have != prompt;
    if spec.prompts.get(&stage.prompt).is_some_and(clash) {
        return Err(ArchonError::Pipeline(format!(
            "a recorded edit gives prompt {:?} a second text",
            stage.prompt
        )));
    }
    spec.prompts.insert(stage.prompt.clone(), prompt);
    if let Some(level) = holds_review
        && spec.file.pipeline.review.is_none()
    {
        spec.file.pipeline.review = Some(InStageReview {
            level,
            stage: stage.id.clone(),
        });
    }
    Ok(stage)
}

/// `spec` after every edit in the order they were recorded.
pub fn apply_all<'a>(
    spec: PipelineSpec,
    edits: impl IntoIterator<Item = &'a RunEdit>,
) -> Result<PipelineSpec, ArchonError> {
    let name = spec.name().to_owned();
    let mut out = spec;
    for edit in edits {
        out = apply(&out, edit)?;
    }
    assert_eq!(out.name(), name, "edits never rename the snapshot");
    Ok(out)
}
