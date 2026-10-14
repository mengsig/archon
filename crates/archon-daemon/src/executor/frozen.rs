//! Per-run pipeline snapshots. A run's pipeline files are copied at start
//! to `<data>/pipeline-runs/<run>/<name>/` and every driver of that run is
//! built from the copy, so editing or breaking a pipeline mid-run cannot
//! change a resumed run. Each file is written durably and swapped in by
//! rename, so a crash or power loss never leaves a half-written snapshot.
//!
//! A steer changes a run's stages through recorded edits (§6.6), which are
//! applied to the snapshot every time it is read; the files never change.

use std::io::Write;
use std::path::{Path, PathBuf};

use archon_core::catalog::PipelineSource;
use archon_core::error::ArchonError;
use archon_core::id::PipelineRunId;
use archon_core::steer::RunEdit;
use archon_pipeline::PipelineSpec;
use archon_pipeline::catalog::load_dir;
use archon_pipeline::reshape;

use crate::context::DaemonContext;

const MANIFEST: &str = "pipeline.toml";

fn runs_root(data_dir: &Path) -> PathBuf {
    data_dir.join("pipeline-runs")
}

fn run_dir(data_dir: &Path, run: &PipelineRunId) -> PathBuf {
    runs_root(data_dir).join(run.as_str())
}

/// Write `spec` for `run` and load the copy, which is what the run uses
/// from then on. The manifest is re-serialized, so stages the autonomy
/// knobs inserted are frozen too.
pub(super) async fn freeze(
    data_dir: &Path,
    run: &PipelineRunId,
    spec: &PipelineSpec,
) -> Result<PipelineSpec, ArchonError> {
    let root = runs_root(data_dir);
    let dir = run_dir(data_dir, run).join(spec.name());
    let manifest = spec.manifest()?;
    let prompts = spec.prompts.clone();
    let source = spec.source;
    let frozen = tokio::task::spawn_blocking(move || {
        write_files(&root, &dir, &manifest, &prompts)?;
        load_frozen(&dir, source)
    })
    .await
    .map_err(|e| ArchonError::io("pipeline snapshot task", std::io::Error::other(e)))??;
    assert_eq!(frozen.name(), spec.name(), "a snapshot keeps its name");
    assert_eq!(frozen.file, spec.file, "a snapshot keeps every stage");
    Ok(frozen)
}

/// The pipeline the run has now: its snapshot with every edit a steer
/// recorded for it, in the order they were recorded.
pub(crate) async fn thaw(
    ctx: &DaemonContext,
    run: &PipelineRunId,
    pipeline: &str,
) -> Result<PipelineSpec, ArchonError> {
    let recorded = ctx.store.lock().await.run_edits(run)?;
    let edits: Vec<RunEdit> = recorded.into_iter().map(|r| r.edit).collect();
    thaw_edited(&ctx.paths.data_dir, run, pipeline, &edits).await
}

/// The run's snapshot, as frozen at its start, after `edits`.
pub(super) async fn thaw_edited(
    data_dir: &Path,
    run: &PipelineRunId,
    pipeline: &str,
    edits: &[RunEdit],
) -> Result<PipelineSpec, ArchonError> {
    let dir = run_dir(data_dir, run).join(pipeline);
    let spec = tokio::task::spawn_blocking(move || load_frozen(&dir, PipelineSource::User))
        .await
        .map_err(|e| ArchonError::io("pipeline snapshot task", std::io::Error::other(e)))??;
    assert_eq!(spec.name(), pipeline, "a snapshot is filed under its name");
    let edited = reshape::apply_all(spec, edits)
        .map_err(|e| ArchonError::Pipeline(format!("recorded edits of run {run}: {e}")))?;
    assert!(
        !edited.file.stages.is_empty(),
        "an edited run keeps a stage"
    );
    Ok(edited)
}

/// Drop a finished run's snapshot. Failing leaves only a stale copy.
pub(super) fn discard(data_dir: &Path, run: &PipelineRunId) {
    let dir = run_dir(data_dir, run);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(dir = %dir.display(), %error, "pipeline snapshot not removed"),
    }
}

/// Write the snapshot into `dir`, under `root`, and sync every directory
/// from `dir` up to `root`'s parent, so the new entries survive a power loss.
fn write_files(
    root: &Path,
    dir: &Path,
    manifest: &str,
    prompts: &std::collections::BTreeMap<String, String>,
) -> Result<(), ArchonError> {
    assert!(
        dir.starts_with(root) && dir != root,
        "a snapshot lives under {}",
        root.display()
    );
    std::fs::create_dir_all(dir)
        .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))?;
    let files = std::iter::once((MANIFEST, manifest)).chain(
        prompts
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str())),
    );
    for (name, text) in files {
        // Prompt names were validated as plain file names by the catalog.
        assert!(
            !name.contains('/') && !name.starts_with('.'),
            "unsafe prompt name {name:?}"
        );
        write_durably(dir, name, text)?;
    }
    // The data directory holds `root`'s own entry, new on the first run.
    let top = root
        .parent()
        .expect("the runs root is in the data directory");
    for parent in dir.ancestors().take_while(|parent| parent.starts_with(top)) {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Write `text` to a temp file beside `dir/name`, sync it, then rename it
/// over `name`: a reader sees the old file or the whole new one.
fn write_durably(dir: &Path, name: &str, text: &str) -> Result<(), ArchonError> {
    let (path, temp) = (dir.join(name), dir.join(format!(".{name}.tmp")));
    assert!(dir.is_dir(), "{} is created first", dir.display());
    assert_eq!(
        temp.parent(),
        path.parent(),
        "a rename stays in one directory"
    );
    let mut file = std::fs::File::create(&temp)
        .map_err(|e| ArchonError::io(format!("creating {}", temp.display()), e))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| ArchonError::io(format!("writing {}", temp.display()), e))?;
    drop(file);
    std::fs::rename(&temp, &path).map_err(|e| {
        let context = format!("renaming {} to {}", temp.display(), path.display());
        ArchonError::io(context, e)
    })
}

fn sync_dir(dir: &Path) -> Result<(), ArchonError> {
    std::fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| ArchonError::io(format!("syncing directory {}", dir.display()), e))
}

/// `load_dir` refuses the built-in source (built-ins are not on disk), so
/// the copy is loaded as a user pipeline and keeps its original source.
fn load_frozen(dir: &Path, source: PipelineSource) -> Result<PipelineSpec, ArchonError> {
    let mut spec = load_dir(dir, PipelineSource::User)
        .map_err(|e| ArchonError::Pipeline(format!("pipeline snapshot {}: {e}", dir.display())))?;
    spec.source = source;
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let entries = std::fs::read_dir(dir).unwrap();
        let mut names: Vec<String> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_snapshot_file_is_replaced_whole_and_no_temp_file_is_left() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("pipeline-runs");
        let dir = root.join("run_1").join("default");
        let prompts = BTreeMap::from([("work.md".to_owned(), "Do it.".to_owned())]);
        write_files(&root, &dir, "old manifest", &prompts).unwrap();
        let before = std::fs::metadata(dir.join(MANIFEST)).unwrap().ino();
        let mut reader = std::fs::File::open(dir.join(MANIFEST)).unwrap();

        write_files(&root, &dir, "new manifest", &prompts).unwrap();
        let after = std::fs::metadata(dir.join(MANIFEST)).unwrap().ino();
        assert_ne!(
            before, after,
            "the file is swapped in, not rewritten in place"
        );
        let mut old = String::new();
        std::io::Read::read_to_string(&mut reader, &mut old).unwrap();
        assert_eq!(
            old, "old manifest",
            "an open reader keeps the whole old file"
        );
        let new = std::fs::read_to_string(dir.join(MANIFEST)).unwrap();
        assert_eq!(new, "new manifest");
        assert_eq!(names(&dir), [MANIFEST, "work.md"]);
    }

    #[test]
    fn a_temp_file_left_by_a_crash_is_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("pipeline-runs");
        let dir = root.join("run_1").join("default");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!(".{MANIFEST}.tmp")), "half a manif").unwrap();
        write_files(&root, &dir, "whole manifest", &BTreeMap::new()).unwrap();
        let text = std::fs::read_to_string(dir.join(MANIFEST)).unwrap();
        assert_eq!(text, "whole manifest");
        assert_eq!(names(&dir), [MANIFEST]);
    }
}
