//! Whether reviewers' verdicts hold for the change as the run's branch holds
//! it: a counted pass read the head, or later passes read every commit since.
//! A verdict on a commit the branch no longer holds (amended or rebased away)
//! is superseded: it never counts, and a superseded fail stands until the
//! counted passes read the whole change. A named delta read no named one
//! anchors may rest on the lead's earlier reviewers, which Archon adds itself.

use std::collections::BTreeMap;
use std::path::Path;

use archon_core::error::ArchonError;
use archon_pipeline::review::Verdict;

use crate::worktree;
use crate::worktree::stack::is_commit_id;

/// Commits a refusal names, shortened past this many digits.
const SHOWN_DIGITS: usize = 12;

/// One reviewer's verdict, its commits resolved in the run's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Placed {
    pub agent: String,
    /// The lead named it; an earlier reviewer it did not name only anchors.
    pub named: bool,
    pub pass: bool,
    pub head: String,
    /// Where the diff it read starts; `None` when it read the whole change.
    pub base: Option<String>,
    /// The branch no longer holds `head`: an amend or rebase replaced it.
    pub superseded: bool,
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

/// Where the change under review starts.
#[derive(Debug, Clone, Copy)]
pub(super) struct Base<'a> {
    /// The branch the run forked from; empty when its variables name none.
    pub branch: &'a str,
    /// The frozen pull request head an `onto` item's branch was forked at: the
    /// change is what the item added on top of it, not the pull request.
    pub onto_head: Option<&'a str>,
}

/// The branch head and each verdict on its commits, `named` first, or why a named one is
/// not on the branch (an `earlier` one is left out). `base` gives the fork point.
pub(super) fn place(
    history: &mut History,
    base: Base,
    named: &[(String, Verdict)],
    earlier: &[(String, Verdict)],
) -> Result<Result<(String, Vec<Placed>), String>, ArchonError> {
    let dir = history.dir;
    let Some(head) = worktree::commit_id(dir, "HEAD")? else {
        return Ok(Err("the run's workspace has no commit to review".into()));
    };
    let forks = fork_points(dir, base, &head)?;
    let mut placed = Vec::with_capacity(named.len() + earlier.len());
    for (agent, verdict) in named {
        match place_one(history, &head, &forks, agent, verdict)? {
            Ok(read) => placed.push(read),
            Err(why) => return Ok(Err(format!("reviewer {agent} {why}"))),
        }
    }
    assert_eq!(placed.len(), named.len(), "every named verdict is placed");
    for (agent, verdict) in earlier {
        match place_one(history, &head, &forks, agent, verdict)? {
            Ok(read) => placed.push(Placed {
                named: false,
                ..read
            }),
            Err(why) => tracing::debug!(%agent, %why, "an earlier reviewer anchors nothing"),
        }
    }
    assert!(
        placed.iter().all(|read| is_commit_id(&read.head)),
        "every placed head is a full commit id"
    );
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

/// Where the run's branch last took in `base.branch`, local or as fetched from
/// `origin`: after a refresh merged the remote base, the newer of the two. Empty
/// when neither is there, so only a review stating no base reads the whole change.
/// An `onto` item's change starts at its frozen head, which the branch holds; a
/// newer base a refresh merged is a start too, as both hold the item's change.
fn fork_points(dir: &Path, base: Base, head: &str) -> Result<Vec<String>, ArchonError> {
    let forks: Vec<String> = branch_fork(dir, base.branch, head)?.into_iter().collect();
    let Some(onto) = base.onto_head else {
        return Ok(forks);
    };
    let Some(onto) = worktree::commit_id(dir, onto)? else {
        return Ok(forks);
    };
    if !worktree::is_ancestor(dir, &onto, head)? {
        return Ok(forks);
    }
    // The pull request's fork from its base is behind the frozen head: not a start.
    let mut starts = Vec::with_capacity(forks.len() + 1);
    for fork in forks {
        if !worktree::is_ancestor(dir, &fork, &onto)? {
            starts.push(fork);
        }
    }
    starts.push(onto);
    Ok(starts)
}

/// The newest commit of `base_branch` (local or `origin`'s) that `head` holds.
fn branch_fork(dir: &Path, base_branch: &str, head: &str) -> Result<Option<String>, ArchonError> {
    if base_branch.is_empty() {
        return Ok(None);
    }
    let remote = format!("refs/remotes/origin/{base_branch}");
    let mut bases = Vec::with_capacity(2);
    for name in [base_branch, remote.as_str()] {
        if let Some(base) = worktree::commit_id(dir, name)? {
            bases.push(base);
        }
    }
    if bases.is_empty() {
        return Ok(None);
    }
    worktree::merge_base(dir, head, &bases)
}

/// `agent`'s verdict with its head and base as full commit ids, the base
/// dropped when it is at or before the fork: such a reviewer read the whole change.
fn place_one(
    history: &mut History,
    head: &str,
    forks: &[String],
    agent: &str,
    verdict: &Verdict,
) -> Result<Result<Placed, String>, ArchonError> {
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
    let placed = |head: String, base: Option<String>, superseded: bool| Placed {
        agent: agent.to_owned(),
        named: true,
        pass: verdict.pass,
        head,
        base,
        superseded,
    };
    if !history.holds(&read, head)? {
        return Ok(Ok(placed(read, None, true)));
    }
    if forks.contains(&read) {
        return Ok(Err("read the base branch, not the change".into()));
    }
    let Some(base) = &verdict.base else {
        return Ok(Ok(placed(read, None, false)));
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
    let mut whole = false;
    for fork in forks {
        whole |= history.holds(&base, fork)?;
    }
    Ok(Ok(placed(read, (!whole).then_some(base), false)))
}

/// What makes a review: the named passes that hold, by index in the placed
/// reads, and the earlier reviewers Archon added to anchor named delta reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Held {
    pub passes: Vec<usize>,
    pub implicit: Vec<String>,
}

/// The `placed` passes that hold for the branch at `head`, or why the
/// verdicts do not make `owed` named ones. A fail never counts.
pub(super) fn counted(
    history: &mut impl Ancestry,
    placed: &[Placed],
    head: &str,
    owed: u32,
) -> Result<Result<Held, String>, ArchonError> {
    assert!(owed >= 1, "only an owed review is checked");
    assert!(
        placed
            .iter()
            .all(|read| !read.superseded || read.base.is_none()),
        "a superseded read keeps no base"
    );
    let in_play = match in_play(history, placed)? {
        Ok(in_play) => in_play,
        Err(why) => return Ok(Err(why)),
    };
    let reads: Vec<Placed> = in_play.iter().map(|i| placed[*i].clone()).collect();
    let passes = match judged(history, &reads, head, owed)? {
        Ok(passes) => passes,
        Err(why) => return Ok(Err(why)),
    };
    let passes: Vec<usize> = passes.into_iter().map(|i| in_play[i]).collect();
    assert!(
        passes.iter().all(|i| placed[*i].named),
        "only a named pass counts toward the review"
    );
    let implicit = reads.into_iter().filter(|read| !read.named);
    Ok(Ok(Held {
        passes,
        implicit: implicit.map(|read| read.agent).collect(),
    }))
}

/// Why a named read in `placed` is refused when no named read anchors it and
/// the lead's earlier reviewers are not read; `None` when every one is anchored.
pub(super) fn unanchored_named(
    history: &mut impl Ancestry,
    placed: &[Placed],
) -> Result<Option<String>, ArchonError> {
    let named = named_count(placed);
    let alone = anchored(history, &placed[..named])?;
    let stray = alone.iter().position(|held| !held);
    Ok(stray.map(|stray| unanchored(&placed[stray], false)))
}

/// Indexes of the reads coverage rests on: the named ones, and when one of
/// those is unanchored, every earlier read anchored with them. `Err` says which is not.
fn in_play(
    history: &mut impl Ancestry,
    placed: &[Placed],
) -> Result<Result<Vec<usize>, String>, ArchonError> {
    let named = named_count(placed);
    let alone = anchored(history, &placed[..named])?;
    if !alone.contains(&false) {
        return Ok(Ok((0..named).collect()));
    }
    let with_earlier = anchored(history, placed)?;
    if let Some(stray) = with_earlier[..named].iter().position(|held| !held) {
        return Ok(Err(unanchored(&placed[stray], true)));
    }
    // An earlier read nothing anchors read only part of the change: it is left out.
    let kept: Vec<usize> = (0..placed.len()).filter(|i| with_earlier[*i]).collect();
    assert!(kept.len() > named, "an earlier read anchors a named one");
    Ok(Ok(kept))
}

/// How many reads lead `placed`, all named: the earlier ones follow them.
fn named_count(placed: &[Placed]) -> usize {
    let named = placed.iter().take_while(|read| read.named).count();
    assert!(
        placed[named..].iter().all(|read| !read.named),
        "named reads come first"
    );
    named
}

/// For each of `reads`, whether it read the whole change or from a commit
/// another of them read the change up to.
fn anchored(history: &mut impl Ancestry, reads: &[Placed]) -> Result<Vec<bool>, ArchonError> {
    // What a superseded read covered is not on the branch, so it anchors nothing.
    spread(
        reads,
        |_, read| read.base.is_none(),
        |read, by| {
            let base = read.base.as_deref().expect("unanchored reads have a base");
            Ok(!by.superseded && history.holds(base, &by.head)?)
        },
    )
}

/// Why the named `read` is unanchored, by its own or, once `earlier_read`,
/// the lead's earlier reviewers.
fn unanchored(read: &Placed, earlier_read: bool) -> String {
    assert!(read.named, "only a named read is refused for its anchor");
    let base = short(read.base.as_deref().expect("a stray read has a base"));
    let by = match earlier_read {
        true => "no reviewer named, nor any earlier one of this attempt,",
        false => "no reviewer named",
    };
    format!(
        "reviewer {} read only from {base}, and {by} read the change up to it",
        read.agent
    )
}

/// Indexes of the anchored `reads` whose named passes hold for the branch
/// at `head`, or why they are not `owed` passes with every fail answered.
fn judged(
    history: &mut impl Ancestry,
    reads: &[Placed],
    head: &str,
    owed: u32,
) -> Result<Result<Vec<usize>, String>, ArchonError> {
    let current = spread(
        reads,
        |_, read| read.pass && read.head == head,
        |read, by| Ok(read.pass && !read.superseded && covers(history, by, &read.head)?),
    )?;
    let passes: Vec<usize> = (0..reads.len())
        .filter(|i| current[*i] && reads[*i].named)
        .collect();
    assert!(
        passes
            .iter()
            .all(|i| reads[*i].pass && !reads[*i].superseded),
        "only a pass of a commit the branch holds counts"
    );
    if passes.len() < owed as usize {
        return Ok(Err(short_of(reads, &current, passes.len(), owed)));
    }
    if let Some(why) = open_fail(history, reads, &current)? {
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
/// the commits after it, nor a later fail that read past it and was answered.
fn open_fail(
    history: &mut impl Ancestry,
    placed: &[Placed],
    current: &[bool],
) -> Result<Option<String>, ArchonError> {
    assert_eq!(placed.len(), current.len(), "one mark per read");
    let mut by_a_pass = Vec::with_capacity(placed.len());
    for read in placed {
        by_a_pass.push(!read.pass && answered_by_a_pass(history, placed, current, read)?);
    }
    // A reviewer that read a fix and failed it again answers the fail it read past once its own is.
    let answered = spread(
        placed,
        |i, _| by_a_pass[i],
        |read, by| {
            let chained = !read.pass && !by.pass && !read.superseded && !by.superseded;
            Ok(chained && reads_past(history, by, read)?)
        },
    )?;
    let Some(fail) = placed
        .iter()
        .zip(&answered)
        .find(|(read, held)| !read.pass && !**held)
        .map(|(read, _)| read)
    else {
        return Ok(None);
    };
    let at = short(&fail.head);
    Ok(Some(match fail.superseded {
        true => format!(
            "reviewer {} failed {at}, which the branch no longer holds, and no passing \
             reviewer named read the whole change since",
            fail.agent
        ),
        false => format!(
            "reviewer {} failed the change at {at}, and no passing reviewer named read \
             what changed after it",
            fail.agent
        ),
    }))
}

/// Whether one of the counted passes answers `fail`.
fn answered_by_a_pass(
    history: &mut impl Ancestry,
    placed: &[Placed],
    current: &[bool],
    fail: &Placed,
) -> Result<bool, ArchonError> {
    for (pass, _) in placed.iter().zip(current).filter(|(_, held)| **held) {
        if answers(history, pass, fail)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the counted `pass` read past `fail`: the commits after its head,
/// or, when that head was amended or rebased away, the change from the fork.
fn answers(history: &mut impl Ancestry, pass: &Placed, fail: &Placed) -> Result<bool, ArchonError> {
    assert!(pass.pass && !fail.pass, "a pass answers a fail");
    assert!(!pass.superseded, "a counted pass is on the branch");
    if fail.superseded {
        return Ok(pass.base.is_none());
    }
    reads_past(history, pass, fail)
}

/// Whether `by` read a later head than `earlier` and every commit after it.
fn reads_past(
    history: &mut impl Ancestry,
    by: &Placed,
    earlier: &Placed,
) -> Result<bool, ArchonError> {
    assert!(
        !earlier.superseded,
        "a superseded head has no commits after it"
    );
    let later = by.head != earlier.head && history.holds(&earlier.head, &by.head)?;
    Ok(later && covers(history, by, &earlier.head)?)
}

/// Why only `counted` named passes count, naming each read that no longer holds.
fn short_of(placed: &[Placed], current: &[bool], counted: usize, owed: u32) -> String {
    assert!(counted < owed as usize, "only a short review is refused");
    assert_eq!(placed.len(), current.len(), "one mark per read");
    let stale: Vec<String> = placed
        .iter()
        .zip(current)
        .filter(|(_, held)| !**held)
        .map(|(read, _)| match (read.superseded, read.pass) {
            (true, _) => format!(
                "{} read {}, which the branch no longer holds",
                read.agent,
                short(&read.head)
            ),
            (false, true) => format!(
                "{} passed {} and no passing reviewer named read the commits after it",
                read.agent,
                short(&read.head)
            ),
            (false, false) => format!("{} failed {}", read.agent, short(&read.head)),
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
            named: true,
            pass,
            head: head.into(),
            base: base.map(Into::into),
            superseded: false,
        }
    }

    /// A read of `head`, a commit an amend or rebase took off the branch.
    fn gone(agent: &str, pass: bool, head: &str) -> Placed {
        Placed {
            superseded: true,
            ..read(agent, pass, head, None)
        }
    }

    /// A read by a reviewer the lead delegated to earlier and did not name.
    fn earlier(agent: &str, pass: bool, head: &str, base: Option<&str>) -> Placed {
        Placed {
            named: false,
            ..read(agent, pass, head, base)
        }
    }

    fn held(placed: &[Placed], head: &str, owed: u32) -> Result<Held, String> {
        counted(&mut Line, placed, head, owed).unwrap()
    }

    fn count(placed: &[Placed], head: &str, owed: u32) -> Result<Vec<usize>, String> {
        held(placed, head, owed).map(|held| held.passes)
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
    /// A fix failed its delta review, and a third reviewer passed the fix of the fix.
    #[test]
    fn a_failed_fix_answered_by_a_later_delta_answers_the_fail_it_read_past() {
        let chain = [
            read("agt_a", false, "c3", None),
            read("agt_b", false, "c4", Some("c3")),
            read("agt_c", true, "c5", Some("c4")),
        ];
        assert_eq!(count(&chain, "c5", 1).unwrap(), [2]);
        let unanswered = [
            read("agt_a", false, "c3", None),
            read("agt_b", false, "c4", Some("c3")),
            read("agt_c", true, "c4", Some("c3")),
        ];
        let refused = count(&unanswered, "c4", 1).unwrap_err();
        assert!(
            refused.contains("agt_b failed the change at c4"),
            "{refused}"
        );
        let skipped = [
            read("agt_a", false, "c3", None),
            read("agt_b", false, "c4", None),
            read("agt_c", false, "c5", Some("c4")),
            read("agt_d", true, "c6", Some("c5")),
        ];
        assert_eq!(count(&skipped, "c6", 1).unwrap(), [3]);
    }

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

    #[test]
    fn an_early_fix_needs_only_the_missing_delta_passes() {
        let fixed = [
            read("agt_fail", false, "c3", None),
            read("agt_pass", true, "c3", None),
            read("agt_delta", true, "c4", Some("c3")),
            read("agt_whole", true, "c4", None),
        ];
        assert_eq!(count(&fixed, "c4", 3).unwrap(), [1, 2, 3]);
        let missing = count(&fixed[..3], "c4", 3).unwrap_err();
        assert!(missing.starts_with("2 of the reviewers named"), "{missing}");

        // Two reviewers cancelled at c3 are replaced by whole-change reads at c4.
        let fail_anchored = [
            read("agt_fail", false, "c3", None),
            read("agt_delta", true, "c4", Some("c3")),
            read("agt_whole_a", true, "c4", None),
            read("agt_whole_b", true, "c4", None),
        ];
        assert_eq!(count(&fail_anchored, "c4", 3).unwrap(), [1, 2, 3]);
        let unanchored = count(&fail_anchored[1..2], "c4", 1).unwrap_err();
        assert!(
            unanchored.contains("nor any earlier one of this attempt, read the change up to it"),
            "{unanchored}"
        );
    }

    /// The lead amended a reviewed commit (`x3`, off the line) into `c4`.
    #[test]
    fn a_pass_of_an_amended_commit_is_superseded_and_never_counts() {
        let amended = [gone("agt_a", true, "x3"), read("agt_b", true, "c4", None)];
        assert_eq!(count(&amended, "c4", 1).unwrap(), [1]);
        let short = count(&amended, "c4", 2).unwrap_err();
        assert!(short.starts_with("1 of the reviewers named"), "{short}");
        assert!(
            short.contains("agt_a read x3, which the branch no longer holds"),
            "{short}"
        );
        // `x3` sorts after `c3` in `Line`, so only the guard keeps it from anchoring.
        let unanchored = [
            gone("agt_a", true, "x3"),
            read("agt_b", true, "c4", Some("c3")),
        ];
        let refused = count(&unanchored, "c4", 1).unwrap_err();
        assert!(refused.contains("agt_b read only from c3"), "{refused}");
    }

    #[test]
    fn a_fail_of_an_amended_commit_stands_until_passes_read_the_whole_change() {
        let delta = [
            gone("agt_a", false, "x3"),
            read("agt_b", false, "c2", None),
            read("agt_c", true, "c4", Some("c2")),
        ];
        let refused = count(&delta, "c4", 1).unwrap_err();
        assert!(
            refused.contains("agt_a failed x3, which the branch no longer holds"),
            "{refused}"
        );
        let chain = [
            gone("agt_a", false, "x3"),
            read("agt_b", true, "c2", None),
            read("agt_c", true, "c4", Some("c2")),
        ];
        assert_eq!(count(&chain, "c4", 1).unwrap(), [1, 2]);
        let whole = [gone("agt_a", false, "x3"), read("agt_b", true, "c4", None)];
        assert_eq!(count(&whole, "c4", 1).unwrap(), [1]);
    }

    /// The holder named only its last delta reviewers; the whole-change pass
    /// and the delta between came from reviewers it delegated earlier.
    #[test]
    fn an_unnamed_earlier_review_anchors_a_named_delta_chain() {
        let reads = [
            read("agt_d1", true, "c5", Some("c4")),
            read("agt_d2", true, "c5", Some("c4")),
            earlier("agt_whole", true, "c3", None),
            earlier("agt_mid", true, "c4", Some("c3")),
        ];
        let found = held(&reads, "c5", 2).unwrap();
        assert_eq!(found.passes, [0, 1], "only named passes count");
        assert_eq!(found.implicit, ["agt_whole", "agt_mid"]);
        let short = count(&reads, "c5", 3).unwrap_err();
        assert!(short.starts_with("2 of the reviewers named"), "{short}");
        let named = held(&reads[..2], "c5", 1).unwrap_err();
        assert!(
            named.contains("agt_d1 read only from c4, and no reviewer named"),
            "{named}"
        );
        let stray = unanchored_named(&mut Line, &reads).unwrap().unwrap();
        assert!(
            stray.ends_with("no reviewer named read the change up to it"),
            "{stray}"
        );
        let gap = held(&[reads[0].clone(), reads[2].clone()], "c5", 1).unwrap_err();
        assert!(gap.contains("agt_d1 read only from c4"), "{gap}");
        let whole = [read("agt_a", true, "c5", None), reads[2].clone()];
        let unneeded = held(&whole, "c5", 1).unwrap();
        assert!(unneeded.implicit.is_empty(), "added only when needed");
        assert_eq!(unanchored_named(&mut Line, &whole).unwrap(), None);
    }

    #[test]
    fn an_earlier_fail_that_anchors_a_delta_stands_until_a_pass_covers_it() {
        let answered = [
            read("agt_delta", true, "c5", Some("c4")),
            earlier("agt_fail", false, "c4", None),
        ];
        assert_eq!(count(&answered, "c5", 1).unwrap(), [0]);
        let open = [
            read("agt_delta", true, "c5", Some("c4")),
            earlier("agt_fail", false, "c4", None),
            earlier("agt_late", false, "c5", None),
        ];
        let refused = count(&open, "c5", 1).unwrap_err();
        assert!(
            refused.contains("agt_late failed the change at c5"),
            "{refused}"
        );
    }

    /// An amend took `x2` and `x3` off the branch: what read them anchors nothing,
    /// and a fail of one stands until a pass reads the whole change since.
    #[test]
    fn an_earlier_read_of_an_amended_commit_anchors_nothing() {
        let unnamed = |gone: Placed| Placed {
            named: false,
            ..gone
        };
        let amended = [
            read("agt_delta", true, "c4", Some("c3")),
            unnamed(gone("agt_old", true, "x3")),
        ];
        let refused = count(&amended, "c4", 1).unwrap_err();
        assert!(refused.contains("agt_delta read only from c3"), "{refused}");
        let failed = [
            read("agt_delta", true, "c4", Some("c3")),
            earlier("agt_whole", false, "c3", None),
            unnamed(gone("agt_old", false, "x2")),
        ];
        let refused = count(&failed, "c4", 1).unwrap_err();
        assert!(
            refused.contains("agt_old failed x2, which the branch no longer holds"),
            "{refused}"
        );
        let rereviewed = [
            failed[0].clone(),
            earlier("agt_rereview", true, "c3", None),
            failed[2].clone(),
        ];
        assert_eq!(count(&rereviewed, "c4", 1).unwrap(), [0]);
    }

    /// Git in `dir` with no config but the repository's own and a throwaway identity.
    fn git(dir: &Path, args: &[&str]) -> String {
        let who = ["-c", "user.name=t", "-c", "user.email=t@example.invalid"];
        let git = worktree::daemon_git::DaemonGit::sealed_in(dir).args(who);
        git.args(args.iter().copied()).text().unwrap()
    }

    /// Commit `file` in `dir`; the new commit.
    fn commit(dir: &Path, file: &str) -> String {
        std::fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", file]);
        git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn a_review_from_the_fetched_base_a_refresh_merged_reads_the_whole_change() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        git(&repo, &["init", "-q", "--initial-branch=main"]);
        let fork = commit(&repo, "base.txt");
        git(&repo, &["checkout", "-q", "-b", "run"]);
        commit(&repo, "change.txt");
        git(&repo, &["checkout", "-q", "--detach", &fork]);
        let upstream = commit(&repo, "upstream.txt");
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", &upstream],
        );
        git(&repo, &["checkout", "-q", "run"]);
        git(&repo, &["merge", "-q", "--no-edit", &upstream]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let verdict = Verdict {
            pass: true,
            head: head.clone(),
            base: Some(upstream.clone()),
            findings: None,
            titles: Vec::new(),
        };
        let named = [("agt_r".to_owned(), verdict)];

        let mut history = History::new(&repo);
        let (at, placed) = place(
            &mut history,
            Base {
                branch: "main",
                onto_head: None,
            },
            &named,
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(at, head);
        assert_eq!(
            placed[0].base, None,
            "the local `main` is stale at {fork}; the fetched base is where the change starts"
        );

        git(&repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let (_, placed) = place(
            &mut history,
            Base {
                branch: "main",
                onto_head: None,
            },
            &named,
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            placed[0].base.as_deref(),
            Some(upstream.as_str()),
            "without the fetched base the same read is only a delta"
        );
    }

    /// A pull request of two commits on `main`, and a run branch of two commits
    /// forked at its head: (pull request's first commit, its head, run's first, run's head).
    fn onto_run(repo: &Path) -> (String, String, String, String) {
        git(repo, &["init", "-q", "--initial-branch=main"]);
        commit(repo, "base.txt");
        git(repo, &["checkout", "-q", "-b", "pr"]);
        let first = commit(repo, "pr1.txt");
        let onto = commit(repo, "pr2.txt");
        git(repo, &["checkout", "-q", "-b", "run"]);
        let change = commit(repo, "change1.txt");
        let head = commit(repo, "change2.txt");
        (first, onto, change, head)
    }

    /// The base the placed read of `head` from `from` keeps, for a run on `main`.
    fn kept_base(repo: &Path, onto: Option<&str>, head: &str, from: &str) -> Option<String> {
        let verdict = Verdict {
            pass: true,
            head: head.to_owned(),
            base: Some(from.to_owned()),
            findings: None,
            titles: Vec::new(),
        };
        let named = [("agt_r".to_owned(), verdict)];
        let base = Base {
            branch: "main",
            onto_head: onto,
        };
        let mut history = History::new(repo);
        let (_, placed) = place(&mut history, base, &named, &[]).unwrap().unwrap();
        placed[0].base.clone()
    }

    #[test]
    fn an_onto_items_change_starts_at_the_frozen_head_not_the_pull_requests_fork() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        let (first, onto, change, head) = onto_run(&repo);
        let at = Some(onto.as_str());

        assert_eq!(kept_base(&repo, at, &head, &onto), None);
        assert_eq!(kept_base(&repo, at, &head, &first), None);
        assert_eq!(
            kept_base(&repo, at, &head, &change).as_deref(),
            Some(change.as_str()),
            "a read from after the frozen head is a delta needing an anchor"
        );
    }

    #[test]
    fn without_onto_a_read_from_the_pull_requests_head_is_only_a_delta() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        let (_, onto, _, head) = onto_run(&repo);
        assert_eq!(
            kept_base(&repo, None, &head, &onto).as_deref(),
            Some(onto.as_str())
        );
    }

    #[test]
    fn an_onto_item_that_took_a_newer_base_reads_whole_from_either_start() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        let (_, onto, _, _) = onto_run(&repo);
        git(&repo, &["checkout", "-q", "main"]);
        let newer = commit(&repo, "newer.txt");
        git(&repo, &["checkout", "-q", "run"]);
        git(&repo, &["merge", "-q", "--no-edit", "main"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "main"]);
        let at = Some(onto.as_str());

        assert_eq!(kept_base(&repo, at, &head, &onto), None);
        assert_eq!(kept_base(&repo, at, &head, &newer), None);
    }
}
