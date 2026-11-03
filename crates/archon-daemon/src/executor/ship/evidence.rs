//! The screenshots a delivered pull request shows (ARCHITECTURE §8.7). The
//! files a record stage names under `.archon/evidence/` or `.archon/reports/`
//! of its run worktree, which a delivery refuses to commit, go into a
//! commit of their own, never on the run branch, pushed to a new ref under
//! `refs/archon/evidence/<item>/` of the same repository with the
//! delivery's own credentials; the body links each file by that commit, so
//! a private repository shows it to whoever may read the repository. A file
//! that cannot be attached is told in the section, and the delivery goes on.

use std::io::Read;
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::pull_request::PullRequestRef;
use archon_core::repo::Repo;
use archon_pipeline::evidence::{Evidence, FILE_ROOTS, Hosted};

use super::remote;
use crate::context::DaemonContext;
use crate::worktree::daemon_git::Config;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack::{EVIDENCE_REFS, is_commit_id};

/// The largest screenshot uploaded, and all of one delivery's together.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 20 * 1024 * 1024;

/// Where a delivery's screenshot files are read from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Source<'a> {
    pub workspace: &'a Path,
    pub item: &'a QueueItemId,
}

/// One file ready to upload: its path as the agent named it, where it lies,
/// and the name it gets in the evidence commit.
struct Upload {
    named: String,
    file: PathBuf,
    name: String,
}

/// Upload every file `evidence` names from `source` for the pull request at
/// `pr_url`; each file's link, or why it could not be attached. Nothing here
/// fails the delivery.
pub(crate) async fn host(
    ctx: &DaemonContext,
    repo: &Repo,
    source: Option<Source<'_>>,
    pr_url: &str,
    evidence: &Evidence,
) -> Hosted {
    let files: Vec<String> = evidence.files().into_iter().map(str::to_owned).collect();
    if files.is_empty() {
        return Hosted::new();
    }
    let all_failed = |why: &str| -> Hosted {
        files
            .iter()
            .map(|f| (f.clone(), Err(why.to_owned())))
            .collect()
    };
    let Some(source) = source else {
        return all_failed("no run worktree holds it");
    };
    let Some(pr) = PullRequestRef::from_url(pr_url) else {
        return all_failed("the pull request's URL names no GitHub repository");
    };
    let (mut hosted, uploads) = readable(source.workspace, &files);
    if uploads.is_empty() {
        return hosted;
    }
    let uploaded = upload(ctx, repo, source.item, &uploads).await;
    for upload in &uploads {
        let link = match &uploaded {
            Ok(commit) => Ok(format!(
                "https://{}/{}/blob/{commit}/{}?raw=true",
                pr.host, pr.repository, upload.name
            )),
            Err(why) => Err(why.clone()),
        };
        hosted.insert(upload.named.clone(), link);
    }
    match &uploaded {
        Ok(commit) => tracing::info!(%pr_url, %commit, files = uploads.len(), "evidence hosted"),
        Err(why) => tracing::warn!(%pr_url, %why, "evidence could not be hosted"),
    }
    debug_assert_eq!(hosted.len(), files.len(), "every file is told");
    hosted
}

/// Commit `uploads` and push the commit; the commit, or why not.
async fn upload(
    ctx: &DaemonContext,
    repo: &Repo,
    item: &QueueItemId,
    uploads: &[Upload],
) -> Result<String, String> {
    assert!(!uploads.is_empty(), "something to upload");
    let (data_dir, path) = (ctx.paths.data_dir.clone(), repo.path.clone());
    let files: Vec<(String, PathBuf)> = uploads
        .iter()
        .map(|u| (u.name.clone(), u.file.clone()))
        .collect();
    let message = format!("Archon evidence for {item}");
    let committed = tokio::task::spawn_blocking(move || {
        let shadow = Shadow::of(&data_dir, &path)?;
        commit_files(&shadow, &files, &message)
    })
    .await
    .map_err(|e| format!("the evidence commit task failed: {e}"))?
    .map_err(|e| format!("the evidence commit could not be made: {e}"))?;
    let target = format!("{EVIDENCE_REFS}{item}/{committed}");
    let binary = &ctx.config.github.binary;
    match remote::push_evidence(binary, &ctx.paths.data_dir, repo, &committed, &target).await {
        Ok(Ok(())) => Ok(committed),
        Ok(Err(refusal)) => Err(refusal),
        Err(error) => Err(format!("pushing `{target}` failed: {error}")),
    }
}

/// The files of `files` that may be uploaded, and why each other one may not.
fn readable(workspace: &Path, files: &[String]) -> (Hosted, Vec<Upload>) {
    let mut refused = Hosted::new();
    let mut uploads = Vec::new();
    let mut total = 0;
    for named in files {
        match image_file(workspace, named) {
            Ok((file, extension, size)) if total + size <= MAX_TOTAL_BYTES => {
                total += size;
                let name = format!("{}.{extension}", uploads.len() + 1);
                let named = named.clone();
                uploads.push(Upload { named, file, name });
            }
            Ok(_) => {
                let why = format!("the screenshots together pass {MAX_TOTAL_BYTES} bytes");
                refused.insert(named.clone(), Err(why));
            }
            Err(why) => {
                refused.insert(named.clone(), Err(why));
            }
        }
    }
    assert!(total <= MAX_TOTAL_BYTES, "the uploads fit the cap");
    (refused, uploads)
}

/// `named`, resolved in `workspace`, when it is an image file inside the
/// one of [`FILE_ROOTS`] it names even past symlinks: its path, type and size.
fn image_file(workspace: &Path, named: &str) -> Result<(PathBuf, &'static str, u64), String> {
    assert!(workspace.is_absolute(), "a run worktree");
    let found = FILE_ROOTS.iter().find(|root| named.starts_with(*root));
    let Some(within) = found else {
        panic!("checked when the result was read: {named}")
    };
    let canonical = |path: &Path| path.canonicalize().map_err(|e| e.to_string());
    let tree = canonical(workspace).map_err(|e| format!("the worktree is unreadable: {e}"))?;
    let root = canonical(&workspace.join(within))
        .map_err(|e| format!("the worktree has no readable `{within}`: {e}"))?;
    let file = canonical(&workspace.join(named)).map_err(|e| format!("it is not there: {e}"))?;
    if !root.starts_with(&tree) || !file.starts_with(&root) {
        return Err(format!("it leads out of `{within}` of the worktree"));
    }
    let meta = std::fs::metadata(&file).map_err(|e| format!("it is unreadable: {e}"))?;
    if !meta.is_file() {
        return Err("it is not a file".into());
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!("it passes {MAX_IMAGE_BYTES} bytes"));
    }
    let mut head = [0u8; 12];
    let mut opened = std::fs::File::open(&file).map_err(|e| format!("it is unreadable: {e}"))?;
    let read = opened
        .read(&mut head)
        .map_err(|e| format!("it is unreadable: {e}"))?;
    let Some(extension) = image_type(&head[..read]) else {
        return Err("it is not a PNG, JPEG, GIF or WebP image".into());
    };
    Ok((file, extension, meta.len()))
}

/// The image type `head`, a file's first bytes, names.
fn image_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("gif");
    }
    let webp = head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP";
    webp.then_some("webp")
}

/// A commit holding `files` as `(name, path)`, which no branch names; the
/// same files make the same commit, so a repeated delivery pushes it once.
fn commit_files(
    shadow: &Shadow,
    files: &[(String, PathBuf)],
    message: &str,
) -> Result<String, ArchonError> {
    assert!(!files.is_empty(), "a commit of something");
    let index = shadow.scratch_index();
    let tree = write_tree(shadow, &index, files);
    let removed = match std::fs::remove_file(&index) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(ArchonError::io(format!("removing {}", index.display()), e))
        }
        _ => Ok(()),
    };
    let tree = tree?;
    removed?;
    let commit = shadow
        .git(None, Config::Sealed)?
        .env("GIT_AUTHOR_NAME", "Archon")
        .env("GIT_AUTHOR_EMAIL", "archon@localhost")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Archon")
        .env("GIT_COMMITTER_EMAIL", "archon@localhost")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .args(["commit-tree", "--no-gpg-sign", &tree, "-m", message])
        .text()?;
    assert!(is_commit_id(&commit), "commit-tree prints the commit");
    Ok(commit)
}

/// The tree of `files`, written through the scratch `index`.
fn write_tree(
    shadow: &Shadow,
    index: &Path,
    files: &[(String, PathBuf)],
) -> Result<String, ArchonError> {
    let index = index
        .to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", index.display())))?;
    let git = || -> Result<_, ArchonError> {
        Ok(shadow
            .git(None, Config::Sealed)?
            .env("GIT_INDEX_FILE", index))
    };
    for (name, file) in files {
        assert!(!name.contains('/') && !name.is_empty(), "a flat name");
        let file = file
            .to_str()
            .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", file.display())))?;
        let blob = git()?
            .args(["hash-object", "-w", "--no-filters", "--", file])
            .text()?;
        let entry = format!("100644,{blob},{name}");
        git()?
            .args(["update-index", "--add", "--cacheinfo", &entry])
            .text()?;
    }
    let tree = git()?.args(["write-tree"]).text()?;
    assert!(is_commit_id(&tree), "write-tree prints the tree");
    Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_image_bytes_name_an_image_type() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n\0\0\0\0"), Some("png"));
        assert_eq!(image_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(image_type(b"GIF89a......"), Some("gif"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBP"), Some("webp"));
        assert_eq!(image_type(b"-----BEGIN OPENSSH"), None);
        assert_eq!(image_type(b""), None);
    }

    #[test]
    fn a_file_outside_the_evidence_root_or_not_an_image_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        let shots = tree.join(".archon/evidence");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("a.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
        std::fs::write(shots.join("fake.png"), b"secret key").unwrap();
        std::fs::write(dir.path().join("secret.png"), b"\x89PNG\r\n\x1a\nkey").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.png"), shots.join("link.png")).unwrap();
        let files: Vec<String> = ["a.png", "fake.png", "link.png", "gone.png"]
            .iter()
            .map(|f| format!(".archon/evidence/{f}"))
            .collect();
        let (refused, uploads) = readable(&tree, &files);
        assert_eq!(uploads.len(), 1);
        assert_eq!(
            (uploads[0].named.as_str(), uploads[0].name.as_str()),
            (files[0].as_str(), "1.png")
        );
        let why = |n: usize| refused[&files[n]].clone().unwrap_err();
        assert!(why(1).contains("not a PNG"), "{}", why(1));
        assert!(
            why(2).contains("leads out of `.archon/evidence/`"),
            "{}",
            why(2)
        );
        assert!(why(3).contains("it is not there"), "{}", why(3));
    }
}
