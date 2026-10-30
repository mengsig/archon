//! A ship record whose `commit` or `tree` is an abbreviated hash, as
//! `git commit` prints one, names its object as surely as the full hash once
//! git resolves it in the run worktree to exactly one object of that type.
//! archond stores the full id, so every later check compares full ids; one
//! that resolves to nothing, or to more than one object, is left for the gate
//! to refuse. Blocking git that runs no program the worktree's config names.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_pipeline::ship::{RECORD_IDS, is_abbreviated_id};
use serde_json::Value;

use crate::worktree::daemon_git::DaemonGit;

/// `data` with each abbreviated record id git resolves in `workspace`
/// replaced by its full id, lowercase.
pub(super) fn resolve(workspace: &Path, data: &mut Value) -> Result<(), ArchonError> {
    assert!(workspace.is_absolute(), "run worktrees are absolute");
    let Some(fields) = data.as_object_mut() else {
        return Ok(());
    };
    for (key, kind) in RECORD_IDS {
        let Some(id) = fields.get(key).and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_abbreviated_id(id) {
            continue;
        }
        let prefix = id.to_ascii_lowercase();
        if let Some(full) = full_id(workspace, &prefix, kind)? {
            fields.insert(key.to_owned(), Value::String(full));
        }
    }
    Ok(())
}

/// The one object of `kind` whose id starts with `prefix`; `None` when git
/// knows none or more than one, or the prefix names an object of another
/// type (a commit's prefix given for its tree resolves to an id it does not
/// start).
fn full_id(workspace: &Path, prefix: &str, kind: &str) -> Result<Option<String>, ArchonError> {
    assert!(
        is_abbreviated_id(prefix),
        "only an abbreviation is resolved"
    );
    assert!(matches!(kind, "commit" | "tree"), "a record names these");
    let spec = format!("{prefix}^{{{kind}}}");
    let ran = DaemonGit::sealed_in(workspace)
        .args(["rev-parse", "--verify", "--quiet", spec.as_str()])
        .output()?;
    // Unknown or ambiguous: the gate's refusal says so, and a repair can fix it.
    if !ran.status.success() {
        return Ok(None);
    }
    let full = ran.stdout_text()?;
    if !matches!(full.len(), 40 | 64) || !full.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ArchonError::Protocol(format!(
            "git rev-parse {spec} printed {full:?}, not an object id"
        )));
    }
    Ok(full.starts_with(prefix).then_some(full))
}
