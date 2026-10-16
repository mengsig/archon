//! Promoting a repository's integration branch into its release branch
//! (issue #82): the one pull request between them, merged with a merge
//! commit only while the full check is green on its exact head, then the
//! integration branch fast-forwarded to the release branch and the release
//! hook run. The operator's `archon promote` merges; an agent's call merges
//! only under the repository's `grant_merge`, else it stops at the pull
//! request and asks the operator.

use std::path::PathBuf;
use std::time::Duration;

use archon_core::error::ArchonError;
use archon_core::integration::{
    IntegrationHealth, Promotion, PromotionOutcome, ReleaseHook, short,
};
use archon_core::merge::{self, ChecksSeen, Expected, PullRequestFacts};
use archon_core::notify::NotificationKind;
use archon_core::protocol::RepoRef;
use archon_core::repo::Repo;
use tokio::sync::Mutex;

use super::branch::{Checkout, Head};
use super::full::{self, Slot};
use super::state::{self, BranchState, Verdict};
use super::{Watch, blocking};
use crate::context::DaemonContext;
use crate::executor::gh;
use crate::executor::ship::remote;
use crate::feed;
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

mod pending;

use self::pending::Pending;
pub(crate) use self::pending::forget_unregistered;

/// One promotion at a time in this daemon.
static PROMOTING: Mutex<()> = Mutex::const_new(());
/// Reads of a pull request whose mergeability GitHub is still working out.
const MERGEABLE_READS: u32 = 4;
const MERGEABLE_PAUSE: Duration = Duration::from_secs(2);
/// Merges listed in the pull request's body.
const MAX_LISTED: usize = 50;

/// Who asked for the promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promoter {
    /// The operator's own `archon promote`: it merges.
    Operator,
    /// An agent's call: it merges only under the repository's `grant_merge`.
    Agent,
}

/// The integration and release branches' heads on `origin`, read afresh.
#[derive(Debug, Clone)]
struct Heads {
    branch: Head,
    release: Head,
}

/// Promote `reference`'s integration branch: refused unless its head is
/// green, and stopped at the pull request when `by` may not merge.
pub async fn promote(
    ctx: &DaemonContext,
    reference: &RepoRef,
    by: Promoter,
) -> Result<Promotion, ArchonError> {
    let _one = PROMOTING.lock().await;
    let repo = ctx.store.lock().await.repo(reference)?;
    let watch = promotable(ctx, &repo)?;
    let heads = {
        let watch = watch.clone();
        blocking(move || read_heads(&watch)).await?
    };
    if let Some(promotion) = resume(ctx, &watch, &heads).await? {
        return Ok(promotion);
    }
    let head = heads.branch.commit.clone();
    let saved = state::load(&*ctx.store.lock().await, &repo.id)?;
    if let Some(why) = not_green(&repo.name, saved.as_ref(), &watch.branch, &head) {
        return Err(ArchonError::InvalidTransition(format!(
            "promotion refused: {why}"
        )));
    }
    let mut promotion = promotion_of(&watch, &head, None);
    if contains(&watch, &heads.release.commit, &head).await? {
        let realign_problem = realign(&watch, &heads.release.commit).await?;
        promotion.outcome = PromotionOutcome::UpToDate { realign_problem };
        return Ok(promotion);
    }
    let url = open_pull_request(ctx, &watch, &heads).await?;
    promotion.pull_request = Some(url.clone());
    if by == Promoter::Agent && !repo.grant_merge {
        let text = promotion.describe();
        feed::notify(ctx, NotificationKind::Blocker, text, None, None).await?;
        return Ok(promotion);
    }
    let sent = Pending {
        branch: watch.branch.clone(),
        release: watch.release.clone(),
        head: head.clone(),
        pull_request: url.clone(),
    };
    pending::save(&mut *ctx.store.lock().await, &repo.id, &sent)?;
    merge_pull_request(ctx, &watch, &url, &head).await?;
    finish(ctx, &watch, promotion).await
}

fn promotion_of(watch: &Watch, head: &str, pull_request: Option<String>) -> Promotion {
    Promotion {
        repo: watch.repo.name.clone(),
        branch: watch.branch.clone(),
        release: watch.release.clone(),
        head: head.to_owned(),
        pull_request,
        outcome: PromotionOutcome::AwaitingOperator,
    }
}

/// Finish a promotion whose merge was sent earlier and has since landed, as
/// through a merge queue; `None` when none waits, it has not landed yet, or
/// it was between other branches than the ones configured now.
async fn resume(
    ctx: &DaemonContext,
    watch: &Watch,
    heads: &Heads,
) -> Result<Option<Promotion>, ArchonError> {
    let Some(sent) = pending::load(&*ctx.store.lock().await, &watch.repo.id)? else {
        return Ok(None);
    };
    if (sent.branch.as_str(), sent.release.as_str()) != (&watch.branch, &watch.release) {
        return Ok(None);
    }
    if !contains(watch, &heads.release.commit, &sent.head).await? {
        return Ok(None);
    }
    let promotion = promotion_of(watch, &sent.head, Some(sent.pull_request));
    finish(ctx, watch, promotion).await.map(Some)
}

/// The post-merge work, then the pending promotion forgotten: a failure
/// before the release hook ended leaves it for the next `promote`.
async fn finish(
    ctx: &DaemonContext,
    watch: &Watch,
    mut promotion: Promotion,
) -> Result<Promotion, ArchonError> {
    assert!(promotion.pull_request.is_some(), "a merged pull request");
    promotion.outcome = after_merge(ctx, watch, &promotion.head).await?;
    pending::clear(&mut *ctx.store.lock().await, &watch.repo.id)?;
    let text = promotion.describe();
    feed::notify(ctx, NotificationKind::Outcome, text, None, None).await?;
    Ok(promotion)
}

/// The repository's integration branch, when it can be promoted: it has
/// one, and its release branch is reached through a pull request.
fn promotable(ctx: &DaemonContext, repo: &Repo) -> Result<Watch, ArchonError> {
    let Some(watch) = Watch::of(ctx, repo)? else {
        return Err(ArchonError::Invalid(format!(
            "`{}` has no integration branch to promote; set `integration_branch` first",
            repo.name
        )));
    };
    if !watch.source.remote || stack::origin_url(&repo.path)?.is_none() {
        return Err(ArchonError::Invalid(format!(
            "promotion opens a pull request on `origin`: `{}` must ship in `pr` mode and have \
             an `origin`",
            repo.name
        )));
    }
    assert_ne!(watch.branch, watch.release, "validated: two branches");
    Ok(watch)
}

fn read_heads(watch: &Watch) -> Result<Heads, ArchonError> {
    let missing = |branch: &str| {
        ArchonError::Invalid(format!("`origin` has no branch `{branch}` to promote"))
    };
    let branch = watch
        .source
        .head(&watch.branch)?
        .ok_or_else(|| missing(&watch.branch))?;
    let release = watch
        .source
        .head(&watch.release)?
        .ok_or_else(|| missing(&watch.release))?;
    assert!(
        branch.source_ref.ends_with(&watch.branch),
        "the branch asked for"
    );
    Ok(Heads { branch, release })
}

/// Why `head`, the integration branch's head now, may not be promoted, as
/// its saved state shows it; `None` when the full check is green on it.
fn not_green(repo: &str, saved: Option<&BranchState>, branch: &str, head: &str) -> Option<String> {
    let Some(state) = saved.filter(|state| state.branch == branch) else {
        return Some(format!(
            "`{branch}` of `{repo}` has not been verified yet; promote once it is green"
        ));
    };
    let shown = state.status(repo).describe();
    if state.head.as_deref() != Some(head) {
        return Some(format!(
            "`{branch}` moved to {} since it was last verified; promote once that head is \
             green ({shown})",
            short(head)
        ));
    }
    match state.health() {
        IntegrationHealth::Green => None,
        IntegrationHealth::Red => Some(format!("`{branch}` is red: {shown}")),
        IntegrationHealth::Verifying => Some(format!("`{branch}` is verifying: {shown}")),
        IntegrationHealth::Pending | IntegrationHealth::Unknown => Some(format!(
            "the full check has not passed on `{branch}`'s head yet: {shown}"
        )),
    }
}

/// Whether `commit` is `head` or one of its ancestors.
async fn contains(watch: &Watch, head: &str, commit: &str) -> Result<bool, ArchonError> {
    let (source, head, commit) = (watch.source.clone(), head.to_owned(), commit.to_owned());
    blocking(move || source.contains(&head, &commit)).await
}

/// The open pull request from the integration branch, moved onto the
/// release branch when it targets another, else opened.
async fn open_pull_request(
    ctx: &DaemonContext,
    watch: &Watch,
    heads: &Heads,
) -> Result<String, ArchonError> {
    let body = {
        let (watch, heads) = (watch.clone(), heads.clone());
        blocking(move || body(&watch, &heads)).await?
    };
    let title = format!("Promote `{}` into `{}`", watch.branch, watch.release);
    let request = remote::Request {
        branch: &watch.branch,
        base: &watch.release,
        title: &title,
        body: &body,
        draft: false,
    };
    let binary = &ctx.config.github.binary;
    match remote::pull_request(binary, &watch.repo.path, request).await? {
        Ok(url) => Ok(url),
        Err(refusal) => Err(ArchonError::Unavailable(refusal)),
    }
}

/// The pull request's body: what it promotes, and the merges it brings.
fn body(watch: &Watch, heads: &Heads) -> Result<String, ArchonError> {
    let (head, release) = (&heads.branch.commit, &heads.release.commit);
    let args = ["merge-base", release.as_str(), head.as_str()];
    let base = crate::worktree::git(&watch.source.repo, &args)?;
    let merges = watch.source.merges(&base, head)?;
    let mut text = format!(
        "Promotes `{}` at {head} into `{}`. The full check passed on this exact head.\n\n\
         {} merge(s) since `{}`:\n",
        watch.branch,
        watch.release,
        merges.len(),
        watch.release
    );
    for commit in merges.iter().rev().take(MAX_LISTED) {
        let subject = watch.source.subject(commit)?;
        text.push_str(&format!("- {} {subject}\n", short(commit)));
    }
    if merges.len() > MAX_LISTED {
        text.push_str(&format!("- … and {} older\n", merges.len() - MAX_LISTED));
    }
    assert!(!text.trim().is_empty(), "a pull request says something");
    Ok(text)
}

/// Merge the pull request at `url` with a merge commit, only while it shows
/// `head`: the integration branch can then fast-forward to the release
/// branch. A refusal says why, and nothing was merged.
async fn merge_pull_request(
    ctx: &DaemonContext,
    watch: &Watch,
    url: &str,
    head: &str,
) -> Result<(), ArchonError> {
    assert!(stack::is_commit_id(head), "a verified commit");
    let (binary, repo) = (&ctx.config.github.binary, &watch.repo.path);
    let facts = mergeable_facts(binary, repo, url).await?;
    if let Err(why) = may_merge(&facts, head, &watch.release) {
        return Err(ArchonError::InvalidTransition(format!(
            "promotion refused: {url} cannot merge: {why}"
        )));
    }
    let args = ["pr", "merge", url, "--match-head-commit", head, "--merge"];
    let sent = gh::run(binary, repo, &args, None).await?;
    let after = gh::merge_facts(binary, repo, url).await?;
    if after.state == "MERGED" && after.head == head {
        return Ok(());
    }
    let said = match sent.ok {
        true => format!(
            "GitHub accepted the merge but lists the pull request as {}; a merge queue or a \
             branch rule holds it, so promote again once it merged",
            after.state.to_lowercase()
        ),
        false => format!("GitHub refused the merge: {}", sent.detail()),
    };
    Err(ArchonError::Unavailable(said))
}

/// The pull request's facts, read again while GitHub is still working out
/// whether it merges cleanly, as it does just after it was opened.
async fn mergeable_facts(
    binary: &std::path::Path,
    repo: &std::path::Path,
    url: &str,
) -> Result<PullRequestFacts, ArchonError> {
    let mut facts = gh::merge_facts(binary, repo, url).await?;
    for _ in 1..MERGEABLE_READS {
        if facts.mergeable != "UNKNOWN" && !facts.mergeable.is_empty() {
            break;
        }
        tokio::time::sleep(MERGEABLE_PAUSE).await;
        facts = gh::merge_facts(binary, repo, url).await?;
    }
    Ok(facts)
}

/// Whether the pull request may merge: open at `head` onto `release`,
/// mergeable, and no check of its own failing or still running. It may
/// have none: the full check already passed on that head.
fn may_merge(facts: &PullRequestFacts, head: &str, release: &str) -> Result<(), String> {
    if facts.head != head {
        return Err(format!(
            "it shows {}, not the verified head {}; promote again once GitHub caught up",
            short(&facts.head),
            short(head)
        ));
    }
    let expected = Expected {
        head,
        base: release,
        ignore_checks: true,
    };
    let seen: ChecksSeen = merge::evaluate(facts, expected)?;
    if !seen.failed.is_empty() || !seen.pending.is_empty() {
        return Err(format!(
            "its checks are not all green ({})",
            seen.describe()
        ));
    }
    Ok(())
}

/// Once merged: the release branch's new head, the integration branch
/// fast-forwarded to it and the release hook run.
async fn after_merge(
    ctx: &DaemonContext,
    watch: &Watch,
    head: &str,
) -> Result<PromotionOutcome, ArchonError> {
    let release = {
        let watch = watch.clone();
        blocking(move || read_heads(&watch).map(|heads| heads.release)).await?
    };
    if !contains(watch, &release.commit, head).await? {
        return Err(ArchonError::Unavailable(format!(
            "GitHub merged the promotion, but `origin`'s `{}` at {} does not hold {}",
            watch.release,
            short(&release.commit),
            short(head)
        )));
    }
    let realign_problem = realign(watch, &release.commit).await?;
    if realign_problem.is_none() {
        carry_green(ctx, watch, head, &release.commit).await?;
    }
    let hook = match watch.release_hook.clone() {
        Some(command) => Some(run_hook(ctx, watch, &release, command).await),
        None => None,
    };
    Ok(PromotionOutcome::Merged {
        release_head: release.commit,
        realign_problem,
        hook,
    })
}

/// Fast-forward the integration branch on `origin` to `release`, never
/// forced; `Some` says why it was not, as when it moved on meanwhile.
async fn realign(watch: &Watch, release: &str) -> Result<Option<String>, ArchonError> {
    assert!(stack::is_commit_id(release), "a release commit");
    let (source, branch, release) = (
        watch.source.clone(),
        watch.branch.clone(),
        release.to_owned(),
    );
    blocking(move || {
        let url = stack::origin_url(&source.repo)?.expect("checked: promotion has an origin");
        let shadow = Shadow::of(&source.data_dir, &source.repo)?;
        let pushed = stack::push(&shadow, &url, &branch, &release)?;
        if !pushed.status.success() {
            return Ok(Some(format!(
                "`origin` refused the fast-forward to {}: {}",
                short(&release),
                pushed.stderr_text()
            )));
        }
        let Some(now) = source.head(&branch)?.map(|head| head.commit) else {
            return Ok(Some(format!("`{branch}` is gone from `origin`")));
        };
        if now == release || source.contains(&now, &release)? {
            return Ok(None);
        }
        Ok(Some(format!(
            "`{branch}` is at {} on `origin`, which does not hold {}",
            short(&now),
            short(&release)
        )))
    })
    .await
}

/// The merge commit holds the verified head's tree, so it is green as well:
/// record it, so the realigned head is not verified a second time.
async fn carry_green(
    ctx: &DaemonContext,
    watch: &Watch,
    head: &str,
    release: &str,
) -> Result<(), ArchonError> {
    let same_tree = {
        let (repo, head, release) = (
            watch.source.repo.clone(),
            head.to_owned(),
            release.to_owned(),
        );
        blocking(move || {
            let tree = |commit: &str| {
                crate::worktree::git(&repo, &["rev-parse", &format!("{commit}^{{tree}}")])
            };
            Ok(tree(&head)? == tree(&release)?)
        })
        .await?
    };
    if !same_tree {
        return Ok(());
    }
    let mut store = ctx.store.lock().await;
    let Some(mut state) = state::load(&store, &watch.repo.id)? else {
        return Ok(());
    };
    let verified = state
        .verdict
        .clone()
        .filter(|v| v.green && v.commit == head);
    let Some(verified) = verified else {
        return Ok(());
    };
    if state.branch != watch.branch || state.verifying.is_some() {
        return Ok(());
    }
    state.observe(release, archon_core::now());
    state.verdict = Some(Verdict {
        commit: release.to_owned(),
        via: format!("{} on {}, the same tree", verified.via, short(head)),
        at: archon_core::now(),
        ..verified
    });
    state.last_green = Some(release.to_owned());
    state.merges_since_green = 0;
    assert_eq!(state.health(), IntegrationHealth::Green, "carried green");
    state::save(&mut store, &watch.repo.id, &state)
}

/// Run the release hook in the daemon's checkout at the merged `release`
/// head, in the host's build slot; how it ended, never an error.
async fn run_hook(
    ctx: &DaemonContext,
    watch: &Watch,
    release: &Head,
    command: String,
) -> ReleaseHook {
    let ran = hook_run(ctx, watch, release, &command).await;
    match ran {
        Ok(ran) => ReleaseHook {
            command,
            passed: ran.passed,
            excerpt: ran.excerpt,
        },
        Err(error) => {
            tracing::warn!(repo = %watch.repo.name, %error, "the release hook could not run");
            ReleaseHook {
                command,
                passed: false,
                excerpt: format!("it could not run: {error}"),
            }
        }
    }
}

async fn hook_run(
    ctx: &DaemonContext,
    watch: &Watch,
    release: &Head,
    command: &str,
) -> Result<full::FullRun, ArchonError> {
    let slot = Slot::acquire(&full::slot_lock(&ctx.config.integration)).await?;
    let (source, name, head) = (
        watch.source.clone(),
        watch.repo.name.clone(),
        release.clone(),
    );
    let (work, log) = blocking(move || {
        let checkout = Checkout::prepare(&source, &name, &head)?;
        let work = PathBuf::from(checkout.at(&head.commit)?);
        let log = checkout.log.with_file_name(format!("{name}.release.log"));
        Ok((work, log))
    })
    .await?;
    let limit = ctx.config.integration.full_timeout.duration();
    full::run(slot, "the release hook", command, &work, &log, limit).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn verdict(green: bool) -> Option<Verdict> {
        Some(Verdict {
            commit: HEAD.into(),
            green,
            at: archon_core::now(),
            via: "checks.full".into(),
            excerpt: String::new(),
        })
    }

    #[test]
    fn only_a_green_verdict_on_the_head_promotes() {
        assert!(
            not_green("r", None, "next", HEAD)
                .unwrap()
                .contains("not been verified")
        );
        let mut state = BranchState::new("next");
        state.observe(HEAD, archon_core::now());
        state.verdict = verdict(true);
        assert_eq!(not_green("r", Some(&state), "next", HEAD), None);
        let moved = not_green("r", Some(&state), "next", &"f".repeat(40)).unwrap();
        assert!(moved.contains("moved to ffffffff"), "{moved}");
        assert!(
            not_green("r", Some(&state), "staging", HEAD).is_some(),
            "another branch"
        );

        state.verifying = Some(HEAD.into());
        let verifying = not_green("r", Some(&state), "next", HEAD).unwrap();
        assert!(verifying.starts_with("`next` is verifying"), "{verifying}");
        state.verifying = None;
        state.verdict = verdict(false);
        let red = not_green("r", Some(&state), "next", HEAD).unwrap();
        assert!(
            red.starts_with("`next` is red: r/next: red at 01234567"),
            "{red}"
        );
    }

    fn facts(head: &str, checks: Vec<merge::Check>) -> PullRequestFacts {
        PullRequestFacts {
            state: "OPEN".into(),
            is_draft: false,
            head: head.into(),
            base: "main".into(),
            mergeable: "MERGEABLE".into(),
            checks,
            merged_by: None,
            auto_merge: false,
        }
    }

    #[test]
    fn a_pull_request_merges_at_the_verified_head_with_no_check_failing() {
        assert_eq!(may_merge(&facts(HEAD, Vec::new()), HEAD, "main"), Ok(()));
        let moved = may_merge(&facts(&"a".repeat(40), Vec::new()), HEAD, "main");
        assert!(moved.unwrap_err().contains("not the verified head"));
        let failing = merge::Check {
            name: "ci".into(),
            outcome: merge::CheckOutcome::Failed,
        };
        let red = may_merge(&facts(HEAD, vec![failing]), HEAD, "main").unwrap_err();
        assert!(red.contains("1 failed (ci)"), "{red}");
        let elsewhere = may_merge(&facts(HEAD, Vec::new()), HEAD, "trunk").unwrap_err();
        assert!(elsewhere.contains("targets `main`"), "{elsewhere}");
    }
}
