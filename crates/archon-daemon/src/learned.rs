//! What finished runs taught: the model choices that become defaults and the
//! lessons a later run's agents are told. Nothing is learned yet.

use archon_core::error::ArchonError;
use archon_core::id::RepoId;
use archon_core::lesson::Lesson;
use archon_core::model::ModelPlan;
use archon_core::queue::QueueItem;
use archon_core::run::PipelineRun;
use archon_pipeline::PipelineSpec;
use archon_queue::Store;

/// The choices the outcomes in `repo` make the default for stages of `spec`;
/// `None` without enough of them. It may name only some stages.
pub(crate) fn default_plan(
    _store: &Store,
    _repo: Option<&RepoId>,
    _spec: &PipelineSpec,
) -> Result<Option<ModelPlan>, ArchonError> {
    Ok(None)
}

/// The lessons the agents of `run` are told: the same ones for every spawn
/// of the run, also after a restart.
pub(crate) fn lessons_for_run(
    _store: &Store,
    _run: &PipelineRun,
    _item: &QueueItem,
) -> Result<Vec<Lesson>, ArchonError> {
    Ok(Vec::new())
}
