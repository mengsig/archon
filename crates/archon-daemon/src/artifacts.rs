//! The durable per-item artifact store (ARCHITECTURE §13.5): files a stage
//! publishes from its worktree (evidence, logs, gitignored results) are
//! copied to `<data>/artifacts/<item id>/`, where later stages and items
//! read them by item id after the worktree is gone. Size-bounded by
//! `[artifacts]`, and deleted by gc `keep_after` an item settled.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use archon_core::aether_ids::AetherAgentId;
use archon_core::config::ArtifactsConfig;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::protocol::{
    ArtifactFile, ArtifactListing, ArtifactPage, MAX_ARTIFACT_LISTED, MAX_ARTIFACT_PAGE_BYTES,
    Reply,
};

use crate::context::DaemonContext;

/// An item's artifacts at most this many files, so a listing stays readable
/// and a publish cannot spend the inodes of the disk.
const MAX_FILES: usize = 10_000;
/// A stored name is at most this deep and long.
const MAX_DEPTH: usize = 8;
const MAX_NAME_BYTES: usize = 200;
/// One publish at a time, so concurrent ones cannot together pass an item's limit.
static PUBLISHING: Mutex<()> = Mutex::new(());
/// A publish is copied here first, so a failed one leaves nothing half-stored.
const INCOMING_PREFIX: &str = ".incoming-";

pub(crate) fn item_dir(root: &Path, item: &QueueItemId) -> PathBuf {
    root.join(item.as_str())
}

/// Whether the item has anything stored.
pub(crate) fn has_files(root: &Path, item: &QueueItemId) -> bool {
    let Ok(entries) = std::fs::read_dir(item_dir(root, item)) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| !is_incoming(&entry.file_name().to_string_lossy()))
}

fn is_incoming(name: &str) -> bool {
    name.starts_with(INCOMING_PREFIX)
}

/// `name` as a relative path of plain components.
fn stored_name(name: &str) -> Result<PathBuf, ArchonError> {
    let invalid = |why: &str| ArchonError::Invalid(format!("artifact name {name:?}: {why}"));
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(invalid(&format!("must be 1..{MAX_NAME_BYTES} bytes")));
    }
    let path = PathBuf::from(name);
    let mut depth = 0;
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(invalid("must be a relative path without `.` or `..`"));
        };
        let part = part.to_string_lossy();
        if part.chars().any(char::is_control) || is_incoming(&part) {
            return Err(invalid("has a control character or a reserved component"));
        }
        depth += 1;
    }
    if depth > MAX_DEPTH {
        return Err(invalid(&format!("is deeper than {MAX_DEPTH} directories")));
    }
    Ok(path)
}

/// What to copy: regular files by their path under the source and size.
struct Payload {
    files: Vec<(PathBuf, u64)>,
    bytes: u64,
}

/// The files at `source`, a file or a directory tree of regular files.
fn gather(source: &Path, limits: &ArtifactsConfig) -> Result<Payload, ArchonError> {
    let mut payload = Payload {
        files: Vec::new(),
        bytes: 0,
    };
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let path = if relative.as_os_str().is_empty() {
            source.to_path_buf()
        } else {
            source.join(&relative)
        };
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| ArchonError::io(format!("inspecting {}", path.display()), e))?;
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path)
                .map_err(|e| ArchonError::io(format!("reading {}", path.display()), e))?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| ArchonError::io(format!("reading {}", path.display()), e))?;
                pending.push(relative.join(entry.file_name()));
            }
        } else if meta.is_file() {
            if meta.len() > limits.max_file_bytes() {
                return Err(too_big(&path, meta.len(), limits));
            }
            payload.bytes = payload.bytes.saturating_add(meta.len());
            payload.files.push((relative, meta.len()));
            if payload.files.len() > MAX_FILES {
                return Err(ArchonError::Invalid(format!(
                    "{} holds more than {MAX_FILES} files; publish an archive of them",
                    source.display()
                )));
            }
        } else {
            return Err(ArchonError::Invalid(format!(
                "{} is a symlink or a special file; artifacts are regular files only",
                path.display()
            )));
        }
    }
    payload.files.sort();
    Ok(payload)
}

fn too_big(path: &Path, size: u64, limits: &ArtifactsConfig) -> ArchonError {
    ArchonError::Invalid(format!(
        "{} is {size} bytes, over artifacts.max_file_mb ({} MB); publish a smaller file",
        path.display(),
        limits.max_file_mb
    ))
}

/// The stored files under `dir`: relative names with sizes, sorted.
fn stored_files(dir: &Path) -> Result<Vec<ArtifactFile>, ArchonError> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let path = dir.join(&relative);
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(ArchonError::io(format!("reading {}", path.display()), e)),
        };
        for entry in entries {
            let entry =
                entry.map_err(|e| ArchonError::io(format!("reading {}", path.display()), e))?;
            let name = entry.file_name();
            if relative.as_os_str().is_empty() && is_incoming(&name.to_string_lossy()) {
                continue;
            }
            let meta = entry.metadata().map_err(|e| {
                ArchonError::io(format!("inspecting {}", entry.path().display()), e)
            })?;
            let child = relative.join(&name);
            if meta.is_dir() {
                pending.push(child);
            } else if meta.is_file() {
                files.push(ArtifactFile {
                    name: child.to_string_lossy().into_owned(),
                    bytes: meta.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

fn listing(root: &Path, item: &QueueItemId) -> Result<ArtifactListing, ArchonError> {
    let dir = item_dir(root, item);
    let mut files = stored_files(&dir)?;
    let total_bytes = files.iter().map(|f| f.bytes).sum();
    let omitted = files.len().saturating_sub(MAX_ARTIFACT_LISTED) as u64;
    files.truncate(MAX_ARTIFACT_LISTED);
    Ok(ArtifactListing {
        item: item.clone(),
        dir,
        files,
        omitted,
        total_bytes,
        published: Vec::new(),
    })
}

/// A name no other publish of this process is staging under.
fn incoming_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{INCOMING_PREFIX}{}-{n}", std::process::id())
}

/// Removes its directory when dropped, so an abandoned publish leaves nothing.
struct Incoming(PathBuf);

impl Drop for Incoming {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %self.0.display(), %error,
                "could not remove an abandoned artifact publish; gc leaves it for the operator"
            ),
        }
    }
}

/// Copy `path` of `workspace` into the item's store as `name`, replacing a
/// file or directory stored under it before. Blocking.
pub(crate) fn publish(
    root: &Path,
    item: &QueueItemId,
    workspace: &Path,
    path: &str,
    name: Option<&str>,
    limits: &ArtifactsConfig,
) -> Result<ArtifactListing, ArchonError> {
    let source = inside_workspace(workspace, path)?;
    let default_name = source.file_name().map(|n| n.to_string_lossy().into_owned());
    let name = name
        .map(str::to_owned)
        .or(default_name)
        .ok_or_else(|| ArchonError::Invalid(format!("{path:?} has no name to store it under")))?;
    let stored = stored_name(&name)?;
    let payload = gather(&source, limits)?;
    let _one_at_a_time = PUBLISHING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        !payload.files.is_empty() || source.is_dir(),
        "a file is one"
    );

    let dir = item_dir(root, item);
    let before = stored_files(&dir)?;
    let replaced: Vec<_> = before
        .iter()
        .filter(|f| Path::new(&f.name).starts_with(&stored))
        .collect();
    let kept_bytes: u64 =
        before.iter().map(|f| f.bytes).sum::<u64>() - replaced.iter().map(|f| f.bytes).sum::<u64>();
    let kept_files = before.len() - replaced.len();
    check_room(item, limits, kept_bytes, kept_files, &payload)?;

    std::fs::create_dir_all(&dir)
        .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))?;
    let incoming = Incoming(dir.join(incoming_name()));
    let staged = incoming.0.join("payload");
    let copied = copy_payload(&source, &staged, &payload)?;
    // A file grown since it was measured is caught here, before anything is replaced.
    let copied_payload = Payload {
        bytes: copied,
        files: payload.files,
    };
    check_room(item, limits, kept_bytes, kept_files, &copied_payload)?;
    replace(&dir, &stored, &staged)?;
    drop(incoming);

    let mut done = listing(root, item)?;
    done.published = published_files(&stored, &copied_payload, source.is_dir());
    Ok(done)
}

/// `path` resolved inside `workspace`, as a real path: not the workspace
/// itself, and reached through no symlink that leads out of it.
fn inside_workspace(workspace: &Path, path: &str) -> Result<PathBuf, ArchonError> {
    let root = workspace
        .canonicalize()
        .map_err(|e| ArchonError::io(format!("resolving {}", workspace.display()), e))?;
    let wanted = root.join(path);
    let real = wanted.canonicalize().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            ArchonError::Invalid(format!("{} does not exist", wanted.display()))
        }
        _ => ArchonError::io(format!("resolving {}", wanted.display()), e),
    })?;
    if real == root || !real.starts_with(&root) {
        return Err(ArchonError::Forbidden(format!(
            "{path:?} is not inside your workspace {}: publish a file or directory below it",
            root.display()
        )));
    }
    Ok(real)
}

fn check_room(
    item: &QueueItemId,
    limits: &ArtifactsConfig,
    kept_bytes: u64,
    kept_files: usize,
    payload: &Payload,
) -> Result<(), ArchonError> {
    let total = kept_bytes.saturating_add(payload.bytes);
    if total > limits.max_item_bytes() {
        return Err(ArchonError::Invalid(format!(
            "item {item}'s artifacts would take {total} bytes, over artifacts.max_item_mb ({} MB); \
             publish less, or a smaller file",
            limits.max_item_mb
        )));
    }
    if kept_files + payload.files.len() > MAX_FILES {
        return Err(ArchonError::Invalid(format!(
            "item {item}'s artifacts would hold more than {MAX_FILES} files; publish an archive"
        )));
    }
    Ok(())
}

/// Copy the payload to `staged`; the bytes copied.
fn copy_payload(source: &Path, staged: &Path, payload: &Payload) -> Result<u64, ArchonError> {
    let single = source.is_file();
    let mut copied = 0u64;
    for (relative, _) in &payload.files {
        let from = if single {
            source.to_path_buf()
        } else {
            source.join(relative)
        };
        let to = if single {
            staged.to_path_buf()
        } else {
            staged.join(relative)
        };
        let parent = to.parent().expect("a staged file has a directory");
        std::fs::create_dir_all(parent)
            .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
        copied += std::fs::copy(&from, &to)
            .map_err(|e| ArchonError::io(format!("copying {}", from.display()), e))?;
    }
    if payload.files.is_empty() {
        std::fs::create_dir_all(staged)
            .map_err(|e| ArchonError::io(format!("creating {}", staged.display()), e))?;
    }
    Ok(copied)
}

/// Put `staged` at `dir/stored`, removing what was there.
fn replace(dir: &Path, stored: &Path, staged: &Path) -> Result<(), ArchonError> {
    let target = dir.join(stored);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&target),
        Ok(_) => std::fs::remove_file(&target),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
    .map_err(|e| ArchonError::io(format!("replacing {}", target.display()), e))?;
    let parent = target.parent().expect("a stored name has a directory");
    std::fs::create_dir_all(parent)
        .map_err(|e| ArchonError::io(format!("creating {}", parent.display()), e))?;
    std::fs::rename(staged, &target)
        .map_err(|e| ArchonError::io(format!("storing {}", target.display()), e))
}

fn published_files(stored: &Path, payload: &Payload, directory: bool) -> Vec<ArtifactFile> {
    payload
        .files
        .iter()
        .map(|(relative, bytes)| {
            let name = if directory {
                stored.join(relative)
            } else {
                stored.to_path_buf()
            };
            ArtifactFile {
                name: name.to_string_lossy().into_owned(),
                bytes: *bytes,
            }
        })
        .collect()
}

/// One page of the stored file `name`, as text.
pub(crate) fn read(
    root: &Path,
    item: &QueueItemId,
    name: &str,
    offset: u64,
    limit: Option<u64>,
) -> Result<ArtifactPage, ArchonError> {
    let path = item_dir(root, item).join(stored_name(name)?);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => {
            return Err(ArchonError::Invalid(format!(
                "{name:?} is a directory; list_artifacts names the files in it"
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ArchonError::NotFound(format!(
                "item {item} has no artifact {name:?}; list_artifacts names what it has"
            )));
        }
        Err(e) => return Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
    };
    let total = meta.len();
    if offset > total {
        return Err(ArchonError::Invalid(format!(
            "offset {offset} is past the end of {name:?} ({total} bytes)"
        )));
    }
    let limit = limit
        .unwrap_or(MAX_ARTIFACT_PAGE_BYTES)
        .clamp(1, MAX_ARTIFACT_PAGE_BYTES);
    let bytes = read_range(&path, offset, limit.min(total - offset))?;
    let text = utf8_prefix(&bytes).ok_or_else(|| {
        ArchonError::Invalid(format!(
            "{name:?} is not UTF-8 text at offset {offset}; read the file itself at {}",
            path.display()
        ))
    })?;
    let end = offset + text.len() as u64;
    assert!(end <= total, "a page ends inside the file");
    Ok(ArtifactPage {
        item: item.clone(),
        name: name.to_owned(),
        path,
        offset,
        total_bytes: total,
        text: text.to_owned(),
        next_offset: (end < total).then_some(end),
    })
}

fn read_range(path: &Path, offset: u64, len: u64) -> Result<Vec<u8>, ArchonError> {
    let reading = |e| ArchonError::io(format!("reading {}", path.display()), e);
    let mut file = std::fs::File::open(path).map_err(reading)?;
    file.seek(SeekFrom::Start(offset)).map_err(reading)?;
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes).map_err(reading)?;
    Ok(bytes)
}

/// The text of `bytes`, less a character the page cut off at its end; `None`
/// when it is not UTF-8 at all.
fn utf8_prefix(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => {
            std::str::from_utf8(&bytes[..e.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

/// `publish_artifact`: the calling stage agent's file or directory into its
/// item's store.
pub(crate) async fn publish_for(
    ctx: &DaemonContext,
    caller: Option<&AetherAgentId>,
    path: String,
    name: Option<String>,
) -> Result<Reply, ArchonError> {
    let agent = caller.ok_or_else(|| {
        ArchonError::Forbidden(
            "publish_artifact requires Aether caller metadata (send_caller_meta = true)".into(),
        )
    })?;
    let (item, workspace) = {
        let store = ctx.store.lock().await;
        let stage = store
            .stage_run_by_agent(agent)?
            .filter(|stage| !stage.state.is_terminal())
            .ok_or_else(|| {
                ArchonError::Forbidden(format!(
                    "publish_artifact is for a stage agent while its stage runs, and Archon has \
                     none on record for {agent}. A child of a stage agent gives the file to the \
                     agent that delegated to it, which publishes it"
                ))
            })?;
        let run = store.run(&stage.run)?;
        let spawn = crate::executor::load_spawn(&store, &stage.id)?;
        (run.item, spawn.request.workspace)
    };
    let root = ctx.paths.artifacts();
    let limits = ctx.config.artifacts.clone();
    let stored = tokio::task::spawn_blocking(move || {
        publish(&root, &item, &workspace, &path, name.as_deref(), &limits)
    })
    .await
    .map_err(|e| ArchonError::io("the artifact publish task", std::io::Error::other(e)))??;
    Ok(Reply::Artifacts(Box::new(stored)))
}

pub(crate) async fn list_for(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<Reply, ArchonError> {
    ctx.store.lock().await.item(item)?;
    let (root, item) = (ctx.paths.artifacts(), item.clone());
    let listing = tokio::task::spawn_blocking(move || listing(&root, &item))
        .await
        .map_err(|e| ArchonError::io("the artifact listing task", std::io::Error::other(e)))??;
    Ok(Reply::Artifacts(Box::new(listing)))
}

pub(crate) async fn read_for(
    ctx: &DaemonContext,
    item: &QueueItemId,
    name: String,
    offset: u64,
    limit: Option<u64>,
) -> Result<Reply, ArchonError> {
    ctx.store.lock().await.item(item)?;
    let (root, item) = (ctx.paths.artifacts(), item.clone());
    let page = tokio::task::spawn_blocking(move || read(&root, &item, &name, offset, limit))
        .await
        .map_err(|e| ArchonError::io("the artifact read task", std::io::Error::other(e)))??;
    Ok(Reply::ArtifactPage(Box::new(page)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        workspace: PathBuf,
        item: QueueItemId,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("artifacts");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        Fixture {
            root,
            workspace,
            item: QueueItemId::new(),
            _dir: dir,
        }
    }

    impl Fixture {
        fn write(&self, name: &str, bytes: &[u8]) {
            let path = self.workspace.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        fn publish(
            &self,
            path: &str,
            name: Option<&str>,
            limits: &ArtifactsConfig,
        ) -> Result<ArtifactListing, ArchonError> {
            publish(&self.root, &self.item, &self.workspace, path, name, limits)
        }
    }

    fn limits(max_file_mb: u64, max_item_mb: u64) -> ArtifactsConfig {
        ArtifactsConfig {
            max_file_mb,
            max_item_mb,
            ..ArtifactsConfig::default()
        }
    }

    fn names(listing: &ArtifactListing) -> Vec<&str> {
        listing.files.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn a_published_file_is_listed_and_read_back_by_item_id() {
        let f = fixture();
        f.write("bench/out.json", b"{\"ms\": 12}");
        let done = f.publish("bench/out.json", None, &limits(1, 2)).unwrap();
        assert_eq!(names(&done), ["out.json"]);
        assert_eq!(done.published, done.files);
        assert_eq!(done.dir, f.root.join(f.item.as_str()));
        assert!(has_files(&f.root, &f.item));

        let page = read(&f.root, &f.item, "out.json", 0, None).unwrap();
        assert_eq!(page.text, "{\"ms\": 12}");
        assert_eq!(page.next_offset, None);
        assert_eq!(page.total_bytes, 10);
        assert_eq!(listing(&f.root, &f.item).unwrap().files, done.files);
    }

    #[test]
    fn a_directory_is_stored_under_a_chosen_name_and_replaced_by_the_next_publish() {
        let f = fixture();
        f.write("run/a.log", b"first");
        f.write("run/deep/b.log", b"second");
        let done = f
            .publish("run", Some("evidence/run"), &limits(1, 2))
            .unwrap();
        assert_eq!(
            names(&done),
            ["evidence/run/a.log", "evidence/run/deep/b.log"]
        );

        std::fs::remove_file(f.workspace.join("run/a.log")).unwrap();
        f.write("run/deep/b.log", b"changed");
        let done = f
            .publish("run", Some("evidence/run"), &limits(1, 2))
            .unwrap();
        assert_eq!(
            names(&done),
            ["evidence/run/deep/b.log"],
            "the old copy is replaced"
        );
        let page = read(&f.root, &f.item, "evidence/run/deep/b.log", 0, None).unwrap();
        assert_eq!(page.text, "changed");
    }

    #[test]
    fn a_file_over_the_per_file_limit_is_refused_and_leaves_nothing() {
        let f = fixture();
        f.write("big.bin", &vec![0u8; MIB + 1]);
        let refused = f.publish("big.bin", None, &limits(1, 8)).unwrap_err();
        assert!(refused.to_string().contains("max_file_mb"), "{refused}");
        assert!(!has_files(&f.root, &f.item));
    }

    #[test]
    fn an_item_stays_within_its_total_limit_and_a_replacement_frees_its_old_size() {
        let f = fixture();
        f.write("a.bin", &vec![0u8; MIB]);
        f.write("b.bin", &vec![0u8; MIB]);
        f.write("c.bin", &vec![0u8; MIB]);
        let two = limits(1, 2);
        f.publish("a.bin", None, &two).unwrap();
        f.publish("b.bin", None, &two).unwrap();
        let refused = f.publish("c.bin", None, &two).unwrap_err();
        assert!(refused.to_string().contains("max_item_mb"), "{refused}");

        let again = f.publish("b.bin", None, &two).unwrap();
        assert_eq!(names(&again), ["a.bin", "b.bin"], "republishing b fits");
        assert_eq!(again.total_bytes, 2 * MIB as u64);
        let entries: Vec<_> = std::fs::read_dir(f.root.join(f.item.as_str()))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.iter().all(|n| !is_incoming(n)), "{entries:?}");
    }

    #[test]
    fn items_do_not_share_a_store() {
        let f = fixture();
        f.write("a.txt", b"mine");
        f.publish("a.txt", None, &limits(1, 2)).unwrap();
        let other = QueueItemId::new();
        assert!(!has_files(&f.root, &other));
        assert!(listing(&f.root, &other).unwrap().files.is_empty());
        let missing = read(&f.root, &other, "a.txt", 0, None).unwrap_err();
        assert!(matches!(missing, ArchonError::NotFound(_)), "{missing:?}");
    }

    #[test]
    fn paths_outside_the_workspace_and_symlinks_are_refused() {
        let f = fixture();
        let outside = f.root.parent().unwrap().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let any = limits(1, 2);

        let by_dots = f.publish("../secret.txt", None, &any).unwrap_err();
        assert!(matches!(by_dots, ArchonError::Forbidden(_)), "{by_dots:?}");
        let absolute = f
            .publish(outside.to_str().unwrap(), None, &any)
            .unwrap_err();
        assert!(
            matches!(absolute, ArchonError::Forbidden(_)),
            "{absolute:?}"
        );
        let whole = f.publish(".", None, &any).unwrap_err();
        assert!(matches!(whole, ArchonError::Forbidden(_)), "{whole:?}");

        std::os::unix::fs::symlink(&outside, f.workspace.join("link.txt")).unwrap();
        let linked = f.publish("link.txt", None, &any).unwrap_err();
        assert!(matches!(linked, ArchonError::Forbidden(_)), "{linked:?}");
        f.write("dir/ok.txt", b"ok");
        std::os::unix::fs::symlink(&outside, f.workspace.join("dir/link.txt")).unwrap();
        let inside_dir = f.publish("dir", None, &any).unwrap_err();
        assert!(inside_dir.to_string().contains("symlink"), "{inside_dir}");
        assert!(!has_files(&f.root, &f.item));

        let missing = f.publish("nope.txt", None, &any).unwrap_err();
        assert!(matches!(missing, ArchonError::Invalid(_)), "{missing:?}");
    }

    #[test]
    fn stored_names_are_plain_relative_paths() {
        for bad in [
            "",
            "/abs",
            "../up",
            "a/../b",
            "./a",
            ".incoming-1",
            "a/.incoming-2",
            "a\nb",
        ] {
            assert!(stored_name(bad).is_err(), "{bad:?}");
        }
        assert!(stored_name(&"x/".repeat(MAX_DEPTH + 1)).is_err());
        assert!(stored_name(&"n".repeat(MAX_NAME_BYTES + 1)).is_err());
        assert!(stored_name("evidence/run 1/out.json").is_ok());
        let f = fixture();
        let escape = read(&f.root, &f.item, "../x", 0, None).unwrap_err();
        assert!(matches!(escape, ArchonError::Invalid(_)), "{escape:?}");
    }

    #[test]
    fn a_long_file_is_read_in_pages_that_never_split_a_character() {
        let f = fixture();
        let text = "é".repeat(10);
        f.write("long.txt", text.as_bytes());
        f.publish("long.txt", None, &limits(1, 2)).unwrap();
        let mut collected = String::new();
        let mut offset = 0;
        loop {
            let page = read(&f.root, &f.item, "long.txt", offset, Some(5)).unwrap();
            collected.push_str(&page.text);
            match page.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(collected, text);
    }

    #[test]
    fn binary_files_and_bad_offsets_are_refused_with_the_path() {
        let f = fixture();
        f.write("blob.bin", &[0xff, 0xfe, 0x00]);
        f.publish("blob.bin", None, &limits(1, 2)).unwrap();
        let binary = read(&f.root, &f.item, "blob.bin", 0, None).unwrap_err();
        assert!(binary.to_string().contains("not UTF-8"), "{binary}");
        assert!(binary.to_string().contains("blob.bin"), "{binary}");
        let past = read(&f.root, &f.item, "blob.bin", 99, None).unwrap_err();
        assert!(matches!(past, ArchonError::Invalid(_)), "{past:?}");
        let dir = read(&f.root, &f.item, "", 0, None).unwrap_err();
        assert!(matches!(dir, ArchonError::Invalid(_)), "{dir:?}");
    }

    #[test]
    fn a_page_is_capped_at_the_page_limit() {
        let f = fixture();
        f.write(
            "big.txt",
            &vec![b'a'; MAX_ARTIFACT_PAGE_BYTES as usize + 10],
        );
        f.publish("big.txt", None, &limits(1, 2)).unwrap();
        let page = read(&f.root, &f.item, "big.txt", 0, Some(u64::MAX)).unwrap();
        assert_eq!(page.text.len() as u64, MAX_ARTIFACT_PAGE_BYTES);
        assert_eq!(page.next_offset, Some(MAX_ARTIFACT_PAGE_BYTES));
    }
}
