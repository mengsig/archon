//! The blocking findings a lead is told are open, judged as the gate's
//! `coverage::open_blocker` judges them: by commit ancestry, never by the
//! order reviewers came in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use archon_core::error::ArchonError;

use super::coverage::{Ancestry, History};
use super::record::{Finding, Report};
use crate::worktree;

/// A listed finding under the gate's ID: `reviewer:position`, and the
/// earlier ID it says it continues.
struct Listing<'r> {
    id: String,
    continued: Option<&'r str>,
    blocking: bool,
    description: &'r str,
}

/// A read verdict; `head` is `None` when the branch no longer holds it.
struct Read<'r> {
    head: Option<String>,
    pass: bool,
    whole: bool,
    listed: Vec<Listing<'r>>,
    resolved: &'r [String],
}

impl Read<'_> {
    fn lists(&self, id: &str) -> bool {
        self.listed
            .iter()
            .any(|l| l.id == id || l.continued == Some(id))
    }
}

/// What a resolution must read past: an earlier attempt's finding, a fail
/// the branch no longer holds, or a fail at a head on the branch.
enum Origin<'a> {
    Prior,
    Superseded,
    At(&'a str),
}

/// Each open blocking finding's ID and wording, the newest wording kept.
pub(super) fn open_blockers<'r>(
    workspace: &Path,
    reports: &'r [Report],
    prior: &'r [Finding],
) -> Result<Vec<(String, &'r str)>, ArchonError> {
    let mut history = History::new(workspace);
    let reads = place(workspace, &mut history, reports)?;
    let mut open: BTreeMap<String, &'r str> = BTreeMap::new();
    for fail in reads.iter().filter(|read| !read.pass) {
        let origin = fail.head.as_deref().map_or(Origin::Superseded, Origin::At);
        for listing in fail.listed.iter().filter(|l| l.blocking) {
            let id = blocker_id(&mut history, &reads, fail, listing)?;
            if !resolved(&mut history, &reads, &origin, &id)? {
                open.insert(id, listing.description);
            }
        }
    }
    for finding in prior.iter().filter(|f| f.blocking && f.is_open()) {
        if open.contains_key(&finding.id)
            || resolved(&mut history, &reads, &Origin::Prior, &finding.id)?
        {
            continue;
        }
        open.insert(finding.id.clone(), &finding.description);
    }
    assert!(
        open.iter()
            .all(|(id, text)| !id.is_empty() && !text.is_empty())
    );
    Ok(open.into_iter().collect())
}

/// The read verdicts, each head as a full commit ID the branch holds, as
/// `coverage::place` places them; an unknown head is one the branch lost.
fn place<'r>(
    workspace: &Path,
    history: &mut impl Ancestry,
    reports: &'r [Report],
) -> Result<Vec<Read<'r>>, ArchonError> {
    let branch = worktree::commit_id(workspace, "HEAD")?;
    let mut reads = Vec::with_capacity(reports.len());
    for report in reports {
        let (Some(pass), Some(named)) = (report.pass, report.head.as_deref()) else {
            continue;
        };
        let mut head = worktree::commit_id(workspace, named)?;
        if let (Some(at), Some(branch)) = (&head, &branch)
            && !history.holds(at, branch)?
        {
            head = None;
        }
        reads.push(Read {
            head: head.filter(|_| branch.is_some()),
            pass,
            whole: report.base.is_none(),
            listed: listings(report),
            resolved: &report.resolved_findings,
        });
    }
    assert!(reads.len() <= reports.len(), "at most one read per report");
    Ok(reads)
}

/// As `coverage::FindingClaims::new`: each finding keeps its own ID, and
/// only a report's first claim on an earlier ID continues it.
fn listings(report: &Report) -> Vec<Listing<'_>> {
    let mut continued = BTreeSet::new();
    let listed: Vec<Listing> = report
        .findings
        .iter()
        .enumerate()
        .map(|(index, finding)| Listing {
            id: format!("{}:{}", report.agent, index + 1),
            continued: finding.id.as_deref().filter(|id| continued.insert(*id)),
            blocking: finding.blocking,
            description: &finding.description,
        })
        .collect();
    assert_eq!(
        listed.len(),
        report.findings.len(),
        "one listing per finding"
    );
    listed
}

/// As `coverage::blocker_id`: a fail continues an earlier ID only when that
/// ID's origin is at a strict ancestor of its head; otherwise its own.
fn blocker_id(
    history: &mut impl Ancestry,
    reads: &[Read],
    fail: &Read,
    listing: &Listing,
) -> Result<String, ArchonError> {
    assert!(
        !fail.pass && listing.blocking,
        "only a fail's blocker is judged"
    );
    let own = Ok(listing.id.clone());
    let (Some(continued), Some(at)) = (listing.continued, fail.head.as_deref()) else {
        return own;
    };
    let origin = reads
        .iter()
        .find(|read| read.listed.iter().any(|l| l.id == continued));
    let Some(origin) = origin.and_then(|read| read.head.as_deref()) else {
        return own;
    };
    if origin == at || !history.holds(origin, at)? {
        return own;
    }
    Ok(continued.to_owned())
}

/// As `coverage::blocker_resolved`: a read on the branch resolved `id`, read
/// past its origin, and every read on the branch listing it precedes it.
fn resolved(
    history: &mut impl Ancestry,
    reads: &[Read],
    origin: &Origin,
    id: &str,
) -> Result<bool, ArchonError> {
    assert!(!id.is_empty(), "a blocker has an ID");
    for read in reads
        .iter()
        .filter(|read| read.resolved.iter().any(|r| r == id))
    {
        let Some(head) = read.head.as_deref() else {
            continue;
        };
        let past = match origin {
            Origin::Prior => true,
            Origin::Superseded => read.pass && read.whole,
            Origin::At(at) => *at != head && history.holds(at, head)?,
        };
        if past && listings_precede(history, reads, head, id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn listings_precede(
    history: &mut impl Ancestry,
    reads: &[Read],
    head: &str,
    id: &str,
) -> Result<bool, ArchonError> {
    assert!(!head.is_empty(), "a resolving read names its head");
    for read in reads.iter().filter(|read| read.lists(id)) {
        let Some(at) = read.head.as_deref() else {
            continue;
        };
        if at == head || !history.holds(at, head)? {
            return Ok(false);
        }
    }
    Ok(true)
}
