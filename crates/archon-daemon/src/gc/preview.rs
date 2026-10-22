//! What a pass would delete from run worktrees, decided as [`super::sweep`]
//! decides it but deleting nothing.

use std::io::ErrorKind;
use std::path::Path;

use archon_core::protocol::{GcReport, WouldDelete};

use super::plan::{Action, Candidate, ItemPlan, Kind};
use super::sweep::{is_inside, is_listed_worktree, unremovable};
use super::{Policy, Roots, cache};

/// The report of a pass over `plans` that removes nothing: `removed` holds
/// the worktrees it would remove, `would_delete` the build output it would
/// delete from the others. Shared caches and artifacts are not looked at.
pub(super) fn preview(plans: &[ItemPlan], roots: &Roots, policy: &Policy) -> GcReport {
    let mut report = GcReport::default();
    for found in plans.iter().map(|plan| &plan.found) {
        for (path, why) in &found.unreadable {
            report.failed.push((path.clone(), why.clone()));
        }
        for candidate in &found.candidates {
            preview_candidate(candidate, roots, policy, &mut report);
        }
    }
    assert!(report.bytes_freed == 0, "a preview frees nothing");
    assert!(report.reclaimed.is_empty(), "a preview deletes nothing");
    report
}

fn preview_candidate(candidate: &Candidate, roots: &Roots, policy: &Policy, report: &mut GcReport) {
    let workspace = &candidate.workspace;
    assert!(workspace.is_absolute(), "run workspaces are absolute paths");
    let path = match workspace.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) => {
            report
                .failed
                .push((workspace.clone(), format!("resolving it: {e}")));
            return;
        }
    };
    if !matches!(candidate.kind, Kind::Worktree) {
        if candidate.action == Action::Remove {
            report.removed.push(path);
        }
        return;
    }
    match worktree_outcome(candidate, &path, roots, policy) {
        Ok(Outcome::Remove) => report.removed.push(path),
        Ok(Outcome::Delete(paths)) if paths.is_empty() => {}
        Ok(Outcome::Delete(paths)) => report.would_delete.push(WouldDelete { path, paths }),
        Ok(Outcome::Skip { why, caches }) => {
            if !caches.is_empty() {
                report.would_delete.push(WouldDelete {
                    path: path.clone(),
                    paths: caches,
                });
            }
            report.skipped.push((path, why));
        }
        Err(why) => report.failed.push((path, why)),
    }
}

enum Outcome {
    Remove,
    /// Only these ignored paths would go.
    Delete(Vec<String>),
    /// It stays for `why`, but `caches` would go.
    Skip {
        why: String,
        caches: Vec<String>,
    },
}

fn worktree_outcome(
    candidate: &Candidate,
    path: &Path,
    roots: &Roots,
    policy: &Policy,
) -> Result<Outcome, String> {
    let remove = candidate.action == Action::Remove;
    if is_inside(path, &roots.scratch) {
        return Ok(if remove {
            Outcome::Remove
        } else {
            Outcome::Delete(Vec::new())
        });
    }
    let skip = |why: String| {
        Ok(Outcome::Skip {
            why,
            caches: Vec::new(),
        })
    };
    if !is_inside(path, &roots.worktrees) {
        return skip(format!(
            "outside the worktree root {} and the scratch directory",
            roots.worktrees.display()
        ));
    }
    let Some(repo) = &candidate.repo else {
        return skip("its repository is no longer registered and its `.git` names none".into());
    };
    if !is_listed_worktree(repo, path).map_err(|e| e.to_string())? {
        return skip(format!("not a worktree of {}", repo.display()));
    }
    let caches = || cache::preview_worktree(path, &policy.cache_dirs).map_err(|e| e.to_string());
    if !remove {
        return Ok(Outcome::Delete(caches()?));
    }
    if policy.force {
        return Ok(Outcome::Remove);
    }
    match unremovable(&roots.data_dir, repo, path, &policy.cache_dirs) {
        Ok(None) => Ok(Outcome::Remove),
        Ok(Some(why)) => {
            let caches = if policy.reclaim_before == archon_core::Timestamp::MIN_UTC {
                Vec::new()
            } else {
                caches()?
            };
            let why = format!("{why}; `archon gc --force` removes it anyway");
            Ok(Outcome::Skip { why, caches })
        }
        Err(e) => Err(e.to_string()),
    }
}
