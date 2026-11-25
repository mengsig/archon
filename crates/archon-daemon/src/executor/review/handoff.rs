use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::id::ReportId;
use rustix::fs::{Mode, OFlags, mkdirat, open, openat, renameat, unlinkat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const INLINE_BYTES: usize = 2_048;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The inline list or a worktree-relative link to the complete list.
pub(super) fn findings(workspace: &Path, list: &[(&str, &str)]) -> Result<String, ArchonError> {
    let (prompt, file) = prepare(list);
    if let Some(file) = file {
        file.install(workspace)?;
    }
    Ok(prompt)
}

pub(super) fn prepare(list: &[(&str, &str)]) -> (String, Option<FindingsFile>) {
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
        return (text, None);
    }
    let file = FindingsFile { text };
    let name = file.name();
    let prompt = format!(
        "Read all finding IDs and descriptions in `.archon/reports/{name}` in the shared worktree. \
         Cite that file in the reviewer prompt so the reviewer can read the full list."
    );
    (prompt, Some(file))
}

/// A large handoff reserved with its objective and installed before sending it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::executor) struct FindingsFile {
    text: String,
}

impl FindingsFile {
    pub(in crate::executor) fn install(&self, workspace: &Path) -> Result<(), ArchonError> {
        write_list(workspace, &self.name(), &self.text)
            .map_err(|e| ArchonError::io("writing the complete review finding list", e))
    }

    fn name(&self) -> String {
        format!("review-findings-{}.txt", digest(&self.text))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn concurrent_publication_leaves_one_complete_file_per_list() {
        let workspace = tempfile::tempdir().unwrap();
        let (result, received) = mpsc::channel();
        let publishers: Vec<_> = ['a', 'b', 'a', 'b']
            .into_iter()
            .map(|label| {
                let path = workspace.path().to_owned();
                let result = result.clone();
                std::thread::spawn(move || {
                    let description = label.to_string().repeat(INLINE_BYTES);
                    let prompt = findings(&path, &[("agt_test:1", &description)]).unwrap();
                    let relative = prompt.split('`').nth(1).expect("the linked list");
                    let full = std::fs::read_to_string(path.join(relative)).unwrap();
                    result.send((description, full)).unwrap();
                })
            })
            .collect();
        for _ in 0..publishers.len() {
            let (description, full) = received
                .recv_timeout(Duration::from_secs(5))
                .expect("bounded concurrent writes");
            assert_eq!(full, format!("- agt_test:1: {description}\n"));
        }
        for publisher in publishers {
            publisher.join().unwrap();
        }
        assert_eq!(
            workspace
                .path()
                .join(".archon/reports")
                .read_dir()
                .unwrap()
                .count(),
            2
        );
    }
}
