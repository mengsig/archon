//! One fix item per red integration head: it forks from the integration
//! branch, names the merge that broke it and carries the failing output.

use archon_core::error::ArchonError;
use archon_core::integration::short;
use archon_core::model::ModelPlan;
use archon_core::notify::NotificationKind;
use archon_core::protocol::{EnqueueRequest, ItemQuery, RepoRef};
use archon_core::queue::{Priority, QueueItem};
use archon_core::repo::Repo;

use super::state::{BranchState, Culprit, Fix};
use crate::context::DaemonContext;
use crate::feed;

/// The tag every integration fix item carries.
pub const FIX_TAG: &str = "integration-fix";
/// The item states a fix is still open in.
const OPEN_STATES: [&str; 4] = ["queued", "dispatching", "running", "blocked"];
/// Most characters of an item title.
const MAX_TITLE_CHARS: usize = 200;

/// What a red verification found.
#[derive(Debug, Clone)]
pub(crate) struct Red {
    pub head: String,
    /// `checks.full` or `GitHub checks`.
    pub via: String,
    /// The end of the failing output, or the failing checks' names.
    pub excerpt: String,
    pub culprit: Option<Culprit>,
    /// `<short> <subject>` of each merge since the last green commit,
    /// oldest first, for a fix the bisection could not narrow.
    pub merges: Vec<String>,
}

/// Queue the fix item for `red`, unless one is still open or one was
/// already queued for this head; the operator is told of a queued one.
pub(crate) async fn repair(
    ctx: &DaemonContext,
    repo: &Repo,
    state: &mut BranchState,
    red: &Red,
) -> Result<Option<QueueItem>, ArchonError> {
    assert_eq!(
        state.head.as_ref(),
        Some(&red.head),
        "the red head was read"
    );
    if !needs_fix(ctx, repo, state, &red.head).await? {
        return Ok(None);
    }
    let request = request(repo, state, red);
    let item = crate::handlers::enqueue_for_archond(ctx, request).await?;
    assert_eq!(
        item.repo.as_ref(),
        Some(&repo.id),
        "queued in the repository"
    );
    state.fix = Some(Fix {
        item: item.id.clone(),
        head: red.head.clone(),
    });
    let broke = red
        .culprit
        .as_ref()
        .map_or("the bisection named no single merge".to_owned(), |c| {
            format!("broken by {}", c.line())
        });
    let text = format!(
        "The integration branch `{}` of `{}` is red at {} ({}): {broke}. Queued fix item {}.",
        state.branch,
        repo.name,
        short(&red.head),
        red.via,
        item.id
    );
    feed::notify(
        ctx,
        NotificationKind::Blocker,
        text,
        Some(item.id.clone()),
        None,
    )
    .await?;
    Ok(Some(item))
}

/// No fix item is queued while the last one is open, nor twice for one head.
async fn needs_fix(
    ctx: &DaemonContext,
    repo: &Repo,
    state: &BranchState,
    head: &str,
) -> Result<bool, ArchonError> {
    if state.fix.as_ref().is_some_and(|fix| fix.head == head) {
        return Ok(false);
    }
    // The store, not `state.fix`, is asked: a restart between the enqueue and
    // the state's save loses the reference but not the open item.
    let query = ItemQuery {
        states: OPEN_STATES.map(String::from).to_vec(),
        repo: Some(RepoRef(repo.id.as_str().to_owned())),
        tag: Some(FIX_TAG.to_owned()),
        limit: Some(1),
        ..ItemQuery::default()
    };
    let open = ctx.store.lock().await.items(&query)?;
    assert!(open.len() <= 1, "the query is limited to one");
    assert!(
        open.iter().all(|item| item.repo.as_ref() == Some(&repo.id)),
        "only this repository's fix items"
    );
    Ok(open.is_empty())
}

/// The fix item: in the repository's default pipeline, forked from the
/// integration branch, and keyed by the red head so a retry adds none.
fn request(repo: &Repo, state: &BranchState, red: &Red) -> EnqueueRequest {
    let what = red.culprit.as_ref().map_or_else(
        || format!("red at {}", short(&red.head)),
        |c| format!("broken by {}", c.line()),
    );
    let title: String = format!("fix(integration): repair `{}` {what}", state.branch)
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    EnqueueRequest {
        repo: Some(RepoRef(repo.name.clone())),
        pipeline: None,
        title,
        brief: brief(repo, state, red),
        priority: Priority::High,
        depends_on: Vec::new(),
        tags: vec![FIX_TAG.to_owned()],
        model_plan: ModelPlan::default().into(),
        model_reasons: None,
        budget_usd: None,
        stage_timeouts: Default::default(),
        idempotency_key: Some(format!("{FIX_TAG}-{}-{}", repo.id, red.head)),
        autonomy: Default::default(),
        origin: None,
    }
}

fn brief(repo: &Repo, state: &BranchState, red: &Red) -> String {
    let green = state
        .last_green
        .as_deref()
        .map_or("none recorded".to_owned(), |c| c.to_owned());
    let culprit = match &red.culprit {
        Some(culprit) => format!(
            "Bisecting the merges since the last green commit found the first failing one: \
             {} {} (`git show {}`).",
            culprit.commit, culprit.subject, culprit.commit
        ),
        None => format!(
            "The bisection could not name a single failing merge. The merges since the last \
             green commit, oldest first:\n{}",
            red.merges.join("\n")
        ),
    };
    format!(
        "The integration branch `{branch}` of `{name}` fails its full verification ({via}) at \
         {head}. Last green commit: {green}; {merges} merge(s) since.\n\n{culprit}\n\n\
         The end of the failing output:\n\n```text\n{excerpt}\n```\n\n\
         Make `{branch}` pass its full verification again with the smallest change: fix the \
         breakage forward, or revert the failing merge when the fix is unclear. Keep every other \
         merge. This item forks from `{branch}` and its pull request targets it.",
        branch = state.branch,
        name = repo.name,
        via = red.via,
        head = red.head,
        merges = state.merges_since_green,
        excerpt = red.excerpt.trim_end(),
    )
}
