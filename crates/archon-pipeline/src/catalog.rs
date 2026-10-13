//! Pipeline discovery with override precedence (ARCHITECTURE §8.1):
//! built-in < user (`~/.config/archon/pipelines`) < repository
//! (`<repo>/.archon/pipelines`). A later source replaces an earlier one by
//! name as a whole directory; a broken override makes the name unavailable
//! rather than falling back.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use archon_core::catalog::{CatalogListing, PipelineProblem, PipelineSource};
use archon_core::error::ArchonError;

use crate::spec::{self, PipelineFile, PipelineSpec};

/// Where to look. `repo_root` is `None` when listing outside a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSources {
    pub user_dir: Option<PathBuf>,
    pub repo_root: Option<PathBuf>,
    /// From `<repo>/.archon/config.toml [pipelines] disabled`.
    pub disabled: Vec<String>,
}

impl CatalogSources {
    pub fn user_only() -> Self {
        Self {
            user_dir: Some(archon_core::paths::config_dir().join("pipelines")),
            repo_root: None,
            disabled: Vec::new(),
        }
    }
}

/// `pipeline.toml` and prompts are small; anything bigger is a mistake.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;

pub const PIPELINE_FILE: &str = "pipeline.toml";

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pipelines: BTreeMap<String, PipelineSpec>,
    problems: Vec<PipelineProblem>,
    /// Names hidden by the repository's `[pipelines] disabled`.
    disabled: Vec<String>,
}

impl Catalog {
    /// Cheap enough to call on every enqueue: a handful of small files.
    /// Only an unreadable source directory is an error; a broken pipeline
    /// is recorded as a problem and its name becomes unavailable.
    pub fn discover(sources: &CatalogSources) -> Result<Self, ArchonError> {
        let mut catalog = Self::builtin();
        if let Some(user_dir) = &sources.user_dir {
            catalog.overlay_dir(user_dir, PipelineSource::User)?;
        }
        if let Some(repo_root) = &sources.repo_root {
            let repo_dir = archon_core::paths::repo_archon_dir(repo_root).join("pipelines");
            catalog.overlay_dir(&repo_dir, PipelineSource::Repo)?;
        }
        for name in &sources.disabled {
            catalog.pipelines.remove(name);
        }
        catalog.disabled = sources.disabled.clone();
        debug_assert!(catalog.pipelines.iter().all(|(n, s)| n == s.name()));
        Ok(catalog)
    }

    /// Only the pipelines compiled into the binary.
    pub fn builtin() -> Self {
        let mut catalog = Self::default();
        for builtin in crate::builtin::BUILTINS {
            match load_builtin(builtin) {
                Ok(spec) => catalog.insert(spec),
                Err(e) => catalog.record_problem(builtin.name, PipelineSource::Builtin, None, &e),
            }
        }
        catalog
    }

    pub fn get(&self, name: &str) -> Result<&PipelineSpec, ArchonError> {
        if let Some(spec) = self.pipelines.get(name) {
            return Ok(spec);
        }
        if self.disabled.iter().any(|d| d == name) {
            return Err(ArchonError::Pipeline(format!(
                "pipeline {name:?} is disabled by the repository's .archon/config.toml"
            )));
        }
        // The highest-precedence problem is the one that made the name unavailable.
        match self.problems.iter().rev().find(|p| p.name == name) {
            Some(problem) => Err(ArchonError::Pipeline(format!(
                "pipeline {name:?} is broken: {}",
                problem.error
            ))),
            None => Err(ArchonError::NotFound(format!("pipeline {name:?}"))),
        }
    }

    pub fn listing(&self) -> CatalogListing {
        CatalogListing {
            pipelines: self.pipelines.values().map(PipelineSpec::summary).collect(),
            problems: self.problems.clone(),
        }
    }

    pub fn problems(&self) -> &[PipelineProblem] {
        &self.problems
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.pipelines.keys().map(String::as_str)
    }

    fn insert(&mut self, spec: PipelineSpec) {
        let name = spec.name().to_owned();
        self.pipelines.insert(name, spec);
    }

    /// Record a broken pipeline and drop any lower-precedence one of its name:
    /// a broken override never falls back. The catalog's reader reports it.
    fn record_problem(
        &mut self,
        name: &str,
        source: PipelineSource,
        path: Option<PathBuf>,
        error: &ArchonError,
    ) {
        tracing::debug!(pipeline = name, ?source, ?path, %error, "pipeline excluded from catalog");
        self.pipelines.remove(name);
        self.problems.push(PipelineProblem {
            name: name.to_owned(),
            source,
            path,
            error: error.to_string(),
        });
    }

    /// Load every `<dir>/<name>/pipeline.toml`, replacing same-named
    /// pipelines from earlier sources. A missing `dir` contributes nothing.
    fn overlay_dir(&mut self, dir: &Path, source: PipelineSource) -> Result<(), ArchonError> {
        assert!(
            source != PipelineSource::Builtin,
            "built-ins are not loaded from disk"
        );
        for (name, path) in pipeline_dirs(dir)? {
            match name {
                Ok(name) => match load_dir(&path, source) {
                    Ok(spec) => self.insert(spec),
                    Err(e) => self.record_problem(&name, source, Some(path), &e),
                },
                Err(raw) => {
                    let e = ArchonError::Pipeline(format!("directory name {raw:?} is not UTF-8"));
                    self.record_problem(&raw, source, Some(path), &e);
                }
            }
        }
        Ok(())
    }
}

type DirName = Result<String, String>;

/// Subdirectories of `dir` that contain `pipeline.toml`, sorted by name so
/// problems are reported deterministically. Hidden entries are skipped.
/// A `pipeline.toml` that dangles or cannot be read still counts, so
/// [`load_dir`] reports it instead of the name falling back.
fn pipeline_dirs(dir: &Path) -> Result<Vec<(DirName, PathBuf)>, ArchonError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ArchonError::io(format!("reading {}", dir.display()), e)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| ArchonError::io(format!("reading {}", dir.display()), e))?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|raw| raw.to_string_lossy().into_owned());
        let hidden = name.as_ref().is_ok_and(|n| n.starts_with('.'));
        if hidden || !has_manifest(&path) {
            continue;
        }
        found.push((name, path));
    }
    found.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(found)
}

/// Whether `dir` has a `pipeline.toml` entry of any kind. Only "no such
/// entry" and "not a directory" mean no; any other error means maybe.
fn has_manifest(dir: &Path) -> bool {
    match std::fs::symlink_metadata(dir.join(PIPELINE_FILE)) {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

/// Load one pipeline directory (user or repo source): parse
/// `pipeline.toml`, read every stage prompt, and validate.
pub fn load_dir(dir: &Path, source: PipelineSource) -> Result<PipelineSpec, ArchonError> {
    assert!(
        source != PipelineSource::Builtin,
        "built-ins come from builtin::BUILTINS"
    );
    let dir_name = dir.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        ArchonError::Pipeline(format!("{} has no UTF-8 directory name", dir.display()))
    })?;
    let root = dir
        .canonicalize()
        .map_err(|e| ArchonError::io(format!("resolving {}", dir.display()), e))?;
    let manifest = dir.join(PIPELINE_FILE);
    let file = parse_file(
        &read_small(&root, &manifest)?,
        &manifest.display().to_string(),
    )?;
    // Checked before reading prompts, so a huge stage list reads nothing.
    spec::check_stage_count(file.stages.len())?;
    let mut prompts = BTreeMap::new();
    for stage in &file.stages {
        // Validation reports unsafe or missing names with the stage id.
        let path = dir.join(&stage.prompt);
        if spec::valid_file_name(&stage.prompt)
            && !prompts.contains_key(&stage.prompt)
            && path.is_file()
        {
            prompts.insert(stage.prompt.clone(), read_small(&root, &path)?);
        }
    }
    let spec = PipelineSpec {
        file,
        source,
        dir: Some(dir.to_path_buf()),
        prompts,
    };
    spec.validate(dir_name)?;
    Ok(spec)
}

fn load_builtin(builtin: &crate::builtin::BuiltinPipeline) -> Result<PipelineSpec, ArchonError> {
    let manifest = builtin.file(PIPELINE_FILE).ok_or_else(|| {
        ArchonError::Pipeline(format!("built-in {} has no {PIPELINE_FILE}", builtin.name))
    })?;
    let file = parse_file(
        manifest,
        &format!("built-in {}/{PIPELINE_FILE}", builtin.name),
    )?;
    let prompts = builtin
        .files
        .iter()
        .filter(|(name, _)| *name != PIPELINE_FILE)
        .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
        .collect();
    let spec = PipelineSpec {
        file,
        source: PipelineSource::Builtin,
        dir: None,
        prompts,
    };
    spec.validate(builtin.name)?;
    Ok(spec)
}

fn parse_file(text: &str, origin: &str) -> Result<PipelineFile, ArchonError> {
    toml::from_str(text).map_err(|e| ArchonError::Pipeline(format!("{origin}: {e}")))
}

/// Read a pipeline file of at most [`MAX_FILE_BYTES`]. It must resolve
/// inside `root` (the canonical pipeline directory): a symlink out of it
/// could hand the operator's files to an agent or into an error message.
fn read_small(root: &Path, path: &Path) -> Result<String, ArchonError> {
    assert!(root.is_absolute(), "root is canonical");
    let context = || format!("reading {}", path.display());
    let resolved = path
        .canonicalize()
        .map_err(|e| ArchonError::io(context(), e))?;
    if !resolved.starts_with(root) {
        return Err(ArchonError::Pipeline(format!(
            "{} resolves outside its pipeline directory {}",
            path.display(),
            root.display()
        )));
    }
    // Bounded read: `metadata().len()` is 0 for procfs-like files and a file
    // can grow after it is checked.
    let file = std::fs::File::open(&resolved).map_err(|e| ArchonError::io(context(), e))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ArchonError::io(context(), e))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(ArchonError::Pipeline(format!(
            "{} is larger than {MAX_FILE_BYTES} bytes, the limit for pipeline files",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|e| ArchonError::Pipeline(format!("{} is not UTF-8: {e}", path.display())))
}
