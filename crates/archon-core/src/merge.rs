//! Merging on the operator's request (ARCHITECTURE §18.6). The `merge` knob
//! and the ceiling stay as they are; an item's pull request is merged when
//! the operator asks for it, through a surface archond can check, and only
//! once the preconditions here hold. The facts are GitHub's, read by archond.

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::id::{MergeRequestId, PipelineRunId, QuestionId, QueueItemId, RepoId};

/// Who authorised a merge, as archond verified it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum MergeAuthority {
    /// `archon merge` on the operator's socket.
    Cli,
    /// `/merge` in `archon chat`.
    ArchonChat,
    /// A whole-message `/merge` from the `user` actor in the orchestrator's
    /// Aether chat, in dedicated mode.
    AetherChat,
    /// A watcher-authenticated operator command, with its original thread.
    Github {
        repository: String,
        number: u64,
        login: String,
    },
    /// The orchestrator's `request_merge`, which the operator confirmed by
    /// answering the daemon's question `Q<number>`.
    Confirmed { question: QuestionId, number: u64 },
    /// The orchestrator's `request_merge` under the repository's
    /// `grant_merge` setting, which only the operator sets.
    Grant,
}

impl MergeAuthority {
    /// Whether the orchestrator asked, so it hears how the merge ended.
    pub fn orchestrator_asked(&self) -> bool {
        matches!(
            self,
            Self::AetherChat | Self::Github { .. } | Self::Confirmed { .. } | Self::Grant
        )
    }

    /// Who authorised it, in words for the feed and the audit.
    pub fn describe(&self) -> String {
        match self {
            Self::Cli => "the operator's `archon merge`".into(),
            Self::ArchonChat => "the operator's `/merge` in archon chat".into(),
            Self::AetherChat => "the operator's `/merge` in the orchestrator's chat".into(),
            Self::Github {
                repository,
                number,
                login,
            } => format!("GitHub operator {login} on {repository}#{number}"),
            Self::Confirmed { number, .. } => {
                format!(
                    "the orchestrator's request, confirmed by the operator's answer to Q{number}"
                )
            }
            Self::Grant => {
                "the orchestrator's request under the repository's grant_merge setting".into()
            }
        }
    }
}

/// Where a merge request stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeState {
    /// The operator has not answered the confirmation question yet.
    AwaitingOperator,
    /// A `grant_merge` request whose checks are still pending: archond reads
    /// them again and merges once all are green, or asks the operator.
    AwaitingChecks,
    /// GitHub has not yet decided whether the pull request merges cleanly:
    /// archond reads it again with a growing pause and merges once it can.
    /// The operator's confirmation, if any, stays valid through the wait.
    AwaitingMergeability,
    /// archond is checking and merging; the GitHub write may have happened.
    Merging,
    /// GitHub accepted the request but has not merged it yet.
    Queued,
    Merged {
        /// Merge method, or a description when confirmed later.
        method: String,
        /// GitHub's merging actor, never inferred from the requester.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
        /// Discovered by reconciliation rather than confirmed during execution.
        #[serde(default)]
        observed: bool,
    },
    /// The operator answered no.
    Declined,
    /// A precondition failed or GitHub refused; nothing was merged.
    Refused {
        // Flattened beside the request's own `reason`, so named apart.
        #[serde(rename = "refusal")]
        reason: String,
    },
}

/// How a refusal ends when the base already holds the work: news for the
/// operator, not a blocker.
pub const NOTHING_TO_MERGE: &str = "nothing to merge";

impl MergeState {
    /// Whether this is a refusal only because the work already landed.
    pub fn is_nothing_to_merge(&self) -> bool {
        matches!(self, Self::Refused { reason } if reason.ends_with(NOTHING_TO_MERGE))
    }

    /// The stored name of the state.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AwaitingOperator => "awaiting_operator",
            Self::AwaitingChecks => "awaiting_checks",
            Self::AwaitingMergeability => "awaiting_mergeability",
            Self::Merging => "merging",
            Self::Queued => "queued",
            Self::Merged { .. } => "merged",
            Self::Declined => "declined",
            Self::Refused { .. } => "refused",
        }
    }

    /// The name the store's `state` column holds. A request awaiting checks
    /// is kept as `queued` there, so the column's constraint and the open-
    /// request index are unchanged; the request's data holds the real state.
    pub fn column(&self) -> &'static str {
        match self {
            Self::AwaitingChecks | Self::AwaitingMergeability => "queued",
            other => other.kind(),
        }
    }

    /// Whether the request is still in flight: at most one per item is.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::AwaitingOperator
                | Self::AwaitingChecks
                | Self::AwaitingMergeability
                | Self::Merging
                | Self::Queued
        )
    }
}

/// One request to merge an item's pull request, and how it ended: the audit
/// record and the one source of truth for a waiting confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub id: MergeRequestId,
    pub item: QueueItemId,
    /// The run that delivered the pull request.
    pub run: PipelineRunId,
    pub repo: RepoId,
    pub pr_url: String,
    /// The commit the run's review passed and its delivery pushed; GitHub
    /// merges only while the pull request's head is this commit.
    pub head: String,
    pub authority: MergeAuthority,
    /// The operator acknowledged failing, pending or missing checks.
    pub ignore_checks: bool,
    /// Why the orchestrator asked, for `request_merge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The base branch whose `merge_waive_checks` entry let this merge go
    /// ahead without waiting for its checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waived_by: Option<String>,
    /// The orchestrator's grant request waited for pending checks first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waited: bool,
    /// When the request began waiting for GitHub's mergeability check; the
    /// wait's limit and pauses count from here, not from the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeability_wait_since: Option<Timestamp>,
    /// The checks as last read, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<String>,
    #[serde(flatten)]
    pub state: MergeState,
    pub requested_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Starts the `pr_url` of a request to fast-forward a `ship = local` item's
/// branch into its base, which has no pull request.
pub const LOCAL_REF_PREFIX: &str = "local:";

impl MergeRequest {
    /// The item branch a local request lands, `None` for a pull request.
    pub fn local_branch(&self) -> Option<&str> {
        self.pr_url.strip_prefix(LOCAL_REF_PREFIX)
    }

    /// What is merged, in words: the pull request or the local branch.
    pub fn subject(&self) -> String {
        match self.local_branch() {
            Some(branch) => format!("branch `{branch}`"),
            None => self.pr_url.clone(),
        }
    }

    /// The confirmation question's number while the operator is asked.
    pub fn waiting_on(&self) -> Option<u64> {
        match (&self.state, &self.authority) {
            (MergeState::AwaitingOperator, MergeAuthority::Confirmed { number, .. }) => {
                Some(*number)
            }
            _ => None,
        }
    }

    /// One line on where the request stands, for `archon queue show`.
    pub fn line(&self) -> String {
        if let Some(branch) = self.local_branch() {
            return self.local_line(branch);
        }
        let head = short(&self.head);
        match &self.state {
            MergeState::AwaitingOperator => format!(
                "{} at {head} awaits the operator's answer to Q{}",
                self.pr_url,
                self.waiting_on().unwrap_or_default()
            ),
            MergeState::AwaitingChecks => {
                format!("{} at {head} waits for its checks to finish", self.pr_url)
            }
            MergeState::AwaitingMergeability => format!(
                "{} at {head} is waiting for GitHub to finish its mergeability check",
                self.pr_url
            ),
            MergeState::Merging => format!("{} at {head} is being merged", self.pr_url),
            MergeState::Queued => format!("{} at {head} is queued at GitHub", self.pr_url),
            MergeState::Merged {
                method,
                actor,
                observed,
            } => format!(
                "{} merged at {head} ({method}) on {}",
                self.pr_url,
                merged_attribution(actor.as_deref(), *observed)
            ),
            MergeState::Declined => format!("{} not merged: the operator said no", self.pr_url),
            MergeState::Refused { reason } => {
                format!("{} not merged: {reason}", self.pr_url)
            }
        }
    }
}

impl MergeRequest {
    fn local_line(&self, branch: &str) -> String {
        let head = short(&self.head);
        let what = format!("branch `{branch}` at {head}");
        match &self.state {
            MergeState::AwaitingOperator => format!(
                "{what} awaits the operator's answer to Q{}",
                self.waiting_on().unwrap_or_default()
            ),
            MergeState::AwaitingChecks
            | MergeState::AwaitingMergeability
            | MergeState::Merging
            | MergeState::Queued => {
                format!("{what} is being fast-forwarded into its base")
            }
            MergeState::Merged { .. } => {
                format!("{what} fast-forwarded into its base; the branch is kept")
            }
            MergeState::Declined => format!("{what} not merged: the operator said no"),
            MergeState::Refused { reason } => format!("{what} not merged: {reason}"),
        }
    }
}

/// What a `ship = local` item left on its branch, for review before
/// `archon merge` lands it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalReview {
    pub item: QueueItemId,
    pub repo_path: std::path::PathBuf,
    pub branch: String,
    pub base: String,
    /// The reviewed commit the branch ends at.
    pub head: String,
    /// `git diff --stat` against the base, or a one-line summary of it.
    pub diffstat: String,
    /// Why `archon merge` would refuse now; `None` when it would go ahead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

impl LocalReview {
    /// Branch, repository, base, changes and the next command, in words.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{}: branch `{}` at {} in {}, to be merged into `{}`.\n{}",
            self.item,
            self.branch,
            short(&self.head),
            self.repo_path.display(),
            self.base,
            self.diffstat
        );
        match &self.blocker {
            Some(why) => text.push_str(&format!("\nNot mergeable now: {why}")),
            None => text.push_str(&format!(
                "\nLand it with `archon merge {}` (fast-forward only; the branch is kept).",
                self.item
            )),
        }
        text
    }
}

/// Attribute execution separately from the authority that requested it.
pub fn merged_attribution(actor: Option<&str>, observed: bool) -> String {
    let actor = actor.unwrap_or("an unknown GitHub actor");
    if observed {
        format!(
            "GitHub reports merged by {actor}; confirmed later (possibly merged outside Archon)"
        )
    } else {
        format!("GitHub confirmation by {actor}")
    }
}

/// The first 7 characters of a commit id.
pub fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// How one check of a pull request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Passed, neutral or skipped.
    Green,
    /// Queued, running or expected.
    Pending,
    /// Anything else: failed, cancelled, timed out, an error.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub outcome: CheckOutcome,
}

/// What archond read of a pull request just before deciding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestFacts {
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub is_draft: bool,
    pub head: String,
    pub base: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    pub checks: Vec<Check>,
    pub merged_by: Option<String>,
    pub auto_merge: bool,
}

/// What a merge must find.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    /// The reviewed commit.
    pub head: &'a str,
    /// The branch the change is for.
    pub base: &'a str,
    pub ignore_checks: bool,
}

/// The checks of a pull request that may be merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChecksSeen {
    pub green: usize,
    pub pending: Vec<String>,
    pub failed: Vec<String>,
}

impl ChecksSeen {
    /// What `checks` add up to: which passed, failed or are pending.
    pub fn of(checks: &[Check]) -> Self {
        let mut seen = Self::default();
        for check in checks {
            match check.outcome {
                CheckOutcome::Green => seen.green += 1,
                CheckOutcome::Pending => seen.pending.push(check.name.clone()),
                CheckOutcome::Failed => seen.failed.push(check.name.clone()),
            }
        }
        assert_eq!(seen.total(), checks.len(), "every check is counted once");
        seen
    }

    pub fn total(&self) -> usize {
        self.green + self.pending.len() + self.failed.len()
    }

    /// Every check passed, and there is at least one.
    pub fn all_green(&self) -> bool {
        self.green > 0 && self.pending.is_empty() && self.failed.is_empty()
    }

    /// The checks in words, naming those that did not pass.
    pub fn describe(&self) -> String {
        if self.total() == 0 {
            return "no checks reported".into();
        }
        let mut parts = vec![format!("{} passed", self.green)];
        if !self.failed.is_empty() {
            let names = self.failed.join(", ");
            parts.push(format!("{} failed ({names})", self.failed.len()));
        }
        if !self.pending.is_empty() {
            let names = self.pending.join(", ");
            parts.push(format!("{} pending ({names})", self.pending.len()));
        }
        parts.join(", ")
    }
}

/// What the pull request's facts say about merging it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// It may be merged now; the checks seen.
    Ready(ChecksSeen),
    /// GitHub has not decided whether it merges cleanly: read it again.
    Undecided(ChecksSeen),
}

/// Whether the pull request may be merged now: the checks seen, or why not.
/// An undecided mergeability is neither a yes nor a refusal.
/// Checks that are not all green refuse unless the operator acknowledged them.
pub fn assess(pr: &PullRequestFacts, expected: Expected<'_>) -> Result<Assessment, String> {
    assert!(!expected.head.is_empty(), "a reviewed commit is expected");
    assert!(!expected.base.is_empty(), "a base branch is expected");
    match pr.state.as_str() {
        "OPEN" => {}
        "MERGED" => {
            return Err(format!(
                "the pull request is already merged; {NOTHING_TO_MERGE}"
            ));
        }
        state => return Err(format!("the pull request is {}", state.to_lowercase())),
    }
    if pr.is_draft {
        return Err("the pull request is a draft".into());
    }
    if pr.head != expected.head {
        return Err(format!(
            "its head moved to {} since the review of {}; retry the item so the new commits \
             are reviewed",
            short(&pr.head),
            short(expected.head)
        ));
    }
    if pr.base != expected.base {
        return Err(format!(
            "it targets `{}`, not `{}`, the branch its change is for",
            pr.base, expected.base
        ));
    }
    let seen = ChecksSeen::of(&pr.checks);
    match pr.mergeable.as_str() {
        "MERGEABLE" => {}
        "CONFLICTING" => return Err("it conflicts with its base; retry the item".into()),
        _ => return Ok(Assessment::Undecided(seen)),
    }
    if !seen.all_green() && !expected.ignore_checks {
        return Err(format!(
            "its checks are not all green ({}); the operator must acknowledge that \
             (`--ignore-checks`, or answer `yes ignore-checks`)",
            seen.describe()
        ));
    }
    Ok(Assessment::Ready(seen))
}

/// [`assess`] for a caller that cannot wait: an undecided mergeability refuses.
pub fn evaluate(pr: &PullRequestFacts, expected: Expected<'_>) -> Result<ChecksSeen, String> {
    match assess(pr, expected)? {
        Assessment::Ready(seen) => Ok(seen),
        Assessment::Undecided(_) => Err("GitHub is still working out whether it merges \
             cleanly (see `gh pr view --json mergeable`); ask again in a minute"
            .into()),
    }
}

/// The operator's answer to a merge confirmation question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeAnswer {
    Yes { ignore_checks: bool },
    No,
}

/// The answers a confirmation question offers.
pub const MERGE_OPTIONS: [&str; 3] = ["yes", "yes ignore-checks", "no"];

impl MergeAnswer {
    /// `yes`, `yes ignore-checks` or `no`, in a few spellings; anything else
    /// is refused so that the question stays open, never read as a yes.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lower = text.trim().to_lowercase();
        let (word, rest) = lower
            .split_once(char::is_whitespace)
            .unwrap_or((&lower, ""));
        let word = word.trim_end_matches(['.', '!', ',']);
        let rest = rest.trim().trim_start_matches("--").replace(' ', "-");
        let refused = || {
            format!(
                "{text:?} is not an answer to a merge question: answer `yes`, `yes \
                 ignore-checks` or `no`"
            )
        };
        match word {
            "yes" | "y" | "merge" | "ok" | "approve" | "lgtm" => match rest.as_str() {
                "" => Ok(Self::Yes {
                    ignore_checks: false,
                }),
                "ignore-checks" | "ignore-the-checks" => Ok(Self::Yes {
                    ignore_checks: true,
                }),
                _ => Err(refused()),
            },
            "no" | "n" | "reject" | "cancel" | "decline" if rest.is_empty() => Ok(Self::No),
            _ => Err(refused()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEWED: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

    fn green(name: &str) -> Check {
        Check {
            name: name.into(),
            outcome: CheckOutcome::Green,
        }
    }

    fn open_pr() -> PullRequestFacts {
        PullRequestFacts {
            state: "OPEN".into(),
            is_draft: false,
            head: REVIEWED.into(),
            base: "main".into(),
            mergeable: "MERGEABLE".into(),
            checks: vec![green("test"), green("lint")],
            merged_by: None,
            auto_merge: false,
        }
    }

    fn expected(ignore_checks: bool) -> Expected<'static> {
        Expected {
            head: REVIEWED,
            base: "main",
            ignore_checks,
        }
    }

    fn refusal(pr: &PullRequestFacts) -> String {
        evaluate(pr, expected(false)).expect_err("refused")
    }

    #[test]
    fn an_open_mergeable_pull_request_at_the_reviewed_head_with_green_checks_merges() {
        let seen = evaluate(&open_pr(), expected(false)).unwrap();
        assert!(seen.all_green());
        assert_eq!(seen.describe(), "2 passed");
    }

    #[test]
    fn closed_merged_draft_conflicting_or_unknown_pull_requests_are_refused() {
        let with = |change: fn(&mut PullRequestFacts)| {
            let mut pr = open_pr();
            change(&mut pr);
            refusal(&pr)
        };
        assert!(with(|pr| pr.state = "CLOSED".into()).contains("is closed"));
        assert!(with(|pr| pr.state = "MERGED".into()).contains("already merged"));
        assert!(with(|pr| pr.is_draft = true).contains("draft"));
        assert!(with(|pr| pr.mergeable = "CONFLICTING".into()).contains("conflicts"));
        assert!(with(|pr| pr.mergeable = "UNKNOWN".into()).contains("ask again"));
        let refused = |reason: String| MergeState::Refused { reason };
        assert!(refused(with(|pr| pr.state = "MERGED".into())).is_nothing_to_merge());
        assert!(!refused(with(|pr| pr.state = "CLOSED".into())).is_nothing_to_merge());
        assert!(!MergeState::Declined.is_nothing_to_merge());
    }

    #[test]
    fn a_moved_head_or_another_base_is_refused_even_when_checks_are_acknowledged() {
        let mut moved = open_pr();
        moved.head = "f".repeat(40);
        let why = evaluate(&moved, expected(true)).unwrap_err();
        assert!(
            why.contains("moved to fffffff") && why.contains("a1b2c3d"),
            "{why}"
        );
        let mut stacked = open_pr();
        stacked.base = "archon/parent".into();
        let why = evaluate(&stacked, expected(true)).unwrap_err();
        assert!(why.contains("targets `archon/parent`"), "{why}");
    }

    #[test]
    fn checks_that_are_not_all_green_need_the_operators_acknowledgement() {
        let mut red = open_pr();
        red.checks.push(Check {
            name: "ci".into(),
            outcome: CheckOutcome::Failed,
        });
        red.checks.push(Check {
            name: "e2e".into(),
            outcome: CheckOutcome::Pending,
        });
        let why = refusal(&red);
        assert!(why.contains("1 failed (ci), 1 pending (e2e)"), "{why}");
        let seen = evaluate(&red, expected(true)).unwrap();
        assert_eq!(seen.failed, vec!["ci".to_owned()]);
        let mut none = open_pr();
        none.checks.clear();
        assert!(refusal(&none).contains("no checks reported"));
        assert_eq!(evaluate(&none, expected(true)).unwrap().total(), 0);
    }

    #[test]
    fn only_a_plain_yes_or_no_answers_a_merge_question() {
        let yes = MergeAnswer::Yes {
            ignore_checks: false,
        };
        let ignoring = MergeAnswer::Yes {
            ignore_checks: true,
        };
        for (text, answer) in [
            ("yes", yes),
            (" Y ", yes),
            ("merge", yes),
            ("ok", yes),
            ("approve", yes),
            ("lgtm", yes),
            ("yes.", yes),
            ("yes ignore-checks", ignoring),
            ("yes --ignore-checks", ignoring),
            ("YES ignore checks", ignoring),
            ("no", MergeAnswer::No),
            ("reject", MergeAnswer::No),
        ] {
            assert_eq!(MergeAnswer::parse(text), Ok(answer), "{text:?}");
        }
        for text in [
            "",
            "maybe",
            "yes but later",
            "ok I will review this later",
            "this looks ok to me",
            "approve after tests",
            "lgtm except the migration",
            "no thanks",
            "sure",
            "ignore-checks",
        ] {
            assert!(MergeAnswer::parse(text).is_err(), "{text:?}");
        }
        for option in MERGE_OPTIONS {
            assert!(MergeAnswer::parse(option).is_ok(), "{option}");
        }
    }

    #[test]
    fn a_waiting_request_names_its_question_and_states_round_trip() {
        let now = crate::now();
        let request = MergeRequest {
            id: MergeRequestId::new(),
            item: QueueItemId::new(),
            run: PipelineRunId::new(),
            repo: RepoId::new(),
            pr_url: "https://github.com/o/r/pull/42".into(),
            head: REVIEWED.into(),
            authority: MergeAuthority::Confirmed {
                question: QuestionId::new(),
                number: 7,
            },
            ignore_checks: false,
            reason: Some("the operator said merge it".into()),
            waived_by: None,
            waited: false,
            mergeability_wait_since: None,
            checks: None,
            state: MergeState::AwaitingOperator,
            requested_at: now,
            updated_at: now,
        };
        assert_eq!(request.waiting_on(), Some(7));
        assert!(request.line().contains("Q7"), "{}", request.line());
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["state"], "awaiting_operator");
        assert_eq!(wire["authority"]["via"], "confirmed");
        let back: MergeRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(back, request);
        let merged = MergeRequest {
            state: MergeState::Merged {
                actor: None,
                observed: false,
                method: "squash".into(),
            },
            ..request
        };
        assert_eq!(merged.waiting_on(), None);
        assert!(merged.line().contains("merged at a1b2c3d (squash)"));
        let refused = MergeRequest {
            state: MergeState::Refused {
                reason: "the head moved".into(),
            },
            ..merged
        };
        let back: MergeRequest =
            serde_json::from_value(serde_json::to_value(&refused).unwrap()).unwrap();
        assert_eq!(back, refused, "a refusal keeps the orchestrator's reason");
    }
}
