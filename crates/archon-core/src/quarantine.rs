//! Quarantined records: items, runs and initiatives whose stored state archond
//! could not use. Each is set aside with why and the commands that fix or
//! cancel it, so a start never stops on one record. The files live under the
//! data directory, one per record, so `archon doctor` reads them even while
//! archond is down; archond rebuilds the set at every start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

const DIR: &str = "quarantine";
/// Longest reason kept, in bytes.
const MAX_REASON_BYTES: usize = 2048;

/// One record archond could not use, and what the operator can do about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantined {
    /// `run`, `item` or `initiative`.
    pub kind: String,
    pub id: String,
    pub reason: String,
    /// The command that puts the record right, once its cause is fixed.
    pub fix: String,
    /// The command that gives the record up.
    pub cancel: String,
    pub at: crate::Timestamp,
}

impl Quarantined {
    pub fn new(kind: &str, id: &str, reason: &str, fix: &str, cancel: &str) -> Self {
        assert!(
            !kind.is_empty() && !id.is_empty(),
            "a quarantined record is named"
        );
        let mut end = reason.len().min(MAX_REASON_BYTES);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            kind: kind.to_owned(),
            id: id.to_owned(),
            reason: reason[..end].replace(char::is_control, " "),
            fix: fix.to_owned(),
            cancel: cancel.to_owned(),
            at: crate::now(),
        }
    }

    /// One line for `archon doctor`: the record, why, and both commands.
    pub fn describe(&self) -> String {
        format!(
            "{} {} is set aside: {}. To fix it: `{}`; to give it up: `{}`",
            self.kind,
            self.id,
            self.reason.trim_end_matches('.'),
            self.fix,
            self.cancel
        )
    }
}

fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

fn file_name(kind: &str, id: &str) -> String {
    let safe = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!("{}-{}.json", safe(kind), safe(id))
}

/// Remember `record`, replacing an earlier one for the same record.
pub fn write(data_dir: &Path, record: &Quarantined) -> Result<(), ArchonError> {
    let dir = dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ArchonError::io("create the quarantine directory", e))?;
    let path = dir.join(file_name(&record.kind, &record.id));
    let tmp = path.with_extension("tmp");
    let text = serde_json::to_string(record)?;
    std::fs::write(&tmp, text).map_err(|e| ArchonError::io("write a quarantine record", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| ArchonError::io("store a quarantine record", e))
}

/// Forget the record for `kind` and `id`, if there is one.
pub fn clear(data_dir: &Path, kind: &str, id: &str) -> Result<(), ArchonError> {
    match std::fs::remove_file(dir(data_dir).join(file_name(kind, id))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ArchonError::io("remove a quarantine record", e)),
    }
}

/// Forget every record: archond calls it at a start, before it sets aside the
/// records that are still unusable.
pub fn clear_all(data_dir: &Path) -> Result<(), ArchonError> {
    match std::fs::remove_dir_all(dir(data_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ArchonError::io("clear the quarantine directory", e)),
    }
}

/// Every quarantined record, oldest first. A file that cannot be read is
/// itself listed, with the command that removes it.
pub fn list(data_dir: &Path) -> Result<Vec<Quarantined>, ArchonError> {
    let entries = match std::fs::read_dir(dir(data_dir)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ArchonError::io("read the quarantine directory", e)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| ArchonError::io("read the quarantine directory", e))?
            .path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|raw| serde_json::from_str::<Quarantined>(&raw).map_err(|e| e.to_string()));
        found.push(parsed.unwrap_or_else(|why| {
            let remove = format!("rm {}", path.display());
            Quarantined::new(
                "file",
                &path.display().to_string(),
                &format!("the quarantine record is unreadable ({why})"),
                &remove,
                &remove,
            )
        }));
    }
    found.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_written_listed_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
        let record = Quarantined::new(
            "run",
            "run_x",
            "bad var",
            "archon queue retry qi_a",
            "archon queue cancel qi_a",
        );
        write(dir.path(), &record).unwrap();
        write(dir.path(), &record).unwrap();
        assert_eq!(list(dir.path()).unwrap(), std::slice::from_ref(&record));
        assert!(record.describe().contains("archon queue cancel qi_a"));
        clear(dir.path(), "run", "run_x").unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
        write(dir.path(), &record).unwrap();
        clear_all(dir.path()).unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_record_is_listed_with_the_command_that_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(super::dir(dir.path())).unwrap();
        std::fs::write(super::dir(dir.path()).join("run-x.json"), "{not json").unwrap();
        let listed = list(dir.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].fix.starts_with("rm "));
    }
}
