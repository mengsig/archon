//! Merging an item's pull request because the operator asked (ARCHITECTURE
//! §18.6). The `merge` knob and the ceiling are not involved: archond knows
//! who asked, records the request before any GitHub write, reads the pull
//! request again and merges only the reviewed commit on the branch the change
//! is for, never with `--admin` or `--auto`. Every outcome goes to the feed.
//!
//! `gh` runs without a lock; a request left `merging` by a stop or a lost
//! confirmation is settled from GitHub by the coordination pass.

use std::path::Path;

use archon_core::autonomy::ReviewLevel;
use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::id::{MergeRequestId, PipelineRunId, QuestionId, QueueItemId};
use archon_core::merge::{
    self, Assessment, ChecksSeen, Expected, LOCAL_REF_PREFIX, MERGE_OPTIONS, MergeAnswer,
    MergeAuthority, MergeRequest, MergeState, PullRequestFacts,
};
use archon_core::notify::{AnswerVia, NotificationKind, Question};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::{Repo, ShipMode};
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;
use archon_queue::store::stacks::StackParent;

use super::local_merge::{self, Landing};
use super::ship::{LOCAL_COMMIT_KEY, MergeRefusal, merge_method, pin_auto_merge};
use super::{clip, coordination, finish, gh, recover};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};
use crate::worktree;
use crate::worktree::stack::is_commit_id;

/// The orchestrator's reason is kept to this.
const MAX_REASON_CHARS: usize = 500;
/// A merge archond is still working on is never this old; one older was
/// interrupted, or its confirmation was lost.
const STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(10);
/// Starts the reason of a request whose pull request could not be read.
const GITHUB_UNREADABLE: &str = "GitHub could not be read";
/// A granted request waits this long for pending checks before the operator
/// is asked.
const WAIT_FOR_CHECKS: chrono::TimeDelta = chrono::TimeDelta::minutes(60);
/// A request waits this long for GitHub to decide whether the pull request
/// merges cleanly before it is refused.
const WAIT_FOR_MERGEABILITY: chrono::TimeDelta = chrono::TimeDelta::minutes(60);
/// GitHub is asked about mergeability again after at least this pause, which
/// grows with the time waited up to [`MERGEABILITY_PAUSE_MAX`].
const MERGEABILITY_PAUSE_MIN: chrono::TimeDelta = chrono::TimeDelta::seconds(5);
const MERGEABILITY_PAUSE_MAX: chrono::TimeDelta = chrono::TimeDelta::seconds(60);

/// What `grant_merge` does with a reviewed request, from its checks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GrantRoute {
    /// Merge now; the base branch's waiver covers the checks.
    Waived,
    /// Merge now: every check passed.
    Green,
    /// Some checks are pending and none failed: wait for them.
    Wait,
    /// Failed or missing checks: the operator decides.
    Ask,
}

fn grant_route(waived_bases: &[String], base: &str, seen: &ChecksSeen) -> GrantRoute {
    if waived_bases.iter().any(|b| b == base) {
        GrantRoute::Waived
    } else if seen.all_green() {
        GrantRoute::Green
    } else if seen.failed.is_empty() && !seen.pending.is_empty() {
        GrantRoute::Wait
    } else {
        GrantRoute::Ask
    }
}

/// A succeeded item's delivered pull request, as archond recorded it.
struct Shipped {
    item: QueueItem,
    run: PipelineRunId,
    repo: Repo,
    pr_url: String,
    /// The reviewed commit its delivery pushed; for a local branch, the one
    /// its newest refresh or restack fix recorded, if any came later.
    head: String,
    /// Whether every run that delivered `head` had its change reviewed.
    reviewed: bool,
    /// The pull request is not the item's own: a review item passed it at `head`.
    review_of: bool,
}

impl Shipped {
    /// The item branch of a `ship = local` delivery.
    fn local_branch(&self) -> Option<&str> {
        self.pr_url.strip_prefix(LOCAL_REF_PREFIX)
    }

    /// What is merged, in words.
    fn subject(&self) -> String {
        match self.local_branch() {
            Some(branch) => format!("branch `{branch}`"),
            None => self.pr_url.clone(),
        }
    }

    /// Base branches whose checks the repository waives; none for another's
    /// pull request, which merges only with every check green.
    fn waived_bases(&self) -> &[String] {
        match self.review_of {
            true => &[],
            false => &self.repo.merge_waive_checks,
        }
    }

    fn landing<'a>(&'a self, base: &'a str) -> Option<Landing<'a>> {
        Some(Landing {
            item: &self.item.id,
            repo: &self.repo.path,
            branch: self.local_branch()?,
            base,
            head: &self.head,
        })
    }
}

/// The item's delivered pull request; `Err` says why it has none to merge.
fn shipped(store: &Store, id: &QueueItemId) -> Result<Shipped, ArchonError> {
    let item = store.item(id)?;
    let QueueState::Succeeded { run, .. } = &item.state else {
        return Err(ArchonError::InvalidTransition(format!(
            "{id} is {}, not succeeded: only a succeeded item's pull request is merged",
            item.state.kind()
        )));
    };
    let run = run.clone();
    let Some(repo) = &item.repo else {
        return Err(ArchonError::Invalid(format!("{id} has no repository")));
    };
    let repo = store.repo(&crate::resolve::repo_ref(repo))?;
    let stages = store.stage_runs(&run)?;
    if item.onto.is_some() {
        return reviewed_pull_request(item, run, repo, &stages);
    }
    let Some((pr_url, head)) = delivered(&stages).or_else(|| {
        (repo.ship == ShipMode::Local)
            .then(|| local_delivery(store, &run, &stages))
            .flatten()
    }) else {
        let why = match repo.ship {
            ShipMode::Local => "left no commit on its branch to merge",
            ShipMode::Pr => "delivered no pull request: its pipeline opened none",
        };
        return Err(ArchonError::Invalid(format!("run {run} of {id} {why}")));
    };
    assert!(is_commit_id(&head), "checked by delivered");
    let mut reviewed = item.autonomy.review != ReviewLevel::None;
    let fixed = match pr_url.starts_with(LOCAL_REF_PREFIX) {
        true => fixed_head(store, &item.id, &run, &pr_url)?,
        false => None,
    };
    let head = match fixed {
        Some(fixed) => {
            reviewed &= fixed.reviewed;
            fixed.head
        }
        None => head,
    };
    Ok(Shipped {
        item,
        run,
        repo,
        pr_url,
        head,
        reviewed,
        review_of: false,
    })
}

/// The pull request a succeeded review item passed, at the head its
/// reviewers passed; `Err` refuses an item that delivered into its owner's
/// pull request, or whose review did not pass the frozen head.
fn reviewed_pull_request(
    item: QueueItem,
    run: PipelineRunId,
    repo: Repo,
    stages: &[StageRun],
) -> Result<Shipped, ArchonError> {
    let onto = item.onto.as_deref().expect("an onto item");
    let url = onto.pr.url();
    if !onto.review_only {
        return Err(ArchonError::Invalid(format!(
            "{} was delivered onto {url}, which belongs to its owner and holds commits Archon \
             never reviewed: Archon delivers into it but never merges it; merge it by hand, or \
             queue a `review` item onto it and request the merge of that item once it passes",
            item.id
        )));
    }
    if delivered(stages).is_some() {
        return Err(ArchonError::Invalid(format!(
            "{} was queued to review {url} but recorded a delivery: it is not merged",
            item.id
        )));
    }
    let Some(head) = super::onto::reviewed_head(stages) else {
        return Err(ArchonError::Invalid(format!(
            "{} read {url} but holds no review that passed it: only a review item whose \
             reviewers passed the pull request lets it merge; queue one with review level \
             `standard` or `strict`",
            item.id
        )));
    };
    if head != onto.head_sha {
        return Err(ArchonError::Invalid(format!(
            "{}'s reviewers passed {}, not {url}'s frozen head {}: queue a new review item",
            item.id,
            merge::short(&head),
            merge::short(&onto.head_sha)
        )));
    }
    assert!(is_commit_id(&head), "a reviewed head is a full commit id");
    Ok(Shipped {
        pr_url: url,
        head,
        reviewed: true,
        review_of: true,
        item,
        run,
        repo,
    })
}

/// Why another's pull request is not merged at the last step: only every
/// check green lets it merge, which no acknowledgement or waiver replaces.
fn checks_not_green(ready: &Ready) -> Option<String> {
    if !ready.review_of || ready.seen.all_green() {
        return None;
    }
    Some(format!(
        "its checks are not all green ({}): a pull request Archon did not open merges only \
         with every check green, whatever is acknowledged; ask again once they pass",
        ready.seen.describe()
    ))
}

/// Why a reviewed pull request whose head moved since its review is not
/// merged: the new commits need a review of their own.
fn moved_since_review(shipped: &Shipped, facts: &PullRequestFacts) -> Option<String> {
    if !shipped.review_of || facts.state != "OPEN" || facts.head == shipped.head {
        return None;
    }
    assert!(
        is_commit_id(&shipped.head),
        "a reviewed head is a full commit id"
    );
    Some(format!(
        "its head moved to {} since {} reviewed it at {}; queue a new `review` item onto it so \
         the new commits are reviewed, then request the merge of that item",
        merge::short(&facts.head),
        shipped.item.id,
        merge::short(&shipped.head)
    ))
}

/// A later delivery on the owner's branch by one of its restack fixes or refreshes.
struct Fixed {
    head: String,
    reviewed: bool,
}

/// The local commit the newest succeeded restack fix, refresh or `onto` item
/// of `owner` recorded on the branch `delivery` names, when it finished after
/// `run`, the owner's own delivering run: the branch has ended there since.
/// It counts as reviewed only when every such later delivery was.
fn fixed_head(
    store: &Store,
    owner: &QueueItemId,
    run: &PipelineRunId,
    delivery: &str,
) -> Result<Option<Fixed>, ArchonError> {
    assert!(delivery.starts_with(LOCAL_REF_PREFIX), "a local delivery");
    let own = store.run(run)?;
    let delivered_at = own.finished_at.unwrap_or(own.started_at);
    let mut newest: Option<(archon_core::Timestamp, String)> = None;
    let mut reviewed = true;
    for id in store.stack_fixes_of(owner)? {
        let fix = store.item(&id)?;
        assert_ne!(&fix.id, owner, "a fix works on another item's branch");
        let Some((finished, head)) = later_delivery(store, &fix, delivery, delivered_at)? else {
            continue;
        };
        reviewed &= fix.autonomy.review != ReviewLevel::None;
        if newest.as_ref().is_none_or(|(at, _)| finished > *at) {
            newest = Some((finished, head));
        }
    }
    Ok(newest.map(|(_, head)| Fixed { head, reviewed }))
}

/// When the succeeded `fix` finished and the local commit it recorded on the
/// branch `delivery` names, if it finished after `delivered_at`.
fn later_delivery(
    store: &Store,
    fix: &QueueItem,
    delivery: &str,
    delivered_at: archon_core::Timestamp,
) -> Result<Option<(archon_core::Timestamp, String)>, ArchonError> {
    let QueueState::Succeeded { run, .. } = &fix.state else {
        return Ok(None);
    };
    let Some(finished) = store.run(run)?.finished_at else {
        return Ok(None);
    };
    let stages = store.stage_runs(run)?;
    let Some((url, head)) = local_delivery(store, run, &stages) else {
        return Ok(None);
    };
    assert!(is_commit_id(&head), "checked by local_delivery");
    Ok((url == delivery && finished > delivered_at).then_some((finished, head)))
}

/// `(pull request, commit)` from the stage that recorded the delivery.
fn delivered(stages: &[StageRun]) -> Option<(String, String)> {
    let url = archon_core::run::pr_url(stages.iter())?;
    let record = stages.iter().rev().find(|s| {
        s.state == StageState::Completed
            && s.structured
                .as_ref()
                .and_then(|d| d.get("pr_url")?.as_str())
                == Some(url)
    })?;
    let commit = record.structured.as_ref()?.get("commit")?.as_str()?;
    is_commit_id(commit).then(|| (url.to_owned(), commit.to_owned()))
}

/// `(local:<branch>, commit)` of a `ship = local` delivery: the commit its
/// delivery recorded, which the branch must still end at.
fn local_delivery(
    store: &Store,
    run: &PipelineRunId,
    stages: &[StageRun],
) -> Option<(String, String)> {
    let commit = stages
        .iter()
        .rev()
        .filter(|s| s.state == StageState::Completed)
        .find_map(|s| s.structured.as_ref()?.get(LOCAL_COMMIT_KEY)?.as_str())?;
    let branch = store.run(run).ok()?.branch;
    (is_commit_id(commit) && !branch.is_empty())
        .then(|| (format!("{LOCAL_REF_PREFIX}{branch}"), commit.to_owned()))
}

/// The branch the pull request must target; `Err` refuses the merge.
fn expected_base(
    store: &mut Store,
    shipped: &Shipped,
) -> Result<Result<String, String>, ArchonError> {
    if shipped.review_of {
        let onto = shipped.item.onto.as_deref().expect("a review item");
        // The pull request must still target the base it was reviewed for.
        return Ok(Ok(onto.base_ref.clone()));
    }
    settle_merged_parents(store, shipped)?;
    let parents = store.stack_parents(&shipped.item.id)?;
    if let Some(open) = parents.iter().find(|p| p.is_open()) {
        return Ok(Err(format!(
            "it is stacked on {}'s `{}`, which is not merged yet: run `archon merge {}` first, \
             then merge this item again",
            open.parent_item, open.parent_branch, open.parent_item
        )));
    }
    if let Some(base) = super::restack::target_base(store, &shipped.item.id)? {
        return Ok(Ok(base));
    }
    let forked = recover::forked_base(store, &shipped.run)?;
    let named = |base: &String| !base.trim().is_empty() && !is_commit_id(base);
    let configured = match shipped.repo.base_branch.clone() {
        Some(base) => Some(base),
        None => RepoConfig::load(&shipped.repo.path)?.repo.base_branch,
    };
    match forked.filter(named).or(configured) {
        Some(base) => Ok(Ok(base)),
        None => Ok(Err(format!(
            "archond cannot tell which branch it is for: set the repository's base with \
             `archon repo set {} --base <branch>`",
            shipped.repo.name
        ))),
    }
}

/// Record as merged every unmerged parent that the ledger or git shows
/// merged, so a squash or fast-forward the restack sweep has not seen yet
/// does not block the item.
fn settle_merged_parents(store: &mut Store, shipped: &Shipped) -> Result<(), ArchonError> {
    for mut parent in store.stack_parents(&shipped.item.id)? {
        if !parent.is_open() || !parent_is_merged(store, shipped, &parent)? {
            continue;
        }
        parent.merged = true;
        store.save_stack_parent(&parent)?;
    }
    Ok(())
}

fn parent_is_merged(
    store: &Store,
    shipped: &Shipped,
    parent: &StackParent,
) -> Result<bool, ArchonError> {
    let mut in_ledger = false;
    for merge in store.merges_for_item(&parent.parent_item)? {
        // A retried parent delivers a new branch; only this branch's merge counts.
        if matches!(merge.state, MergeState::Merged { .. })
            && store.run(&merge.run)?.branch == parent.parent_branch
        {
            in_ledger = true;
            break;
        }
    }
    if in_ledger {
        return Ok(true);
    }
    let base = shipped
        .repo
        .base_branch
        .as_ref()
        .or(parent.repository_base.as_ref());
    let repo = &shipped.repo.path;
    let Some(base) = base else {
        return Ok(false);
    };
    let Some(base_tip) = worktree::commit_id(repo, &format!("refs/heads/{base}"))? else {
        return Ok(false);
    };
    let branch_ref = format!("refs/heads/{}", parent.parent_branch);
    let tip = worktree::commit_id(repo, &branch_ref)?.unwrap_or_else(|| parent.parent_sha.clone());
    assert!(is_commit_id(&tip), "a commit id");
    worktree::is_ancestor(repo, &tip, &base_tip)
}

fn fresh(
    shipped: &Shipped,
    authority: MergeAuthority,
    ignore_checks: bool,
    reason: Option<String>,
    state: MergeState,
) -> MergeRequest {
    let now = archon_core::now();
    MergeRequest {
        id: MergeRequestId::new(),
        item: shipped.item.id.clone(),
        run: shipped.run.clone(),
        repo: shipped.repo.id.clone(),
        pr_url: shipped.pr_url.clone(),
        head: shipped.head.clone(),
        authority,
        ignore_checks,
        reason,
        waived_by: None,
        waited: false,
        mergeability_wait_since: None,
        checks: None,
        state,
        requested_at: now,
        updated_at: now,
    }
}

/// The operator's own command (`archon merge`, `/merge`): merge now.
pub(crate) async fn merge_now(
    ctx: &DaemonContext,
    item: &QueueItemId,
    authority: MergeAuthority,
    ignore_checks: bool,
) -> Result<MergeRequest, ArchonError> {
    assert!(
        matches!(
            authority,
            MergeAuthority::Cli
                | MergeAuthority::ArchonChat
                | MergeAuthority::AetherChat
                | MergeAuthority::Github { .. }
        ),
        "only the operator's own command merges without a question"
    );
    let request = {
        let mut store = ctx.store.lock().await;
        let shipped = shipped(&store, item)?;
        end_waiting_for_checks(&mut store, item)?;
        let request = fresh(
            &shipped,
            authority,
            ignore_checks,
            None,
            MergeState::Merging,
        );
        store.insert_merge(&request)?;
        request
    };
    execute(ctx, request).await
}

/// The operator's own merge takes over a granted request still waiting for
/// its checks.
fn end_waiting_for_checks(store: &mut Store, item: &QueueItemId) -> Result<(), ArchonError> {
    for mut waiting in store.merges_for_item(item)? {
        let waits = matches!(
            waiting.state,
            MergeState::AwaitingChecks | MergeState::AwaitingMergeability
        );
        if !waits {
            continue;
        }
        waiting.state = MergeState::Refused {
            reason: "the operator merged it themselves".into(),
        };
        waiting.updated_at = archon_core::now();
        store.update_merge(&waiting)?;
    }
    Ok(())
}

/// The orchestrator's `request_merge`: refused at once when the pull
/// request cannot merge; under `grant_merge` merged when it is reviewed and
/// green (or its base waives checks), waiting while checks are pending;
/// otherwise the operator is asked to confirm.
pub(crate) async fn request(
    ctx: &DaemonContext,
    item: &QueueItemId,
    reason: Option<&str>,
) -> Result<MergeRequest, ArchonError> {
    // One line, so it cannot pose as the daemon's own lines in the question.
    let reason = reason
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|r| !r.is_empty())
        .map(|r| clip(&r, MAX_REASON_CHARS));
    let (shipped, base) = {
        let mut store = ctx.store.lock().await;
        let shipped = shipped(&store, item)?;
        let base = expected_base(&mut store, &shipped)?;
        (shipped, base)
    };
    let base = base.map_err(|why| refused_early(&shipped, &why))?;
    let (seen, undecided) = match shipped.local_branch() {
        Some(_) => {
            local_plan(&shipped, &base)
                .await?
                .map_err(|w| refused_early(&shipped, &w))?;
            (ChecksSeen::default(), false)
        }
        None => seen_on_github(ctx, &shipped, &base).await?,
    };
    supersede_stale_requests(&mut *ctx.store.lock().await, &shipped)?;
    if !(shipped.repo.grant_merge && shipped.reviewed) {
        return ask(ctx, &shipped, reason, &seen).await;
    }
    if shipped.local_branch().is_some() {
        return grant_local(ctx, shipped, reason).await;
    }
    if undecided {
        return wait_for_github(ctx, &shipped, reason, &seen).await;
    }
    match grant_route(shipped.waived_bases(), &base, &seen) {
        GrantRoute::Ask => ask(ctx, &shipped, reason, &seen).await,
        GrantRoute::Wait => wait_for_checks(ctx, &shipped, reason, &seen).await,
        route => {
            let waived = route == GrantRoute::Waived;
            let mut request = fresh(
                &shipped,
                MergeAuthority::Grant,
                waived,
                reason,
                MergeState::Merging,
            );
            request.waived_by = waived.then_some(base);
            ctx.store.lock().await.insert_merge(&request)?;
            execute(ctx, request).await
        }
    }
}

/// The checks GitHub shows on `shipped`'s pull request and whether its
/// mergeability is still undecided; `Err` refuses the request before anything is asked.
async fn seen_on_github(
    ctx: &DaemonContext,
    shipped: &Shipped,
    base: &str,
) -> Result<(ChecksSeen, bool), ArchonError> {
    assert!(shipped.local_branch().is_none(), "a pull request on GitHub");
    assert!(
        is_commit_id(&shipped.head),
        "a delivered head is a full commit id"
    );
    let facts = gh::merge_facts(&ctx.config.github.binary, &shipped.repo, &shipped.pr_url);
    let facts = facts.await?;
    if let Some(why) = moved_since_review(shipped, &facts) {
        return Err(refused_early(shipped, &why));
    }
    let expected = Expected {
        head: &shipped.head,
        base,
        ignore_checks: true,
    };
    match merge::assess(&facts, expected) {
        Ok(Assessment::Ready(seen)) => Ok((seen, false)),
        Ok(Assessment::Undecided(seen)) => Ok((seen, true)),
        Err(why) => Err(refused_early(shipped, &why)),
    }
}

/// A granted request for a local branch: no checks to wait for, so the
/// fast-forward goes ahead.
async fn grant_local(
    ctx: &DaemonContext,
    shipped: Shipped,
    reason: Option<String>,
) -> Result<MergeRequest, ArchonError> {
    let request = fresh(
        &shipped,
        MergeAuthority::Grant,
        false,
        reason,
        MergeState::Merging,
    );
    ctx.store.lock().await.insert_merge(&request)?;
    execute(ctx, request).await
}

/// What fast-forwarding the branch would do now, git read off the runtime.
async fn local_plan(
    shipped: &Shipped,
    base: &str,
) -> Result<Result<local_merge::Plan, String>, ArchonError> {
    let landing = shipped.landing(base).expect("a local branch");
    let owned = (landing.item.clone(), landing.repo.to_path_buf());
    let (branch, base, head) = (
        landing.branch.to_owned(),
        base.to_owned(),
        shipped.head.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let landing = Landing {
            item: &owned.0,
            repo: &owned.1,
            branch: &branch,
            base: &base,
            head: &head,
        };
        local_merge::check(landing)
    })
    .await
    .map_err(|e| ArchonError::io("local merge check task", std::io::Error::other(e)))?
}

/// What a `ship = local` item's branch changes against its base, for
/// `archon diff`; also whether `archon merge` would land it now.
pub(crate) async fn local_review(
    ctx: &DaemonContext,
    item: &QueueItemId,
    full: bool,
) -> Result<archon_core::merge::LocalReview, ArchonError> {
    let (shipped, base) = {
        let mut store = ctx.store.lock().await;
        let shipped = shipped(&store, item)?;
        let base = expected_base(&mut store, &shipped)?;
        (shipped, base)
    };
    let base = base.map_err(ArchonError::InvalidTransition)?;
    let Some(branch) = shipped.local_branch() else {
        return Err(ArchonError::Invalid(format!(
            "{item} was delivered as {}, not as a local branch; review it there",
            shipped.pr_url
        )));
    };
    let (id, repo, branch, head) = (
        item.clone(),
        shipped.repo.path.clone(),
        branch.to_owned(),
        shipped.head.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let landing = Landing {
            item: &id,
            repo: &repo,
            branch: &branch,
            base: &base,
            head: &head,
        };
        local_merge::review(landing, full)
    })
    .await
    .map_err(|e| ArchonError::io("local review task", std::io::Error::other(e)))?
}

/// Close an open request that waits on a commit other than the one the pull
/// request now shows, so a fresh request is not blocked by the stale one.
fn supersede_stale_requests(store: &mut Store, shipped: &Shipped) -> Result<(), ArchonError> {
    for mut stale in store.merges_for_item(&shipped.item.id)? {
        let waiting = matches!(
            stale.state,
            MergeState::AwaitingOperator
                | MergeState::AwaitingChecks
                | MergeState::AwaitingMergeability
        );
        if !waiting || stale.head == shipped.head {
            continue;
        }
        stale.state = MergeState::Refused {
            reason: format!(
                "superseded: the pull request head moved from {} to {}",
                merge::short(&stale.head),
                merge::short(&shipped.head)
            ),
        };
        stale.updated_at = archon_core::now();
        store.update_merge(&stale)?;
    }
    Ok(())
}

/// Record a granted request waiting for its pending checks.
async fn wait_for_checks(
    ctx: &DaemonContext,
    shipped: &Shipped,
    reason: Option<String>,
    seen: &ChecksSeen,
) -> Result<MergeRequest, ArchonError> {
    assert!(
        !seen.pending.is_empty() && seen.failed.is_empty(),
        "only pending checks wait"
    );
    let mut waiting = fresh(
        shipped,
        MergeAuthority::Grant,
        false,
        reason,
        MergeState::AwaitingChecks,
    );
    waiting.waited = true;
    waiting.checks = Some(seen.describe());
    ctx.store.lock().await.insert_merge(&waiting)?;
    Ok(waiting)
}

/// Record a granted request waiting for GitHub to decide whether the pull
/// request merges cleanly.
async fn wait_for_github(
    ctx: &DaemonContext,
    shipped: &Shipped,
    reason: Option<String>,
    seen: &ChecksSeen,
) -> Result<MergeRequest, ArchonError> {
    let mut waiting = fresh(
        shipped,
        MergeAuthority::Grant,
        false,
        reason,
        MergeState::AwaitingMergeability,
    );
    waiting.mergeability_wait_since = Some(waiting.requested_at);
    waiting.checks = Some(seen.describe());
    ctx.store.lock().await.insert_merge(&waiting)?;
    Ok(waiting)
}

/// Turn on GitHub's auto-merge of a succeeded item's pull request, pinned to
/// the commit its delivery reviewed. Nothing is written unless the pull
/// request is open, targets the branch its change is for, and still shows
/// that commit; `Err` says why not. GitHub merges it once its checks allow.
pub(crate) async fn enable_auto_merge(
    ctx: &DaemonContext,
    item: &QueueItemId,
) -> Result<(QueueItem, String), ArchonError> {
    let (shipped, base) = {
        let mut store = ctx.store.lock().await;
        let shipped = shipped(&store, item)?;
        let base = expected_base(&mut store, &shipped)?;
        (shipped, base)
    };
    let refused = |why: &str| {
        assert!(!why.is_empty(), "a refusal gives its reason");
        ArchonError::InvalidTransition(format!(
            "auto-merge of {} of {item} cannot be turned on: {why}",
            shipped.subject()
        ))
    };
    if shipped.local_branch().is_some() {
        return Err(ArchonError::Invalid(format!(
            "{item} was delivered to a local branch, which has no pull request to auto-merge; \
             land it with `archon merge {item}`"
        )));
    }
    if shipped.review_of {
        return Err(ArchonError::Invalid(format!(
            "{item} reviewed {}, which is not its own pull request: GitHub's auto-merge is not \
             turned on for it; ask for the merge with `request_merge` or `archon merge {item}`",
            shipped.pr_url
        )));
    }
    let base = base.map_err(|why| refused(&why))?;
    let binary = &ctx.config.github.binary;
    let facts = gh::merge_facts(binary, &shipped.repo, &shipped.pr_url).await?;
    // Checks may be pending: auto-merge waits for them.
    let expected = Expected {
        head: &shipped.head,
        base: &base,
        ignore_checks: true,
    };
    merge::evaluate(&facts, expected).map_err(|why| refused(&why))?;
    let pinned = pin_auto_merge(binary, &shipped.repo, &shipped.pr_url, Some(&shipped.head));
    pinned.await.map_err(|why| match why {
        MergeRefusal::Retry(why) => ArchonError::Unavailable(format!(
            "GitHub did not take auto-merge of {}: {why}; try again",
            shipped.pr_url
        )),
        MergeRefusal::Never(why) => refused(&format!("GitHub refused it: {why}")),
    })?;
    let note = format!(
        "Auto-merge is on for {} at {}: it is delivered, not merged yet; GitHub merges it once \
         its checks and reviews pass",
        shipped.pr_url,
        merge::short(&shipped.head)
    );
    Ok((shipped.item, note))
}

/// A request refused before anything was asked or written: the orchestrator
/// is told why, in the error.
fn refused_early(shipped: &Shipped, why: &str) -> ArchonError {
    assert!(!why.is_empty(), "a refusal gives its reason");
    ArchonError::InvalidTransition(format!(
        "{} of {} cannot be merged now: {why}",
        shipped.subject(),
        shipped.item.id
    ))
}

/// Ask the operator to confirm, recording the request waiting on it.
async fn ask(
    ctx: &DaemonContext,
    shipped: &Shipped,
    reason: Option<String>,
    seen: &ChecksSeen,
) -> Result<MergeRequest, ArchonError> {
    let question = Question {
        id: QuestionId::new(),
        number: 0,
        text: question_text(shipped, reason.as_deref(), seen),
        options: MERGE_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
        item: Some(shipped.item.id.clone()),
        aether: None,
        asked_by: None,
        asked_at: archon_core::now(),
        answer: None,
        answered_at: None,
    };
    let placeholder = MergeAuthority::Confirmed {
        question: question.id.clone(),
        number: 0,
    };
    let mut waiting = fresh(
        shipped,
        placeholder,
        false,
        reason,
        MergeState::AwaitingOperator,
    );
    waiting.checks = shipped.local_branch().is_none().then(|| seen.describe());
    let (merge, feed_item) = ctx.store.lock().await.ask_merge(&question, waiting)?;
    assert!(
        merge.waiting_on().is_some_and(|n| n > 0),
        "numbered by the store"
    );
    crate::feed::publish(ctx, vec![feed_item]);
    Ok(merge)
}

fn question_text(shipped: &Shipped, reason: Option<&str>, seen: &ChecksSeen) -> String {
    let mut text = format!(
        "Merge {} of {} ({}) at {}?\nThe orchestrator asked",
        shipped.subject(),
        shipped.item.id,
        shipped.item.title,
        merge::short(&shipped.head)
    );
    match reason {
        Some(reason) => text.push_str(&format!(": {reason}")),
        None => text.push('.'),
    }
    if shipped.local_branch().is_some() {
        text.push_str("\nIt is fast-forwarded into its base branch; the branch is kept.");
        text.push_str("\nAnswer `yes` or `no`.");
        return text;
    }
    text.push_str(&format!("\nChecks: {}.", seen.describe()));
    if !seen.all_green() {
        text.push_str(" Only `yes ignore-checks` merges it so.");
    }
    text.push_str("\nAnswer `yes`, `yes ignore-checks` or `no`.");
    text
}

/// The operator answered the confirmation question of `merge`. An answer
/// that is not one of its options is refused, so the question stays open.
pub(crate) async fn decide(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
    text: &str,
    via: AnswerVia,
) -> Result<(), ArchonError> {
    assert!(
        merge.waiting_on().is_some(),
        "only a waiting request is decided"
    );
    let answer = MergeAnswer::parse(text).map_err(ArchonError::Invalid)?;
    merge.updated_at = archon_core::now();
    match answer {
        MergeAnswer::No => {
            merge.state = MergeState::Declined;
            let feed = ctx.store.lock().await.decide_merge(&merge, text, via)?;
            crate::feed::publish(ctx, feed);
            report(ctx, &merge).await
        }
        MergeAnswer::Yes { ignore_checks } => {
            merge.ignore_checks = ignore_checks;
            merge.state = MergeState::Merging;
            let feed = ctx.store.lock().await.decide_merge(&merge, text, via)?;
            crate::feed::publish(ctx, feed);
            execute(ctx, merge).await.map(drop)
        }
    }
}

/// Merge the recorded request, then record and report how that ended. Only
/// the store failing is an error; GitHub's refusals are the outcome.
async fn execute(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
) -> Result<MergeRequest, ArchonError> {
    assert_eq!(merge.state, MergeState::Merging, "recorded before GitHub");
    if merge.local_branch().is_some() {
        return execute_local(ctx, merge).await;
    }
    let ready = match ready(ctx, &merge).await? {
        Readiness::Ready(ready) => ready,
        Readiness::Undecided => return park_for_github(ctx, merge).await,
        Readiness::Refused(why) => {
            merge.state = MergeState::Refused { reason: why };
            return finish_merge(ctx, merge).await;
        }
    };
    merge.checks = Some(ready.seen.describe());
    if let Some(why) = checks_not_green(&ready) {
        merge.state = MergeState::Refused { reason: why };
        return finish_merge(ctx, merge).await;
    }
    let binary = &ctx.config.github.binary;
    let method = match merge_method(binary, &ready.repo).await {
        Ok(method) => method,
        Err(MergeRefusal::Retry(why) | MergeRefusal::Never(why)) => {
            merge.state = MergeState::Refused { reason: why };
            return finish_merge(ctx, merge).await;
        }
    };
    let args = [
        "pr",
        "merge",
        &merge.pr_url,
        "--match-head-commit",
        &merge.head,
        method,
    ];
    let sent = gh::run(binary, &ready.repo, &args, None).await;
    match confirm(binary, &ready.repo, &merge, method, sent).await {
        Some(state) => {
            merge.state = state;
            finish_merge(ctx, merge).await
        }
        // Left `merging`: the coordination pass settles it from GitHub.
        None => Ok(merge),
    }
}

/// Fast-forward the base to the reviewed commit of a local branch, then
/// record how that ended. The branch is kept either way.
async fn execute_local(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
) -> Result<MergeRequest, ArchonError> {
    assert!(merge.local_branch().is_some(), "a local request");
    merge.state = match land_local(ctx, &merge).await? {
        Ok(method) => MergeState::Merged {
            method,
            actor: None,
            observed: false,
        },
        Err(reason) => MergeState::Refused { reason },
    };
    finish_merge(ctx, merge).await
}

/// Everything a local merge needs, read again just before it, then the
/// fast-forward. `Ok` names the method; `Err` is the refusal.
async fn land_local(
    ctx: &DaemonContext,
    merge: &MergeRequest,
) -> Result<Result<String, String>, ArchonError> {
    let (shipped, base) = {
        let mut store = ctx.store.lock().await;
        let shipped = match shipped(&store, &merge.item) {
            Ok(shipped) => shipped,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => return Ok(Err(error.to_string())),
        };
        let base = match expected_base(&mut store, &shipped) {
            Ok(Ok(base)) => base,
            Ok(Err(why)) => return Ok(Err(why)),
            Err(error) => return Err(error),
        };
        (shipped, base)
    };
    if (&shipped.run, &shipped.pr_url, &shipped.head) != (&merge.run, &merge.pr_url, &merge.head) {
        return Ok(Err(
            "the item was delivered again since it was asked for; ask again".into(),
        ));
    }
    let (item, repo) = (shipped.item.id.clone(), shipped.repo.path.clone());
    let branch = shipped.local_branch().expect("a local delivery").to_owned();
    let (head, into) = (shipped.head.clone(), base.clone());
    let resuming = merge.requested_at < ctx.started_at;
    let landed = tokio::task::spawn_blocking(move || {
        let landing = Landing {
            item: &item,
            repo: &repo,
            branch: &branch,
            base: &into,
            head: &head,
        };
        let plan = match local_merge::check(landing)? {
            Ok(local_merge::Plan::AlreadyIn) if !resuming => {
                return Ok(Err(format!(
                    "`{into}` already holds `{branch}`; {}",
                    archon_core::merge::NOTHING_TO_MERGE
                )));
            }
            Ok(plan) => plan,
            Err(why) => return Ok(Err(why)),
        };
        local_merge::land(landing, &plan)
    })
    .await
    .map_err(|e| ArchonError::io("local merge task", std::io::Error::other(e)))??;
    Ok(landed.map(|()| format!("fast-forward into `{base}`")))
}

/// A request whose pull request may be merged now.
struct Ready {
    repo: Repo,
    base: String,
    seen: ChecksSeen,
    /// The pull request is another's, passed by a review item.
    review_of: bool,
}

/// What reading the pull request again just before a merge decided.
enum Readiness {
    Ready(Box<Ready>),
    /// GitHub has not decided whether it merges cleanly.
    Undecided,
    Refused(String),
}

/// Check again, just before merging, everything a merge needs.
async fn ready(ctx: &DaemonContext, merge: &MergeRequest) -> Result<Readiness, ArchonError> {
    let (shipped, base) = {
        let mut store = ctx.store.lock().await;
        let shipped = match shipped(&store, &merge.item) {
            Ok(shipped) => shipped,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => return Ok(Readiness::Refused(error.to_string())),
        };
        let base = match expected_base(&mut store, &shipped) {
            Ok(base) => base,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => Err(error.to_string()),
        };
        (shipped, base)
    };
    if (&shipped.run, &shipped.pr_url, &shipped.head) != (&merge.run, &merge.pr_url, &merge.head) {
        return Ok(Readiness::Refused(
            "the item was delivered again since it was asked for; ask again".into(),
        ));
    }
    let base = match base {
        Ok(base) => base,
        Err(why) => return Ok(Readiness::Refused(why)),
    };
    let binary = &ctx.config.github.binary;
    let facts = match gh::merge_facts(binary, &shipped.repo, &merge.pr_url).await {
        Ok(facts) => facts,
        Err(error) => {
            return Ok(Readiness::Refused(format!("{GITHUB_UNREADABLE}: {error}")));
        }
    };
    let facts = match retargeted(ctx, &shipped, &base, facts).await {
        Ok(facts) => facts,
        Err(why) => return Ok(Readiness::Refused(why)),
    };
    if let Some(why) = moved_since_review(&shipped, &facts) {
        return Ok(Readiness::Refused(why));
    }
    let expected = Expected {
        head: &merge.head,
        base: &base,
        ignore_checks: merge.ignore_checks,
    };
    Ok(match merge::assess(&facts, expected) {
        Ok(Assessment::Ready(seen)) => Readiness::Ready(Box::new(Ready {
            repo: shipped.repo.clone(),
            base,
            seen,
            review_of: shipped.review_of,
        })),
        Ok(Assessment::Undecided(_)) => Readiness::Undecided,
        Err(why) => Readiness::Refused(why),
    })
}

/// A pull request still aimed at a parent branch whose pull request merged or
/// closed is moved onto the base, then read again.
async fn retargeted(
    ctx: &DaemonContext,
    shipped: &Shipped,
    base: &str,
    facts: PullRequestFacts,
) -> Result<PullRequestFacts, String> {
    if facts.base == base || facts.state != "OPEN" {
        return Ok(facts);
    }
    let stacked = {
        let store = ctx.store.lock().await;
        let parents = store
            .stack_parents(&shipped.item.id)
            .map_err(|e| e.to_string())?;
        parents
            .iter()
            .any(|p| !p.is_open() && p.parent_branch == facts.base)
    };
    if !stacked {
        return Ok(facts);
    }
    let binary = &ctx.config.github.binary;
    let moved = ["pr", "edit", &shipped.pr_url, "--base", base];
    gh::text(binary, &shipped.repo, &moved)
        .await
        .map_err(|error| {
            format!(
                "its parent merged but the pull request could not be retargeted to `{base}`: \
             {error}; run `gh pr edit {} --base {base}`, then merge again",
                shipped.pr_url
            )
        })?;
    gh::merge_facts(binary, &shipped.repo, &shipped.pr_url)
        .await
        .map_err(|error| format!("{GITHUB_UNREADABLE}: {error}"))
}

/// How the merge ended, read back from GitHub; `None` when it cannot be
/// read, after a `gh pr merge` that may have merged.
async fn confirm(
    binary: &Path,
    repo: &Repo,
    merge: &MergeRequest,
    method: &str,
    sent: Result<gh::Ran, ArchonError>,
) -> Option<MergeState> {
    let said = match &sent {
        Ok(ran) if ran.ok => None,
        Ok(ran) => Some(ran.detail()),
        Err(error) => Some(error.to_string()),
    };
    let after = match gh::merge_facts(binary, repo, &merge.pr_url).await {
        Ok(after) => after,
        Err(error) => {
            tracing::warn!(merge = %merge.id, %error, "pull request not read after the merge");
            return match &sent {
                Ok(ran) if !ran.ok => Some(MergeState::Refused {
                    reason: format!("GitHub refused the merge: {}", ran.detail()),
                }),
                // It may have merged: the coordination pass settles it.
                _ => None,
            };
        }
    };
    Some(settled(&after, merge, method.trim_start_matches('-'), said))
}

/// The request's end, from what GitHub shows after the merge was sent.
fn settled(
    after: &archon_core::merge::PullRequestFacts,
    merge: &MergeRequest,
    method: &str,
    said: Option<String>,
) -> MergeState {
    if after.state == "MERGED" && after.head == merge.head {
        return MergeState::Merged {
            method: method.to_owned(),
            actor: after.merged_by.clone(),
            observed: said.is_some(),
        };
    }
    if after.state == "OPEN" && after.head == merge.head && (said.is_none() || after.auto_merge) {
        return MergeState::Queued;
    }
    let reason = match said {
        Some(said) => format!("GitHub refused the merge: {said}"),
        None => format!(
            "`gh pr merge` was accepted, but GitHub lists the pull request as {}: a merge \
             queue or a branch rule holds it, and it may still merge",
            after.state.to_lowercase()
        ),
    };
    MergeState::Refused { reason }
}

async fn finish_merge(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
) -> Result<MergeRequest, ArchonError> {
    assert!(
        !matches!(
            merge.state,
            MergeState::AwaitingOperator
                | MergeState::AwaitingChecks
                | MergeState::AwaitingMergeability
                | MergeState::Merging
        ),
        "only a known outcome is reported"
    );
    merge.updated_at = archon_core::now();
    ctx.store.lock().await.update_merge(&merge)?;
    report(ctx, &merge).await?;
    if matches!(merge.state, MergeState::Merged { .. }) {
        // Stacked dependents restack over the merge now, not at the next timed pass.
        ctx.coordination.changed.notify_one();
    }
    Ok(merge)
}

/// The audit note in the feed, and `merge_finished` news for the orchestrator.
async fn report(ctx: &DaemonContext, merge: &MergeRequest) -> Result<(), ArchonError> {
    let who = merge.authority.describe();
    let (kind, text) = match &merge.state {
        MergeState::Merged {
            method,
            actor,
            observed,
        } => {
            let checks = match (merge.ignore_checks, &merge.checks) {
                _ if merge.waived_by.is_some() => format!(
                    "; checks waived by the repository's merge_waive_checks for `{}` ({})",
                    merge.waived_by.as_deref().unwrap_or_default(),
                    merge.checks.as_deref().unwrap_or("not read")
                ),
                (true, Some(checks)) => {
                    format!("; the operator acknowledged the checks ({checks})")
                }
                (_, Some(checks)) if merge.waited => {
                    format!("; waited for its checks to finish: {checks}")
                }
                (_, Some(checks)) => format!("; checks: {checks}"),
                (_, None) => String::new(),
            };
            let text = match merge.local_branch() {
                Some(_) => format!(
                    "Merged {} of {} at {} ({method}); requested on {who}. The branch is kept.",
                    merge.subject(),
                    merge.item,
                    merge::short(&merge.head)
                ),
                None => format!(
                    "Merged {} of {} at {} ({method}); {}; requested on {who}{checks}.",
                    merge.pr_url,
                    merge.item,
                    merge::short(&merge.head),
                    merge::merged_attribution(actor.as_deref(), *observed)
                ),
            };
            (NotificationKind::Outcome, text)
        }
        MergeState::Queued => (
            NotificationKind::Outcome,
            format!(
                "Queued {} of {} at GitHub on {who}; waiting for it to merge or close.",
                merge.subject(),
                merge.item
            ),
        ),
        MergeState::Declined => {
            let text = format!(
                "Not merged: the operator declined {} of {}.",
                merge.subject(),
                merge.item
            );
            (NotificationKind::Outcome, text)
        }
        MergeState::Refused { reason } => {
            let text = format!(
                "Not merged: {} of {}, on {who}: {reason}",
                merge.subject(),
                merge.item
            );
            match merge.state.is_nothing_to_merge() {
                true => (NotificationKind::Outcome, text),
                false => (NotificationKind::Blocker, text),
            }
        }
        MergeState::AwaitingOperator
        | MergeState::AwaitingChecks
        | MergeState::AwaitingMergeability
        | MergeState::Merging => {
            return Ok(());
        }
    };
    finish::notify(ctx, kind, &text, Some(&merge.item), Some(&merge.run)).await?;
    if merge.state == MergeState::Queued {
        return Ok(());
    }
    let news = TriggerEvent::MergeFinished {
        item: merge.item.clone(),
        pr_url: merge.pr_url.clone(),
        merged: matches!(merge.state, MergeState::Merged { .. }),
        text,
    };
    match merge.authority.orchestrator_asked() {
        true => triggers::push(ctx, news).await,
        false => triggers::push_quiet(ctx, news).await,
    }
}

/// Reconcile queued and interrupted requests from GitHub, closing any
/// legacy confirmation left open. Returns how many requests ended.
pub(crate) async fn settle_stale(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    ctx.store.lock().await.reconcile_merge_questions()?;
    let cutoff = archon_core::now() - STALE_AFTER;
    let open = ctx.store.lock().await.open_merges()?;
    let mut ended = 0;
    for merge in open {
        let stale = merge.updated_at < ctx.started_at || merge.updated_at < cutoff;
        if merge.local_branch().is_some() {
            if merge.state == MergeState::Merging && stale {
                let (subject, item) = (format!("merge:{}", merge.id), merge.item.clone());
                let result = execute_local(ctx, merge).await.map(|_| ());
                ended += usize::from(result.is_ok());
                let what = format!("Settling the interrupted merge of {item}");
                coordination::isolate(ctx, &subject, Some(&item), &what, result).await?;
            }
            continue;
        }
        if merge.state == MergeState::AwaitingChecks {
            let (subject, item) = (format!("merge:{}", merge.id), merge.item.clone());
            let result = resume_waiting(ctx, merge).await;
            if let Ok(settled) = &result {
                ended += usize::from(*settled);
            }
            let what = format!("Waiting for the checks of the merge of {item}");
            coordination::isolate(ctx, &subject, Some(&item), &what, result.map(drop)).await?;
            continue;
        }
        if merge.state != MergeState::Queued && (merge.state != MergeState::Merging || !stale) {
            continue;
        }
        let (subject, item) = (format!("merge:{}", merge.id), merge.item.clone());
        let result = settle_one(ctx, merge).await;
        if let Ok(settled) = &result {
            ended += usize::from(*settled);
        }
        let what = format!("Settling the interrupted merge of {item}");
        coordination::isolate(ctx, &subject, Some(&item), &what, result.map(drop)).await?;
    }
    Ok(ended)
}

/// Read a waiting request's checks again: merge when all are green, ask the
/// operator when one failed or the wait ran out, refuse when the pull request
/// can no longer merge. `true` when the request left the waiting state.
async fn resume_waiting(ctx: &DaemonContext, mut merge: MergeRequest) -> Result<bool, ArchonError> {
    assert_eq!(
        merge.state,
        MergeState::AwaitingChecks,
        "only a waiting request resumes"
    );
    let probe = MergeRequest {
        ignore_checks: true,
        ..merge.clone()
    };
    let timed_out = archon_core::now() - merge.requested_at >= WAIT_FOR_CHECKS;
    let seen = match ready(ctx, &probe).await? {
        Readiness::Ready(ready) => ready.seen,
        // A read that failed is retried on the next pass, until the wait ends.
        Readiness::Refused(why) if why.starts_with(GITHUB_UNREADABLE) && !timed_out => {
            return Ok(false);
        }
        Readiness::Undecided => {
            merge.state = MergeState::AwaitingMergeability;
            merge.updated_at = archon_core::now();
            merge
                .mergeability_wait_since
                .get_or_insert(merge.updated_at);
            ctx.store.lock().await.update_merge(&merge)?;
            return Ok(false);
        }
        Readiness::Refused(why) => {
            merge.state = MergeState::Refused { reason: why };
            finish_merge(ctx, merge).await?;
            return Ok(true);
        }
    };
    let shipped = shipped(&*ctx.store.lock().await, &merge.item)?;
    let granted = shipped.repo.grant_merge && shipped.reviewed;
    let still_pending = seen.failed.is_empty() && !seen.pending.is_empty();
    if granted && seen.all_green() {
        merge.state = MergeState::Merging;
        merge.updated_at = archon_core::now();
        ctx.store.lock().await.update_merge(&merge)?;
        return execute(ctx, merge)
            .await
            .map(|m| m.state != MergeState::Merging);
    }
    if granted && still_pending && !timed_out {
        return Ok(false);
    }
    let why = match (granted, timed_out && still_pending) {
        (false, _) => "the repository no longer grants the merge",
        (true, true) => "its checks were still pending after the wait",
        (true, false) => "its checks did not all pass",
    };
    merge.state = MergeState::Refused {
        reason: format!("{why}; the operator was asked instead"),
    };
    merge.updated_at = archon_core::now();
    ctx.store.lock().await.update_merge(&merge)?;
    ask(ctx, &shipped, merge.reason.clone(), &seen).await?;
    Ok(true)
}

/// GitHub has not decided whether the pull request merges cleanly: keep the
/// request, and any confirmation it holds, and read the pull request again
/// later (see [`poll_mergeability`]).
async fn park_for_github(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
) -> Result<MergeRequest, ArchonError> {
    assert_eq!(merge.state, MergeState::Merging, "parked from a merge");
    merge.state = MergeState::AwaitingMergeability;
    merge.updated_at = archon_core::now();
    merge
        .mergeability_wait_since
        .get_or_insert(merge.updated_at);
    ctx.store.lock().await.update_merge(&merge)?;
    Ok(merge)
}

/// When the request began waiting for GitHub. A request stored before the
/// start was recorded counts from its last update, so its wait is not cut short.
fn wait_started(merge: &MergeRequest) -> archon_core::Timestamp {
    merge.mergeability_wait_since.unwrap_or(merge.updated_at)
}

/// How long to leave a request waiting for GitHub before reading it again:
/// as long as it had waited at its last read, from 5 to 60 seconds.
fn mergeability_pause(waited_at_last_read: chrono::TimeDelta) -> chrono::TimeDelta {
    waited_at_last_read.clamp(MERGEABILITY_PAUSE_MIN, MERGEABILITY_PAUSE_MAX)
}

/// Read again every request waiting for GitHub's mergeability check whose
/// pause is over. Returns how many left the waiting state.
pub(crate) async fn poll_mergeability(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let now = archon_core::now();
    let open = ctx.store.lock().await.open_merges()?;
    let mut ended = 0;
    for merge in open {
        let due = merge.state == MergeState::AwaitingMergeability
            && now - merge.updated_at
                >= mergeability_pause(merge.updated_at - wait_started(&merge));
        if !due {
            continue;
        }
        let (subject, item) = (format!("merge:{}", merge.id), merge.item.clone());
        let result = resume_undecided(ctx, merge).await;
        if let Ok(settled) = &result {
            ended += usize::from(*settled);
        }
        let what = format!("Waiting for GitHub's mergeability check of the merge of {item}");
        coordination::isolate(ctx, &subject, Some(&item), &what, result.map(drop)).await?;
    }
    Ok(ended)
}

/// What a request does once GitHub says the pull request merges cleanly.
enum Next {
    Merge { waived_by: Option<String> },
    WaitForChecks,
    AskOperator(&'static str),
}

/// Under a grant the request takes the route its checks give; one the
/// operator confirmed merges, its checks judged as they were confirmed.
fn next_step(merge: &MergeRequest, shipped: &Shipped, ready: &Ready) -> Next {
    if merge.authority != MergeAuthority::Grant {
        return Next::Merge { waived_by: None };
    }
    let granted = shipped.repo.grant_merge && shipped.reviewed;
    if !granted {
        return Next::AskOperator("the repository no longer grants the merge");
    }
    match grant_route(shipped.waived_bases(), &ready.base, &ready.seen) {
        GrantRoute::Waived => Next::Merge {
            waived_by: Some(ready.base.clone()),
        },
        GrantRoute::Green => Next::Merge { waived_by: None },
        GrantRoute::Wait => Next::WaitForChecks,
        GrantRoute::Ask => Next::AskOperator("its checks did not all pass"),
    }
}

/// Read a request waiting for GitHub again: merge once it merges cleanly,
/// refuse when it conflicts, its head moved or the wait ran out. `true` when
/// the request left the waiting state.
async fn resume_undecided(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
) -> Result<bool, ArchonError> {
    assert_eq!(
        merge.state,
        MergeState::AwaitingMergeability,
        "only a waiting request resumes"
    );
    // Kept from here on, so a retry's new `updated_at` never moves the start.
    merge.mergeability_wait_since = Some(wait_started(&merge));
    let probe = MergeRequest {
        ignore_checks: true,
        ..merge.clone()
    };
    let timed_out = archon_core::now() - wait_started(&merge) >= WAIT_FOR_MERGEABILITY;
    let refusal = match ready(ctx, &probe).await? {
        Readiness::Ready(ready) => return proceed_when_mergeable(ctx, merge, *ready).await,
        Readiness::Undecided if timed_out => format!(
            "GitHub did not finish its mergeability check within {} minutes; run `gh pr view \
             {} --json mergeable`, and once it says MERGEABLE ask for the merge again",
            WAIT_FOR_MERGEABILITY.num_minutes(),
            merge.pr_url
        ),
        // A read that failed is retried on the next pass, until the wait ends.
        Readiness::Refused(why) if why.starts_with(GITHUB_UNREADABLE) && !timed_out => {
            merge.updated_at = archon_core::now();
            ctx.store.lock().await.update_merge(&merge)?;
            return Ok(false);
        }
        Readiness::Undecided => {
            merge.updated_at = archon_core::now();
            ctx.store.lock().await.update_merge(&merge)?;
            return Ok(false);
        }
        Readiness::Refused(why) => why,
    };
    merge.state = MergeState::Refused { reason: refusal };
    finish_merge(ctx, merge).await?;
    Ok(true)
}

async fn proceed_when_mergeable(
    ctx: &DaemonContext,
    mut merge: MergeRequest,
    ready: Ready,
) -> Result<bool, ArchonError> {
    let shipped = shipped(&*ctx.store.lock().await, &merge.item)?;
    match next_step(&merge, &shipped, &ready) {
        Next::Merge { waived_by } => {
            merge.ignore_checks |= waived_by.is_some();
            merge.waived_by = waived_by;
            merge.state = MergeState::Merging;
            merge.updated_at = archon_core::now();
            ctx.store.lock().await.update_merge(&merge)?;
            let after = execute(ctx, merge).await?;
            Ok(after.state != MergeState::AwaitingMergeability)
        }
        Next::WaitForChecks => {
            merge.state = MergeState::AwaitingChecks;
            merge.mergeability_wait_since = None;
            merge.waited = true;
            merge.checks = Some(ready.seen.describe());
            merge.updated_at = archon_core::now();
            ctx.store.lock().await.update_merge(&merge)?;
            Ok(false)
        }
        Next::AskOperator(why) => {
            merge.state = MergeState::Refused {
                reason: format!("{why}; the operator was asked instead"),
            };
            merge.updated_at = archon_core::now();
            ctx.store.lock().await.update_merge(&merge)?;
            ask(ctx, &shipped, merge.reason.clone(), &ready.seen).await?;
            Ok(true)
        }
    }
}

async fn settle_one(ctx: &DaemonContext, mut merge: MergeRequest) -> Result<bool, ArchonError> {
    let repo = ctx
        .store
        .lock()
        .await
        .repo(&crate::resolve::repo_ref(&merge.repo))?;
    let binary = &ctx.config.github.binary;
    let after = gh::merge_facts(binary, &repo, &merge.pr_url).await?;
    merge.state = match (after.state.as_str(), after.head == merge.head) {
        ("MERGED", true) => MergeState::Merged {
            method: "confirmed later by GitHub".into(),
            actor: after.merged_by.clone(),
            observed: true,
        },
        // An interrupted send may have entered a merge queue even when
        // autoMergeRequest is absent. Keep ownership until GitHub settles it.
        ("OPEN", true) => {
            if merge.state != MergeState::Queued {
                merge.state = MergeState::Queued;
                finish_merge(ctx, merge).await?;
            }
            return Ok(false);
        }
        (state, _) => MergeState::Refused {
            reason: format!(
                "GitHub lists the requested pull request as \
                 {} at {}; ask again",
                state.to_lowercase(),
                merge::short(&after.head)
            ),
        },
    };
    finish_merge(ctx, merge).await.map(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::merge::PullRequestFacts;

    fn request() -> MergeRequest {
        let now = archon_core::now();
        MergeRequest {
            id: MergeRequestId::new(),
            item: QueueItemId::new(),
            run: PipelineRunId::new(),
            repo: archon_core::id::RepoId::new(),
            pr_url: "https://github.com/o/r/pull/7".into(),
            head: "a".repeat(40),
            authority: MergeAuthority::Cli,
            ignore_checks: false,
            reason: None,
            waived_by: None,
            waited: false,
            mergeability_wait_since: None,
            checks: None,
            state: MergeState::Merging,
            requested_at: now,
            updated_at: now,
        }
    }

    fn after(state: &str, head: &str) -> PullRequestFacts {
        PullRequestFacts {
            state: state.into(),
            is_draft: false,
            head: head.into(),
            base: "main".into(),
            mergeable: "UNKNOWN".into(),
            checks: vec![],
            merged_by: None,
            auto_merge: false,
        }
    }

    #[test]
    fn a_wait_stored_without_its_start_counts_from_its_last_update() {
        let mut merge = request();
        merge.state = MergeState::AwaitingMergeability;
        assert_eq!(wait_started(&merge), merge.updated_at);
        let begun = merge.updated_at - chrono::TimeDelta::minutes(3);
        merge.mergeability_wait_since = Some(begun);
        assert_eq!(wait_started(&merge), begun);
    }

    #[test]
    fn only_github_showing_the_reviewed_commit_merged_counts_as_merged() {
        let merge = request();
        let merged = settled(&after("MERGED", &merge.head), &merge, "squash", None);
        assert_eq!(
            merged,
            MergeState::Merged {
                actor: None,
                observed: false,
                method: "squash".into()
            }
        );
        let moved = settled(&after("MERGED", &"b".repeat(40)), &merge, "squash", None);
        assert_eq!(
            moved.kind(),
            "refused",
            "merged at another commit is not this merge"
        );
        let queued = settled(&after("OPEN", &merge.head), &merge, "squash", None);
        assert_eq!(queued, MergeState::Queued);
        let said = Some("Head branch was modified".to_owned());
        let MergeState::Refused { reason } =
            settled(&after("OPEN", &merge.head), &merge, "squash", said)
        else {
            panic!("a refused merge is refused");
        };
        assert!(reason.contains("Head branch was modified"), "{reason}");
    }

    #[test]
    fn a_grant_waits_only_while_checks_are_pending_and_none_failed() {
        let seen = |green, pending: &[&str], failed: &[&str]| ChecksSeen {
            green,
            pending: pending.iter().map(|n| (*n).to_owned()).collect(),
            failed: failed.iter().map(|n| (*n).to_owned()).collect(),
        };
        let none: &[String] = &[];
        let next = ["next".to_owned()];
        assert_eq!(
            grant_route(none, "main", &seen(2, &[], &[])),
            GrantRoute::Green
        );
        assert_eq!(
            grant_route(none, "main", &seen(1, &["lint"], &[])),
            GrantRoute::Wait
        );
        assert_eq!(
            grant_route(none, "main", &seen(0, &["lint"], &["ci"])),
            GrantRoute::Ask
        );
        assert_eq!(
            grant_route(none, "main", &seen(0, &[], &[])),
            GrantRoute::Ask
        );
        assert_eq!(
            grant_route(&next, "next", &seen(0, &["lint"], &["ci"])),
            GrantRoute::Waived
        );
        assert_eq!(
            grant_route(&next, "main", &seen(1, &["lint"], &[])),
            GrantRoute::Wait
        );
    }
}
