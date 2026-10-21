//! Aether's notification center (docs/guide.md, "Aether's notification
//! center"). Every open operator question is posted there keyed by its id,
//! with its options as actions; blockers and outcomes are posted without.
//! The operator's answer there takes the `/answer` path, and a question
//! closed anywhere else resolves its notification. Against an aetherd
//! without the center nothing is posted and nothing fails.
//!
//! The feed drives it through a persisted cursor, so what happened while
//! Aether was away is posted once it is back; each connect also reconciles
//! the questions with what Aether holds and applies answers given meanwhile.

mod answers;
pub(crate) mod blockers;
mod posts;

use std::time::Duration;

use archon_aether::notifications::{self, Notification};
use archon_aether::wire::EventQuery;
use archon_aether::{AetherClient, Subscription};
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::notify::{self, FeedEntry, FeedItem, Question};
use archon_queue::store::keys;
use tokio::sync::broadcast;

use crate::context::DaemonContext;

/// Feed items read per pass.
const FEED_PAGE: usize = 200;
/// How often a pass that stopped on a failure is retried without new feed.
const RETRY: Duration = Duration::from_secs(30);
/// A blocker or outcome older than this is no news: it is not posted late.
const STALE_NOTICE: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// Say once per daemon that Aether keeps no notification center.
pub(crate) fn warn_if_unsupported(client: &AetherClient) {
    if client.serves_notifications() {
        return;
    }
    let welcome = client.welcome();
    tracing::info!(
        aetherd = %welcome.daemon_version,
        "aetherd serves no notification center; questions and outcomes stay in Archon's chat"
    );
}

/// Mirror the feed into the notification center and apply answers given
/// there, until the connection drops. Only a store failure is an `Err`.
pub async fn follow(ctx: &DaemonContext, client: &AetherClient) -> Result<(), ArchonError> {
    follow_retrying(ctx, client, RETRY).await
}

/// [`follow`], retrying a failed pass every `retry` instead of every 30s.
pub async fn follow_retrying(
    ctx: &DaemonContext,
    client: &AetherClient,
    retry: Duration,
) -> Result<(), ArchonError> {
    assert!(!retry.is_zero(), "a retry period paces the retries");
    if !client.serves_notifications() {
        // Support may come with a later aetherd; it then starts from that day.
        return ctx
            .store
            .lock()
            .await
            .kv_delete(keys::CURSOR_NOTIFICATION_CENTER);
    }
    let mut feed = ctx.feed.subscribe();
    // Before the first reconcile: what is asked after its snapshot is fed.
    cursor(&mut *ctx.store.lock().await)?;
    while client.is_connected() {
        let mut answers = match client.subscribe(None, answer_query()).await {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::warn!(%error, "notification center: cannot follow answers");
                tokio::time::sleep(retry).await;
                continue;
            }
        };
        let mut reconciled = false;
        catch_up(ctx, client, &mut reconciled).await?;
        follow_pass(ctx, client, &mut feed, &mut answers, &mut reconciled, retry).await?;
    }
    Ok(())
}

/// Reconcile, until that succeeds once on this connection, then mirror the
/// feed; a failure of either is logged and left to the next retry.
async fn catch_up(
    ctx: &DaemonContext,
    client: &AetherClient,
    reconciled: &mut bool,
) -> Result<(), ArchonError> {
    if !*reconciled {
        *reconciled = logged(reconcile(ctx, client).await, "reconcile")?.is_some();
    }
    logged(mirror_feed(ctx, client).await, "mirror the feed").map(drop)
}

/// Every `custom` event, of which `notification.answered` is Archon's.
fn answer_query() -> EventQuery {
    EventQuery {
        types: vec!["custom".to_owned()],
        ..EventQuery::default()
    }
}

/// Answers as Aether records them and new feed items as the store does,
/// until the answer stream ends (a dropped connection or an overflow). A
/// failed reconcile waits for the retry tick, so its own notices cannot spin it.
async fn follow_pass(
    ctx: &DaemonContext,
    client: &AetherClient,
    feed: &mut broadcast::Receiver<FeedItem>,
    answers: &mut Subscription,
    reconciled: &mut bool,
    period: Duration,
) -> Result<(), ArchonError> {
    let mut retry = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            next = answers.next() => {
                let event = match next {
                    Ok(event) => event,
                    Err(closed) => {
                        tracing::debug!(%closed, "notification center: answer stream ended");
                        return Ok(());
                    }
                };
                if let Some(answered) = logged(notifications::answered(&event), "read an answer")?
                    .flatten()
                {
                    answers::apply(ctx, &answered).await?;
                }
            }
            // A lagged receiver only means the store holds more to read.
            _ = feed.recv() => {
                logged(mirror_feed(ctx, client).await, "mirror the feed")?;
            }
            _ = retry.tick() => catch_up(ctx, client, reconciled).await?,
        }
    }
}

/// `Ok(Some)` on success, `Ok(None)` after logging a failure the next pass
/// retries; a store failure stays an `Err`.
fn logged<T>(result: Result<T, ArchonError>, what: &str) -> Result<Option<T>, ArchonError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_store() => Err(error),
        Err(error) => {
            tracing::warn!(%error, "notification center: could not {what}");
            Ok(None)
        }
    }
}

/// Make Aether's questions match the store's. Every open question is posted
/// again, a no-op for one Aether holds unchanged, whose record carries any
/// answer given while Archon was not listening; then what closed is resolved.
async fn reconcile(ctx: &DaemonContext, client: &AetherClient) -> Result<(), ArchonError> {
    let open = ctx.store.lock().await.questions(true)?;
    assert!(
        open.iter().all(Question::is_open),
        "the store lists open questions"
    );
    for question in &open {
        let posted = post_question(ctx, client, question).await?;
        if let Some(answered) = posted.filter(|n| n.answer.is_some()) {
            answers::apply(ctx, &answered).await?;
        }
    }
    answers::forget_refusals(ctx, &open).await?;
    let held = client.archon_notifications(notifications::MAX_LIST).await?;
    let is_open = |key: &str| open.iter().any(|q| q.id.as_str() == key);
    let closed = held
        .iter()
        .filter(|n| n.is_open() && n.kind == "question")
        .filter_map(|n| n.key.as_deref())
        .filter(|key| !is_open(key));
    for key in closed {
        client.resolve_notification(key).await?;
    }
    Ok(())
}

/// The notification as Aether now holds it; `None` for a question Archon
/// does not post.
async fn post_question(
    ctx: &DaemonContext,
    client: &AetherClient,
    question: &Question,
) -> Result<Option<Notification>, ArchonError> {
    let Some(post) = posts::question(ctx, question).await? else {
        return Ok(None);
    };
    let posted = client.post_notification(post).await?;
    assert_eq!(
        posted.key.as_deref(),
        Some(question.id.as_str()),
        "posted under its id"
    );
    Ok(Some(posted))
}

/// Mirror every feed item after the cursor, a page at a time. The cursor
/// stops before an item Aether could not take (no reply, or a failure inside
/// Aether), so it is retried; an item Aether refused is logged and passed.
async fn mirror_feed(ctx: &DaemonContext, client: &AetherClient) -> Result<(), ArchonError> {
    loop {
        let (cursor, items) = {
            let mut store = ctx.store.lock().await;
            let cursor = cursor(&mut store)?;
            (cursor, store.feed_after(cursor, FEED_PAGE)?)
        };
        assert!(
            items.iter().all(|i| i.seq > cursor),
            "a page follows its cursor"
        );
        let Some(last) = items.last().map(|item| item.seq) else {
            return Ok(());
        };
        let mut reached = cursor;
        let mut stopped = None;
        for item in &items {
            match mirror_item(ctx, client, item).await {
                Ok(()) => {}
                Err(error) if error.is_store() => return Err(error),
                Err(error) if is_transient(&error) => {
                    stopped = Some(error);
                    break;
                }
                Err(error) => {
                    tracing::warn!(seq = item.seq, %error, "notification center: feed item not posted")
                }
            }
            reached = item.seq;
        }
        let raw = reached.to_string();
        ctx.store
            .lock()
            .await
            .kv_set(keys::CURSOR_NOTIFICATION_CENTER, &raw)?;
        if let Some(error) = stopped {
            return Err(error);
        }
        assert_eq!(
            reached, last,
            "a page mirrored whole moves the cursor to its end"
        );
    }
}

/// No reply, or a failure inside Aether: worth the same request again.
pub(super) fn is_transient(error: &ArchonError) -> bool {
    matches!(error, ArchonError::Unavailable(_)) || archon_aether::wire::is_internal(error)
}

/// The stored cursor, or the feed's head the first time: the questions
/// open by then are posted by `reconcile`, older news is not.
fn cursor(store: &mut archon_queue::Store) -> Result<u64, ArchonError> {
    let raw = store.kv_get(keys::CURSOR_NOTIFICATION_CENTER)?;
    if let Some(raw) = raw.filter(|raw| !raw.is_empty()) {
        return raw.parse().map_err(|_| {
            ArchonError::Corrupt(format!(
                "kv {} = {raw:?} is not a feed seq",
                keys::CURSOR_NOTIFICATION_CENTER
            ))
        });
    }
    let head = store.last_feed_seq()?;
    store.kv_set(keys::CURSOR_NOTIFICATION_CENTER, &head.to_string())?;
    Ok(head)
}

async fn mirror_item(
    ctx: &DaemonContext,
    client: &AetherClient,
    item: &FeedItem,
) -> Result<(), ArchonError> {
    assert!(item.seq > 0, "feed sequences start at 1");
    match &item.entry {
        FeedEntry::Question(asked) => {
            let question = ctx.store.lock().await.question(&asked.id)?;
            if !question.is_open() {
                return Ok(());
            }
            post_question(ctx, client, &question).await.map(drop)
        }
        FeedEntry::Answer { question, .. } => {
            client.resolve_notification(question.as_str()).await?;
            let item = ctx.store.lock().await.question(question)?.item;
            match item {
                Some(item) => blockers::resolve(ctx, client, &item).await,
                None => Ok(()),
            }
        }
        FeedEntry::Notification(notice) => mirror_notice(ctx, client, notice).await,
        _ => Ok(()),
    }
}

/// A blocker or outcome is posted; the news that an item moved on (a steer
/// or its outcome) first resolves the blockers posted for it earlier.
async fn mirror_notice(
    ctx: &DaemonContext,
    client: &AetherClient,
    notice: &notify::Notification,
) -> Result<(), ArchonError> {
    if moves_on(notice) {
        match &notice.item {
            Some(item) => blockers::resolve(ctx, client, item).await?,
            None if is_clearing_line(&notice.text) => blockers::resolve_all(ctx, client).await?,
            None => {}
        }
    }
    if archon_core::now() - notice.at >= STALE_NOTICE {
        return Ok(());
    }
    let Some(post) = posts::notice(ctx, notice).await? else {
        return Ok(());
    };
    client.post_notification(post).await?;
    if let Some(item) = &notice.item {
        blockers::remember(ctx, item, &notice.id).await?;
    }
    Ok(())
}

/// Whether `notice` says its item is no longer stuck on what blocked it: it
/// finished, or a steer (the orchestrator's answer to an escalation) changed it.
fn moves_on(notice: &notify::Notification) -> bool {
    match notice.kind {
        notify::NotificationKind::Outcome => true,
        notify::NotificationKind::Progress => {
            crate::executor::steer::is_history_line(&notice.text) || is_clearing_line(&notice.text)
        }
        _ => false,
    }
}

const CLEARING_PREFIX: &str = "Clearing the notifications of ";
const CLEARING_ALL: &str = "Clearing every open blocker and outcome in the notification center.";

/// The hidden feed line that asks for notifications to be resolved: those of
/// `item`, or every open blocker and outcome.
pub fn clearing_line(item: Option<&QueueItemId>) -> String {
    match item {
        Some(item) => format!("{CLEARING_PREFIX}{item}."),
        None => CLEARING_ALL.to_owned(),
    }
}

fn is_clearing_line(text: &str) -> bool {
    text == CLEARING_ALL || text.starts_with(CLEARING_PREFIX)
}
