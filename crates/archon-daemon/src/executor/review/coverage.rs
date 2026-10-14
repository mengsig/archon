//! Whether reviewers' verdicts hold for the change as the run's branch holds
//! it: a counted pass read the head, or later passes read every commit since.

use std::collections::BTreeMap;
use std::path::Path;

use archon_core::error::ArchonError;
use archon_pipeline::review::Verdict;

use crate::worktree;
use crate::worktree::stack::is_commit_id;

/// Commits a refusal names, shortened past this many digits.
const SHOWN_DIGITS: usize = 12;

/// One named reviewer's verdict, its commits resolved in the run's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Placed {
    pub agent: String,
    pub pass: bool,
    pub head: String,
    /// Where the diff it read starts; `None` when it read the whole change.
    pub base: Option<String>,
}

/// Ancestry in the run's workspace, each pair asked of git once.
pub(super) struct History<'a> {
    dir: &'a Path,
    known: BTreeMap<(String, String), bool>,
}

impl<'a> History<'a> {
    pub(super) fn new(dir: &'a Path) -> Self {
        Self {
            dir,
            known: BTreeMap::new(),
        }
    }

    /// Whether `ancestor` is `of` or one of its ancestors.
    pub(super) fn holds(&mut self, ancestor: &str, of: &str) -> Result<bool, ArchonError> {
        if ancestor == of {
            return Ok(true);
        }
        let key = (ancestor.to_owned(), of.to_owned());
        if let Some(known) = self.known.get(&key) {
            return Ok(*known);
        }
        let held = worktree::is_ancestor(self.dir, ancestor, of)?;
        self.known.insert(key, held);
        Ok(held)
    }
}

/// The branch head and each verdict on its commits, or why a verdict names
/// commits the branch does not hold. `base_branch` gives the fork point.
pub(super) fn place(
    history: &mut History,
    base_branch: &str,
    named: &[(String, Verdict)],
) -> Result<Result<(String, Vec<Placed>), String>, ArchonError> {
    let dir = history.dir;
    let Some(head) = worktree::commit_id(dir, "HEAD")? else {
        return Ok(Err("the run's workspace has no commit to review".into()));
    };
    let fork = fork_point(dir, base_branch, &head)?;
    let mut placed = Vec::with_capacity(named.len());
    for (agent, verdict) in named {
        let read = match place_one(history, &head, fork.as_deref(), verdict)? {
            Ok(read) => read,
            Err(why) => return Ok(Err(format!("reviewer {agent} {why}"))),
        };
        let (head, base) = read;
        let (agent, pass) = (agent.clone(), verdict.pass);
        placed.push(Placed {
            agent,
            pass,
            head,
            base,
        });
    }
    assert_eq!(placed.len(), named.len(), "every verdict is placed");
    Ok(Ok((head, placed)))
}

/// Whether the branch at `head`, which the verdicts are held to, holds the
/// `tree` the ship record names: archond delivers only a branch holding it.
pub(super) fn holds_tree(
    dir: &Path,
    head: &str,
    tree: &str,
) -> Result<Result<(), String>, ArchonError> {
    assert!(
        matches!(tree.len(), 40 | 64),
        "a ship record names a full tree id"
    );
    let at = format!("{head}^{{tree}}");
    let held = worktree::git(dir, &["rev-parse", "--verify", at.as_str()])?;
    assert!(
        matches!(held.len(), 40 | 64),
        "rev-parse --verify prints a full id"
    );
    if held == tree {
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "the ship record names tree {}, but the reviewers are held to the branch head {}, \
         which holds tree {}: record the commit the reviewers passed",
        short(tree),
        short(head),
        short(&held)
    )))
}

/// Where the run's branch left `base_branch`; `None` when that branch is
/// gone, so only a review stating no base counts as reading the whole change.
fn fork_point(dir: &Path, base_branch: &str, head: &str) -> Result<Option<String>, ArchonError> {
    if base_branch.is_empty() {
        return Ok(None);
    }
    let Some(base) = worktree::commit_id(dir, base_branch)? else {
        return Ok(None);
    };
    worktree::merge_base(dir, &base, head)
}

/// One verdict's head and base as full commit ids, the base dropped when it
/// is at or before the fork: such a reviewer read the whole change.
fn place_one(
    history: &mut History,
    head: &str,
    fork: Option<&str>,
    verdict: &Verdict,
) -> Result<Result<(String, Option<String>), String>, ArchonError> {
    // Ids are compared as text below, so both must be full.
    assert!(is_commit_id(head), "the branch head is a full commit id");
    let dir = history.dir;
    let Some(read) = worktree::commit_id(dir, &verdict.head)? else {
        return Ok(Err(format!(
            "read {}, a commit the run's workspace does not hold",
            verdict.head
        )));
    };
    assert!(is_commit_id(&read), "git names the commit read in full");
    if !history.holds(&read, head)? {
        let read = short(&read);
        return Ok(Err(format!(
            "read {read}, which the branch no longer holds"
        )));
    }
    if fork == Some(read.as_str()) {
        return Ok(Err("read the base branch, not the change".into()));
    }
    let Some(base) = &verdict.base else {
        return Ok(Ok((read, None)));
    };
    let Some(base) = worktree::commit_id(dir, base)? else {
        return Ok(Err(format!(
            "read from {base}, a commit the workspace does not hold"
        )));
    };
    if !history.holds(&base, &read)? {
        return Ok(Err(format!(
            "read from {}, which is not behind its head",
            short(&base)
        )));
    }
    let whole = match fork {
        Some(fork) => history.holds(&base, fork)?,
        None => false,
    };
    Ok(Ok((read, (!whole).then_some(base))))
}

/// Indexes of the `placed` passes that hold for the branch at `head`, or
/// why the verdicts do not make `owed` of them. A fail never counts.
pub(super) fn counted(
    history: &mut impl Ancestry,
    placed: &[Placed],
    head: &str,
    owed: u32,
) -> Result<Result<Vec<usize>, String>, ArchonError> {
    assert!(owed >= 1, "only an owed review is checked");
    let anchored = spread(
        placed,
        |_, read| read.base.is_none(),
        |read, by| {
            let base = read.base.as_deref().expect("unanchored reads have a base");
            history.holds(base, &by.head)
        },
    )?;
    if let Some((read, _)) = placed.iter().zip(&anchored).find(|(_, held)| !**held) {
        let base = short(read.base.as_deref().expect("a stray read has a base"));
        return Ok(Err(format!(
            "reviewer {} read only from {base}, and no reviewer named read the change up to it",
            read.agent
        )));
    }
    let current = spread(
        placed,
        |_, read| read.pass && read.head == head,
        |read, by| Ok(read.pass && covers(history, by, &read.head)?),
    )?;
    let passes: Vec<usize> = (0..placed.len()).filter(|i| current[*i]).collect();
    assert!(passes.iter().all(|i| placed[*i].pass), "only a pass counts");
    if passes.len() < owed as usize {
        return Ok(Err(short_of(placed, &current, owed)));
    }
    if let Some(why) = open_fail(history, placed, &current)? {
        return Ok(Err(why));
    }
    Ok(Ok(passes))
}

/// What `holds` answers: ancestry, asked of git or of a test's history.
pub(super) trait Ancestry {
    fn holds(&mut self, ancestor: &str, of: &str) -> Result<bool, ArchonError>;
}

impl Ancestry for History<'_> {
    fn holds(&mut self, ancestor: &str, of: &str) -> Result<bool, ArchonError> {
        History::holds(self, ancestor, of)
    }
}

/// Whether `by` read every commit after `commit`: the whole change, or a
/// diff starting at or before it.
fn covers(history: &mut impl Ancestry, by: &Placed, commit: &str) -> Result<bool, ArchonError> {
    match &by.base {
        None => Ok(true),
        Some(base) => history.holds(base, commit),
    }
}

/// The reads marked by `seed`, then every read `joins` links to a marked
/// one, until none is added. At most `placed.len()` rounds.
fn spread(
    placed: &[Placed],
    seed: impl Fn(usize, &Placed) -> bool,
    mut joins: impl FnMut(&Placed, &Placed) -> Result<bool, ArchonError>,
) -> Result<Vec<bool>, ArchonError> {
    let mut marked: Vec<bool> = placed.iter().enumerate().map(|(i, r)| seed(i, r)).collect();
    for _round in 0..placed.len() {
        let mut added = false;
        for i in 0..placed.len() {
            if marked[i] {
                continue;
            }
            for j in (0..placed.len()).filter(|j| marked[*j]) {
                if joins(&placed[i], &placed[j])? {
                    marked[i] = true;
                    added = true;
                    break;
                }
            }
        }
        if !added {
            break;
        }
    }
    assert_eq!(marked.len(), placed.len(), "one mark per read");
    Ok(marked)
}

/// Why a named fail still stands: no counted pass read past its head and
/// the commits after it.
fn open_fail(
    history: &mut impl Ancestry,
    placed: &[Placed],
    current: &[bool],
) -> Result<Option<String>, ArchonError> {
    assert_eq!(placed.len(), current.len(), "one mark per read");
    for fail in placed.iter().filter(|read| !read.pass) {
        let mut answered = false;
        for (pass, _) in placed.iter().zip(current).filter(|(_, held)| **held) {
            let later = pass.head != fail.head && history.holds(&fail.head, &pass.head)?;
            if later && covers(history, pass, &fail.head)? {
                answered = true;
                break;
            }
        }
        if !answered {
            return Ok(Some(format!(
                "reviewer {} failed the change at {}, and no passing reviewer named read what \
                 changed after it",
                fail.agent,
                short(&fail.head)
            )));
        }
    }
    Ok(None)
}

/// Why too few passes count, naming each that no longer holds.
fn short_of(placed: &[Placed], current: &[bool], owed: u32) -> String {
    let counted = current.iter().filter(|held| **held).count();
    assert!(counted < owed as usize, "only a short review is refused");
    let stale: Vec<String> = placed
        .iter()
        .zip(current)
        .filter(|(_, held)| !**held)
        .map(|(read, _)| match read.pass {
            true => format!(
                "{} passed {} and no passing reviewer named read the commits after it",
                read.agent,
                short(&read.head)
            ),
            false => format!("{} failed {}", read.agent, short(&read.head)),
        })
        .collect();
    let mut why = format!(
        "{counted} of the reviewers named passed the branch as it stands, and this item's \
         review level needs {owed}, each a different agent"
    );
    if !stale.is_empty() {
        why.push_str(&format!(" ({})", stale.join("; ")));
    }
    why
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(SHOWN_DIGITS)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight history `c1`..`c9`: each commit follows the one before.
    struct Line;

    impl Ancestry for Line {
        fn holds(&mut self, ancestor: &str, of: &str) -> Result<bool, ArchonError> {
            Ok(ancestor <= of)
        }
    }

    fn read(agent: &str, pass: bool, head: &str, base: Option<&str>) -> Placed {
        Placed {
            agent: agent.into(),
            pass,
            head: head.into(),
            base: base.map(Into::into),
        }
    }

    fn count(placed: &[Placed], head: &str, owed: u32) -> Result<Vec<usize>, String> {
        counted(&mut Line, placed, head, owed).unwrap()
    }

    #[test]
    fn a_pass_holds_for_the_head_it_read_or_with_every_later_commit_read() {
        let at_head = [read("agt_a", true, "c3", None)];
        assert_eq!(count(&at_head, "c3", 1).unwrap(), [0]);
        let stale = count(&at_head, "c4", 1).unwrap_err();
        assert!(
            stale.contains("agt_a passed c3 and no passing reviewer"),
            "{stale}"
        );
        assert!(stale.starts_with("0 of the reviewers named"), "{stale}");

        let fixed = [
            read("agt_a", true, "c3", None),
            read("agt_b", true, "c4", Some("c3")),
        ];
        assert_eq!(count(&fixed, "c4", 2).unwrap(), [0, 1]);
        let gap = [
            read("agt_a", true, "c3", None),
            read("agt_b", true, "c5", Some("c4")),
        ];
        let refused = count(&gap, "c5", 1).unwrap_err();
        assert!(refused.contains("agt_b read only from c4"), "{refused}");
    }

    /// The re-review flow: a reviewer fails, the lead fixes, another reads the fix.
    #[test]
    fn a_fail_never_counts_and_stands_until_a_pass_reads_past_it() {
        let failed = [read("agt_a", false, "c3", None)];
        let refused = count(&failed, "c3", 1).unwrap_err();
        assert!(refused.starts_with("0 of the reviewers named"), "{refused}");
        assert!(refused.contains("agt_a failed c3"), "{refused}");

        let refixed = [
            read("agt_a", false, "c3", None),
            read("agt_b", true, "c4", Some("c3")),
        ];
        assert_eq!(count(&refixed, "c4", 1).unwrap(), [1]);
        let short = count(&refixed, "c4", 2).unwrap_err();
        assert!(short.starts_with("1 of the reviewers named"), "{short}");

        let overruled = [
            read("agt_a", false, "c4", None),
            read("agt_b", true, "c4", None),
        ];
        let refused = count(&overruled, "c4", 1).unwrap_err();
        assert!(
            refused.contains("agt_a failed the change at c4"),
            "{refused}"
        );
    }

    /// A strict review: three read the change, one failed, a fourth read the fix.
    #[test]
    fn several_full_reviews_hold_through_a_reviewed_fix() {
        let strict = [
            read("agt_a", false, "c3", None),
            read("agt_b", true, "c3", None),
            read("agt_c", true, "c3", None),
            read("agt_d", true, "c5", Some("c3")),
        ];
        assert_eq!(count(&strict, "c5", 3).unwrap(), [1, 2, 3]);
        let unread = count(&strict, "c6", 3).unwrap_err();
        assert!(unread.contains("agt_d passed c5"), "{unread}");
    }
}
