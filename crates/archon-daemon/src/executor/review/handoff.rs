use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::ReportId;
use rustix::fs::{Mode, OFlags, mkdirat, open, openat, renameat, unlinkat};
use sha2::{Digest, Sha256};

const INLINE_BYTES: usize = 2_048;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The inline list or a worktree-relative link to the complete list.
pub(super) fn findings(workspace: &Path, list: &[(&str, &str)]) -> Result<String, ArchonError> {
    assert!(!list.is_empty());
    assert!(
        list.iter()
            .all(|(id, text)| !id.is_empty() && !text.is_empty())
    );
    let mut text = String::new();
    for (id, description) in list {
        writeln!(&mut text, "- {id}: {description}").expect("writing to a String");
    }
    if text.len() <= INLINE_BYTES {
        return Ok(text);
    }
    let name = format!("review-findings-{}.txt", digest(&text));
    write_list(workspace, &name, &text)
        .map_err(|e| ArchonError::io("writing the complete review finding list", e))?;
    Ok(format!(
        "Read all finding IDs and descriptions in `.archon/reports/{name}` in the shared worktree. \
         Cite that file in the reviewer prompt so the reviewer can read the full list."
    ))
}

pub(super) fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_list(workspace: &Path, name: &str, text: &str) -> io::Result<()> {
    assert!(workspace.is_absolute());
    assert!(text.len() > INLINE_BYTES && !name.contains('/'));
    let worktree = open(workspace, DIRECTORY, Mode::empty())?;
    let archon = subdirectory(&File::from(worktree), ".archon")?;
    let reports = subdirectory(&archon, "reports")?;
    let temporary = format!(".review-findings-{}", ReportId::new());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
    let mut file = File::from(openat(
        &reports,
        temporary.as_str(),
        flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    let result = file.write_all(text.as_bytes()).and_then(|()| {
        renameat(&reports, temporary.as_str(), &reports, name).map_err(io::Error::from)
    });
    if result.is_err()
        && let Err(error) = unlinkat(&reports, temporary.as_str(), rustix::fs::AtFlags::empty())
    {
        tracing::warn!(%error, %temporary, "could not remove incomplete review findings file");
    }
    result
}

fn subdirectory(parent: &File, name: &str) -> io::Result<File> {
    assert!(!name.is_empty() && !name.contains('/'));
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(File::from(openat(parent, name, DIRECTORY, Mode::empty())?))
}
