//! Proposed user pipelines are validated but excluded from the active catalog.
use crate::{PipelineSpec, spec};
use archon_core::catalog::PipelineSource;
use archon_core::error::ArchonError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn validate(name: &str, files: &BTreeMap<String, String>) -> Result<PipelineSpec, ArchonError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ArchonError::Invalid(
            "proposal name must contain only letters, digits, '-' or '_'".into(),
        ));
    }
    if files.len() > 16 || files.values().map(String::len).sum::<usize>() > 64 * 1024 {
        return Err(ArchonError::Invalid(
            "pipeline proposal exceeds 16 files or 64 KiB".into(),
        ));
    }
    if files
        .keys()
        .any(|f| f != "pipeline.toml" && !spec::valid_file_name(f))
    {
        return Err(ArchonError::Invalid(
            "pipeline proposal files must be safe flat filenames".into(),
        ));
    }
    let manifest = files
        .get("pipeline.toml")
        .ok_or_else(|| ArchonError::Invalid("proposal needs pipeline.toml".into()))?;
    let file = toml::from_str(manifest)
        .map_err(|e| ArchonError::Pipeline(format!("proposed pipeline: {e}")))?;
    let pipeline = PipelineSpec {
        file,
        source: PipelineSource::User,
        dir: None,
        prompts: files
            .iter()
            .filter(|(name, _)| name.as_str() != "pipeline.toml")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    pipeline.validate(name)?;
    assert_eq!(pipeline.name(), name);
    assert!(!pipeline.file.stages.is_empty());
    Ok(pipeline)
}

pub fn stage(
    root: &Path,
    name: &str,
    files: &BTreeMap<String, String>,
) -> Result<PathBuf, ArchonError> {
    validate(name, files)?;
    std::fs::create_dir_all(root)
        .map_err(|e| ArchonError::io("create user pipelines directory", e))?;
    let destination = root.join(format!("{name}.proposed"));
    if destination.exists() {
        if read(&destination)? == *files {
            return Ok(destination);
        }
        return Err(ArchonError::Invalid(format!(
            "pipeline proposal {name} already exists"
        )));
    }
    let staging = root.join(format!(
        ".proposal-{}",
        archon_core::id::PipelineRunId::new()
    ));
    std::fs::create_dir(&staging)
        .map_err(|e| ArchonError::io("create proposal staging directory", e))?;
    for (name, text) in files {
        std::fs::write(staging.join(name), text)
            .map_err(|e| ArchonError::io("write proposed pipeline", e))?;
    }
    assert!(destination.parent() == Some(root));
    assert!(staging.parent() == Some(root));
    std::fs::rename(&staging, &destination)
        .map_err(|e| ArchonError::io("publish pipeline proposal", e))?;
    Ok(destination)
}

pub fn accept(root: &Path, name: &str) -> Result<PathBuf, ArchonError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ArchonError::Invalid(
            "invalid proposed pipeline name".into(),
        ));
    }
    let source = root.join(format!("{name}.proposed"));
    let files = read(&source)?;
    validate(name, &files)?;
    let destination = root.join(name);
    if destination.exists() {
        return Err(ArchonError::Invalid(format!(
            "pipeline {name} already exists; inspect and rename the proposal"
        )));
    }
    assert_ne!(source, destination);
    assert_eq!(source.parent(), destination.parent());
    std::fs::rename(&source, &destination)
        .map_err(|e| ArchonError::io("activate proposed pipeline", e))?;
    Ok(destination)
}

fn read(dir: &Path) -> Result<BTreeMap<String, String>, ArchonError> {
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|e| ArchonError::io("inspect proposed pipeline", e))?;
    if !metadata.is_dir() {
        return Err(ArchonError::Invalid(
            "proposal must be a real directory".into(),
        ));
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| ArchonError::io("read proposed pipeline", e))?;
    let mut files = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| ArchonError::io("read proposal entry", e))?;
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|e| ArchonError::io("inspect proposal entry", e))?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 || files.len() >= 16 {
            return Err(ArchonError::Invalid(
                "proposal contains unsafe or oversized files".into(),
            ));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ArchonError::Invalid("proposal filename must be UTF-8".into()))?;
        let text = std::fs::read_to_string(entry.path())
            .map_err(|e| ArchonError::io("read proposal file", e))?;
        files.insert(name, text);
    }
    assert!(files.len() <= 16);
    assert!(files.values().all(|text| text.len() <= 64 * 1024));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn files() -> BTreeMap<String, String> {
        BTreeMap::from([("pipeline.toml".into(),"[pipeline]\nname='learned-check'\ndescription='Repeated checking work'\nversion=1\nworkspace='scratch'\nrequires_repo=false\n[[stage]]\nid='check'\nkind='work'\nprompt='check.md'\ntimeout_minutes=5\n".into()),("check.md".into(),"Check the brief: {{task.brief}}".into())])
    }
    #[test]
    fn proposals_validate_stay_inactive_and_accept_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = stage(dir.path(), "learned-check", &files()).unwrap();
        assert!(path.ends_with("learned-check.proposed"));
        let sources = crate::CatalogSources {
            user_dir: Some(dir.path().into()),
            repo_root: None,
            disabled: vec![],
        };
        assert!(
            crate::Catalog::discover(&sources)
                .unwrap()
                .get("learned-check")
                .is_err()
        );
        assert_eq!(stage(dir.path(), "learned-check", &files()).unwrap(), path);
        let accepted = accept(dir.path(), "learned-check").unwrap();
        assert_eq!(
            crate::catalog::load_dir(&accepted, PipelineSource::User)
                .unwrap()
                .name(),
            "learned-check"
        );
        assert!(
            crate::Catalog::discover(&sources)
                .unwrap()
                .get("learned-check")
                .is_ok()
        );
        assert!(accept(dir.path(), "learned-check").is_err());
    }
    #[test]
    fn proposal_cannot_escape_its_directory_or_skip_validation() {
        assert!(validate("../escape", &files()).is_err());
        let mut bad = files();
        bad.insert("../outside".into(), "bad".into());
        assert!(validate("learned-check", &bad).is_err());
        let mut bad = files();
        bad.remove("check.md");
        assert!(validate("learned-check", &bad).is_err());
    }
}
