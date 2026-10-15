//! Merging an item's pull request because the operator asked (ARCHITECTURE
//! §18.6). The `merge` knob and the ceiling are not involved: archond knows
//! who asked, records the request before any GitHub write, reads the pull
//! request again and merges only the reviewed commit on the branch the change
//! is for, never with `--admin` or `--auto`. Every outcome goes to the feed.
//!
//! `gh` runs without a lock; a request left `merging` by a stop or a lost
//! confirmation is settled from GitHub by the coordination pass.

use std::path::{Path, PathBuf};

use archon_core::autonomy::ReviewLevel;
use archon_core::config::RepoConfig;
use archon_core::error::ArchonError;
use archon_core::id::{MergeRequestId, PipelineRunId, QuestionId, QueueItemId};
use archon_core::merge::{
    self, ChecksSeen, Expected, MERGE_OPTIONS, MergeAnswer, MergeAuthority, MergeRequest,
    MergeState,
};
use archon_core::notify::{NotificationKind, Question};
use archon_core::queue::{QueueItem, QueueState};
use archon_core::repo::Repo;
use archon_core::run::{StageRun, StageState};
use archon_queue::Store;

use super::ship::{MergeRefusal, merge_method};
use super::{clip, coordination, finish, gh, recover};
use crate::context::DaemonContext;
use crate::triggers::{self, TriggerEvent};
use crate::worktree::stack::is_commit_id;

/// The orchestrator's reason is kept to this.
const MAX_REASON_CHARS: usize = 500;
/// A merge archond is still working on is never this old; one older was
/// interrupted, or its confirmation was lost.
const STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// A succeeded item's delivered pull request, as archond recorded it.
struct Shipped {
    item: QueueItem,
    run: PipelineRunId,
    repo: Repo,
    pr_url: String,
    /// The reviewed commit its delivery pushed.
    head: String,
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
    let Some((pr_url, head)) = delivered(&store.stage_runs(&run)?) else {
        return Err(ArchonError::Invalid(format!(
            "run {run} of {id} delivered no pull request: its repository ships `local`, or \
             its pipeline opened none"
        )));
    };
    assert!(is_commit_id(&head), "checked by delivered");
    Ok(Shipped {
        item,
        run,
        repo,
        pr_url,
        head,
    })
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

/// The branch the pull request must target; `Err` refuses the merge.
fn expected_base(store: &Store, shipped: &Shipped) -> Result<Result<String, String>, ArchonError> {
    let parents = store.stack_parents(&shipped.item.id)?;
    if let Some(open) = parents.iter().find(|p| !p.merged) {
        return Ok(Err(format!(
            "it is stacked on {}'s `{}`, which is not merged yet: merge that first",
            open.parent_item, open.parent_branch
        )));
    }
    if let Some(base) = super::restack::target_base(store, &shipped.item.id)? {
        return Ok(Ok(base));
    }
    let forked = match recover::load_run_vars(store, &shipped.run) {
        Ok(vars) => vars.get("base_branch").cloned(),
        Err(ArchonError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
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
            MergeAuthority::Cli | MergeAuthority::ArchonChat | MergeAuthority::AetherChat
        ),
        "only the operator's own command merges without a question"
    );
    let request = {
        let mut store = ctx.store.lock().await;
        let shipped = shipped(&store, item)?;
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

/// The orchestrator's `request_merge`: refused at once when the pull
/// request cannot merge; merged under `grant_merge` when it is reviewed and
/// green; otherwise the operator is asked to confirm.
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
        let store = ctx.store.lock().await;
        let shipped = shipped(&store, item)?;
        let base = expected_base(&store, &shipped)?;
        (shipped, base)
    };
    let base = base.map_err(|why| refused_early(&shipped, &why))?;
    let facts = gh::merge_facts(
        &ctx.config.github.binary,
        &shipped.repo.path,
        &shipped.pr_url,
    );
    let expected = Expected {
        head: &shipped.head,
        base: &base,
        ignore_checks: true,
    };
    let seen = merge::evaluate(&facts.await?, expected).map_err(|w| refused_early(&shipped, &w))?;
    let reviewed = shipped.item.autonomy.review != ReviewLevel::None;
    if shipped.repo.grant_merge && reviewed && seen.all_green() {
        let request = fresh(
            &shipped,
            MergeAuthority::Grant,
            false,
            reason,
            MergeState::Merging,
        );
        ctx.store.lock().await.insert_merge(&request)?;
        return execute(ctx, request).await;
    }
    ask(ctx, &shipped, reason, &seen).await
}

/// A request refused before anything was asked or written: the orchestrator
/// is told why, in the error.
fn refused_early(shipped: &Shipped, why: &str) -> ArchonError {
    assert!(!why.is_empty(), "a refusal gives its reason");
    ArchonError::InvalidTransition(format!(
        "{} of {} cannot be merged now: {why}",
        shipped.pr_url, shipped.item.id
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
    waiting.checks = Some(seen.describe());
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
        shipped.pr_url,
        shipped.item.id,
        shipped.item.title,
        merge::short(&shipped.head)
    );
    match reason {
        Some(reason) => text.push_str(&format!(": {reason}")),
        None => text.push('.'),
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
            ctx.store.lock().await.update_merge(&merge)?;
            report(ctx, &merge).await
        }
        MergeAnswer::Yes { ignore_checks } => {
            merge.ignore_checks = ignore_checks;
            merge.state = MergeState::Merging;
            ctx.store.lock().await.update_merge(&merge)?;
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
    let ready = match ready(ctx, &merge).await? {
        Ok(ready) => ready,
        Err(why) => {
            merge.state = MergeState::Refused { reason: why };
            return finish_merge(ctx, merge).await;
        }
    };
    merge.checks = Some(ready.seen.describe());
    let binary = &ctx.config.github.binary;
    let method = match merge_method(binary, &ready.path).await {
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
    let sent = gh::run(binary, &ready.path, &args, None).await;
    match confirm(binary, &ready.path, &merge, method, sent).await {
        Some(state) => {
            merge.state = state;
            finish_merge(ctx, merge).await
        }
        // Left `merging`: the coordination pass settles it from GitHub.
        None => Ok(merge),
    }
}

/// A request whose pull request may be merged now.
struct Ready {
    path: PathBuf,
    seen: ChecksSeen,
}

/// Check again, just before merging, everything a merge needs.
async fn ready(
    ctx: &DaemonContext,
    merge: &MergeRequest,
) -> Result<Result<Ready, String>, ArchonError> {
    let (shipped, base) = {
        let store = ctx.store.lock().await;
        let shipped = match shipped(&store, &merge.item) {
            Ok(shipped) => shipped,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => return Ok(Err(error.to_string())),
        };
        let base = match expected_base(&store, &shipped) {
            Ok(base) => base,
            Err(error) if error.is_store() => return Err(error),
            Err(error) => Err(error.to_string()),
        };
        (shipped, base)
    };
    if (&shipped.run, &shipped.pr_url, &shipped.head) != (&merge.run, &merge.pr_url, &merge.head) {
        return Ok(Err(
            "the item was delivered again since it was asked for; ask again".into(),
        ));
    }
    let base = match base {
        Ok(base) => base,
        Err(why) => return Ok(Err(why)),
    };
    let binary = &ctx.config.github.binary;
    let facts = match gh::merge_facts(binary, &shipped.repo.path, &merge.pr_url).await {
        Ok(facts) => facts,
        Err(error) => return Ok(Err(format!("GitHub could not be read: {error}"))),
    };
    let expected = Expected {
        head: &merge.head,
        base: &base,
        ignore_checks: merge.ignore_checks,
    };
    Ok(merge::evaluate(&facts, expected).map(|seen| Ready {
        path: shipped.repo.path.clone(),
        seen,
    }))
}

/// How the merge ended, read back from GitHub; `None` when it cannot be
/// read, after a `gh pr merge` that may have merged.
async fn confirm(
    binary: &Path,
    repo: &Path,
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
        };
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
    assert!(!merge.state.is_open(), "only an ended request is reported");
    merge.updated_at = archon_core::now();
    ctx.store.lock().await.update_merge(&merge)?;
    report(ctx, &merge).await?;
    Ok(merge)
}

/// The audit note in the feed, and `merge_finished` news for the orchestrator.
async fn report(ctx: &DaemonContext, merge: &MergeRequest) -> Result<(), ArchonError> {
    let who = merge.authority.describe();
    let (kind, text) = match &merge.state {
        MergeState::Merged { method } => {
            let checks = match (merge.ignore_checks, &merge.checks) {
                (true, Some(checks)) => {
                    format!("; the operator acknowledged the checks ({checks})")
                }
                (_, Some(checks)) => format!("; checks: {checks}"),
                (_, None) => String::new(),
            };
            let text = format!(
                "Merged {} of {} at {} ({method}) on {who}{checks}.",
                merge.pr_url,
                merge.item,
                merge::short(&merge.head)
            );
            (NotificationKind::Outcome, text)
        }
        MergeState::Declined => {
            let text = format!(
                "Not merged: the operator declined {} of {}.",
                merge.pr_url, merge.item
            );
            (NotificationKind::Outcome, text)
        }
        MergeState::Refused { reason } => {
            let text = format!(
                "Not merged: {} of {}, on {who}: {reason}",
                merge.pr_url, merge.item
            );
            (NotificationKind::Blocker, text)
        }
        MergeState::AwaitingOperator | MergeState::Merging => unreachable!("an open request"),
    };
    finish::notify(ctx, kind, &text, Some(&merge.item), Some(&merge.run)).await?;
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

/// Settle each request left `merging` by a stop or a lost confirmation,
/// from what GitHub shows now. Returns how many ended.
pub(crate) async fn settle_stale(ctx: &DaemonContext) -> Result<usize, ArchonError> {
    let cutoff = archon_core::now() - STALE_AFTER;
    let open = ctx.store.lock().await.open_merges()?;
    let mut ended = 0;
    for merge in open {
        let stale = merge.updated_at < ctx.started_at || merge.updated_at < cutoff;
        if merge.state != MergeState::Merging || !stale {
            continue;
        }
        let (subject, item) = (format!("merge:{}", merge.id), merge.item.clone());
        let result = settle_one(ctx, merge).await;
        ended += usize::from(result.is_ok());
        let what = format!("Settling the interrupted merge of {item}");
        coordination::isolate(ctx, &subject, Some(&item), &what, result).await?;
    }
    Ok(ended)
}

async fn settle_one(ctx: &DaemonContext, mut merge: MergeRequest) -> Result<(), ArchonError> {
    let repo = ctx
        .store
        .lock()
        .await
        .repo(&crate::resolve::repo_ref(&merge.repo))?;
    let binary = &ctx.config.github.binary;
    let after = gh::merge_facts(binary, &repo.path, &merge.pr_url).await?;
    merge.state = match (after.state.as_str(), after.head == merge.head) {
        ("MERGED", true) => MergeState::Merged {
            method: "confirmed after archond lost track of it".into(),
        },
        (state, _) => MergeState::Refused {
            reason: format!(
                "archond was interrupted while merging, and GitHub lists the pull request as \
                 {} at {}; ask again",
                state.to_lowercase(),
                merge::short(&after.head)
            ),
        },
    };
    finish_merge(ctx, merge).await.map(drop)
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
        }
    }

    #[test]
    fn only_github_showing_the_reviewed_commit_merged_counts_as_merged() {
        let merge = request();
        let merged = settled(&after("MERGED", &merge.head), &merge, "squash", None);
        assert_eq!(
            merged,
            MergeState::Merged {
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
        let MergeState::Refused { reason } = queued else {
            panic!("an open pull request is not merged");
        };
        assert!(reason.contains("may still merge"), "{reason}");
        let said = Some("Head branch was modified".to_owned());
        let MergeState::Refused { reason } =
            settled(&after("OPEN", &merge.head), &merge, "squash", said)
        else {
            panic!("a refused merge is refused");
        };
        assert!(reason.contains("Head branch was modified"), "{reason}");
    }
}
