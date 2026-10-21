//! The crash marker: why archond last stopped on a panic or a fatal error.
//! The daemon log lives under the runtime directory (tmpfs), so it is gone
//! after a reboot; the marker lives under the data directory and survives.
//!
//! `pending.json` is written when archond panics or stops on an error. The
//! next start reports it once and moves it to `last.json`, which `archon
//! status` and `archon doctor` keep showing. Each file holds one record
//! whose text is bounded, so the pair never grows.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

/// Longest message kept, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 4096;
const DIR: &str = "crash";

/// Why archond stopped, when and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashRecord {
    /// `panic` or `fatal`.
    pub kind: String,
    pub message: String,
    pub location: Option<String>,
    pub version: String,
    pub at: crate::Timestamp,
}

impl CrashRecord {
    pub fn new(kind: &str, message: &str, location: Option<String>) -> Self {
        assert!(matches!(kind, "panic" | "fatal"), "a known kind");
        Self {
            kind: kind.to_owned(),
            message: bounded(message),
            location: location.map(|l| bounded(&l)),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            at: crate::now(),
        }
    }

    /// One friendly paragraph: what happened and the command to run next.
    pub fn describe(&self) -> String {
        let what = if self.kind == "panic" {
            "recorded a panic"
        } else {
            "stopped on an error"
        };
        let place = self
            .location
            .as_ref()
            .map_or_else(String::new, |l| format!(" at {l}"));
        format!(
            "archond {what}{place} on {} (version {}): {}. A panic archond isolated leaves it \
             running; if it stopped, `archon daemon start` starts it again. The log lines \
             before it are in archond.log under the runtime directory",
            self.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            self.version,
            self.message.trim(),
        )
    }
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (cut)", &text[..end])
}

pub fn pending_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR).join("pending.json")
}

pub fn last_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR).join("last.json")
}

/// Write `record` as the pending marker, replacing an earlier one. Safe to
/// call from a panic hook: it allocates little and never panics.
pub fn write_pending(data_dir: &Path, record: &CrashRecord) -> Result<(), ArchonError> {
    let path = pending_path(data_dir);
    let dir = path.parent().expect("the marker has a directory");
    std::fs::create_dir_all(dir).map_err(|e| ArchonError::io("create the crash directory", e))?;
    let text = serde_json::to_string(record)?;
    assert!(text.len() < 4 * MAX_MESSAGE_BYTES, "a bounded record");
    let tmp = dir.join("pending.json.tmp");
    let mut file =
        std::fs::File::create(&tmp).map_err(|e| ArchonError::io("write the crash marker", e))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| ArchonError::io("write the crash marker", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| ArchonError::io("store the crash marker", e))
}

fn read(path: &Path) -> Result<Option<CrashRecord>, ArchonError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ArchonError::io("read the crash marker", e)),
    };
    serde_json::from_str(&raw).map(Some).map_err(|e| {
        ArchonError::Invalid(format!(
            "the crash marker {} is unreadable ({e}); delete it with `rm {}`",
            path.display(),
            path.display()
        ))
    })
}

/// The marker not yet reported by a start of archond.
pub fn read_pending(data_dir: &Path) -> Result<Option<CrashRecord>, ArchonError> {
    read(&pending_path(data_dir))
}

/// The newest crash, reported or not.
pub fn read_latest(data_dir: &Path) -> Result<Option<CrashRecord>, ArchonError> {
    match read_pending(data_dir)? {
        Some(pending) => Ok(Some(pending)),
        None => read(&last_path(data_dir)),
    }
}

/// The pending marker was reported: keep it as the last crash.
pub fn acknowledge(data_dir: &Path) -> Result<(), ArchonError> {
    let pending = pending_path(data_dir);
    assert!(pending.starts_with(data_dir), "inside the data directory");
    std::fs::rename(&pending, last_path(data_dir))
        .map_err(|e| ArchonError::io("move the crash marker", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_marker_is_read_once_then_kept_as_the_last_crash() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_latest(dir.path()).unwrap(), None);
        let record = CrashRecord::new("panic", "boom", Some("src/x.rs:3:9".into()));
        write_pending(dir.path(), &record).unwrap();
        assert_eq!(read_pending(dir.path()).unwrap(), Some(record.clone()));
        acknowledge(dir.path()).unwrap();
        assert_eq!(read_pending(dir.path()).unwrap(), None);
        assert_eq!(read_latest(dir.path()).unwrap(), Some(record));
    }

    #[test]
    fn the_message_is_bounded_on_a_character_boundary() {
        let record = CrashRecord::new("fatal", &"é".repeat(MAX_MESSAGE_BYTES), None);
        assert!(record.message.len() <= MAX_MESSAGE_BYTES + 16);
        assert!(record.message.ends_with("(cut)"));
    }

    #[test]
    fn an_unreadable_marker_says_how_to_remove_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(DIR)).unwrap();
        std::fs::write(pending_path(dir.path()), "{").unwrap();
        let error = read_pending(dir.path()).unwrap_err().to_string();
        assert!(error.contains("rm "), "{error}");
    }

    #[test]
    fn the_description_names_the_place_and_the_next_command() {
        let record = CrashRecord::new("panic", "assertion failed", Some("a.rs:1:2".into()));
        let text = record.describe();
        assert!(text.contains("a.rs:1:2") && text.contains("assertion failed"));
        assert!(text.contains("archon daemon start"));
    }
}
