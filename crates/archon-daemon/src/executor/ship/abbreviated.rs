//! A ship record whose `commit` or `tree` is an abbreviated hash, as
//! `git commit` prints one, names its object as surely as the full hash once
//! git resolves it in the run worktree to exactly one object of that type.
//! archond stores the full id, so every later check compares full ids; one
//! that resolves to nothing, or to more than one object, is left for the gate
//! to refuse. 40 digits are resolved too: in a SHA-256 worktree they abbreviate,
//! so there an unresolved 40-digit id is marked for the gate to refuse.
//! Blocking git that runs no program the worktree's config names.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_pipeline::ship::{RECORD_IDS, may_abbreviate};
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
        if !may_abbreviate(id) {
            continue;
        }
        let prefix = id.to_ascii_lowercase();
        if let Some(full) = full_id(workspace, &prefix, kind)? {
            fields.insert(key.to_owned(), Value::String(full));
        } else if prefix.len() == 40 && is_sha256(workspace)? {
            let marked = format!("{id} (names no single {kind} in this SHA-256 worktree)");
            fields.insert(key.to_owned(), Value::String(marked));
        }
    }
    Ok(())
}

/// Whether `workspace` stores SHA-256 object ids, where 40 digits abbreviate.
fn is_sha256(workspace: &Path) -> Result<bool, ArchonError> {
    let ran = DaemonGit::sealed_in(workspace)
        .args(["rev-parse", "--show-object-format"])
        .output()?;
    let format = ran.stdout_text()?;
    if !ran.status.success() || !matches!(format.as_str(), "sha1" | "sha256") {
        return Err(ArchonError::Protocol(format!(
            "git rev-parse --show-object-format in {} printed {format:?}",
            workspace.display()
        )));
    }
    Ok(format == "sha256")
}

/// The one object of `kind` whose id starts with `prefix`; `None` when git
/// knows none or more than one, or the prefix names an object of another
/// type (a commit's prefix given for its tree resolves to an id it does not
/// start).
fn full_id(workspace: &Path, prefix: &str, kind: &str) -> Result<Option<String>, ArchonError> {
    assert!(may_abbreviate(prefix), "only an abbreviation is resolved");
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git(dir: &Path, args: &[&str]) -> String {
        let identity = ["-c", "user.name=t", "-c", "user.email=t@t.invalid"];
        let ran = std::process::Command::new("git")
            .args(identity)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(ran.status.success(), "git {args:?}: {ran:?}");
        String::from_utf8(ran.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    fn repo(format: &str) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let format = format!("--object-format={format}");
        git(dir.path(), &["init", "--quiet", format.as_str()]);
        git(
            dir.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "one"],
        );
        let commit = git(dir.path(), &["rev-parse", "HEAD"]);
        let tree = git(dir.path(), &["rev-parse", "HEAD^{tree}"]);
        (dir, commit, tree)
    }

    #[test]
    fn forty_digits_of_a_sha256_id_resolve_to_the_full_id() {
        let (dir, commit, tree) = repo("sha256");
        assert_eq!((commit.len(), tree.len()), (64, 64));
        let mut data = json!({"commit": commit[..40], "tree": tree[..40].to_uppercase()});
        resolve(dir.path(), &mut data).expect("git");
        assert_eq!(data, json!({"commit": commit, "tree": tree}));
    }

    #[test]
    fn forty_digits_naming_nothing_in_a_sha256_worktree_are_refused() {
        let (dir, commit, tree) = repo("sha256");
        let unknown = "0".repeat(40);
        let mut data = json!({"commit": unknown, "tree": tree, "pr": {"title": "t", "body": "b"}});
        resolve(dir.path(), &mut data).expect("git");
        assert_ne!(data["commit"], json!(unknown), "marked, not taken as SHA-1");
        let refused = archon_pipeline::ship::ShipRecord::read(Some(&data)).unwrap_err();
        assert!(refused.contains("SHA-256"), "{refused}");
        data["commit"] = json!(commit);
        assert!(archon_pipeline::ship::ShipRecord::read(Some(&data)).is_ok());
    }

    #[test]
    fn a_full_sha1_id_stays_and_an_unknown_one_is_left_for_the_gate() {
        let (dir, commit, _) = repo("sha1");
        let unknown = "0".repeat(40);
        let mut data = json!({"commit": commit, "tree": unknown});
        resolve(dir.path(), &mut data).expect("git");
        assert_eq!(data, json!({"commit": commit, "tree": unknown}));
    }
}
